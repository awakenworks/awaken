// @formal surface obligation=session_run.execution_requires_dispatch_work_and_realization_owner
// @formal reason: Activation publishes exactly the reserved dispatch row once the Session committed its
// @formal reason: activity, and settled observation reads only the committed Run of that row. This classifies
// @formal reason: the activation and observation edge, not a proof claim.
//! Activating a reserved Session Run and observing it settle: the one
//! publication of a reservation row once the Session committed the Run's
//! activity, and the one committed-truth read that decides when a foreground
//! caller observing the Run is released.

use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_dispatch_contract::{SessionRunReservationActivation, SessionRunReservations as _};
use awaken_dispatch_erasure::AnyCoordinatorDispatch;
use awaken_dispatch_execution::DispatchPublication;
use awaken_session_execution_contract::{SessionRunActivation, SessionRunDelivery};
use awaken_session_types::RunError;

use crate::SessionCommit;

/// Activate the Session-approved reservation `delivery` names and publish the
/// row to the Workers that claim it.
///
/// Decision table (causes → effect):
/// | Rule | activity epoch | store outcome | effect |
/// |---|---|---|---|
/// | A1 | zero | - | refused; the store is not touched |
/// | A2 | nonzero | activated / already activated | that outcome; the row is published |
/// | A3 | nonzero | recovery claimed / completed | that outcome; nothing is published |
/// | A4 | nonzero | missing, rejected or conflicting | refused |
/// | A5 | nonzero | the store fails | retryable |
pub async fn activate_session_run_reservation(
    owner: &AnyCoordinatorDispatch,
    publication: &DispatchPublication,
    delivery: SessionRunDelivery,
) -> Result<SessionRunActivation, RunError> {
    if delivery.session_activity_epoch == 0 {
        return Err(RunError::bad_request(
            "Session User Run activity epoch must be nonzero",
        ));
    }
    let outcome = owner
        .activate_session_run_reservation(
            &delivery.run_id,
            &ThreadId(delivery.session_id.into_string()),
            delivery.session_activity_epoch,
        )
        .await
        .map_err(|error| RunError::unavailable(error.to_string()))?;
    let projected = session_run_activation(outcome)?;
    if matches!(
        projected,
        SessionRunActivation::Activated | SessionRunActivation::AlreadyActivated { .. }
    ) {
        publication
            .publish_staged(owner)
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?;
    }
    Ok(projected)
}

fn session_run_activation(
    outcome: SessionRunReservationActivation,
) -> Result<SessionRunActivation, RunError> {
    match outcome {
        SessionRunReservationActivation::Activated => Ok(SessionRunActivation::Activated),
        SessionRunReservationActivation::AlreadyActivated {
            session_activity_epoch,
        } => Ok(SessionRunActivation::AlreadyActivated {
            session_activity_epoch,
        }),
        SessionRunReservationActivation::RecoveryClaimed => {
            Ok(SessionRunActivation::RecoveryClaimed)
        }
        SessionRunReservationActivation::Completed => Ok(SessionRunActivation::Completed),
        SessionRunReservationActivation::MissingOrRejected => Err(RunError::bad_request(
            "Session User Run reservation is missing or rejected",
        )),
        SessionRunReservationActivation::Conflict => Err(RunError::bad_request(
            "Session User Run reservation activation conflicts with durable truth",
        )),
    }
}

/// One committed-truth read: `run_id`'s state once it has settled (`Ended`,
/// or `Awaiting`), else `None`. A caller observing the resume of an answered
/// ticket (`answered_correlation`) is released only by a newly committed
/// ticket: before the Worker consumes the answer, committed truth is still
/// `Awaiting` on the answered one.
pub async fn read_settled_phase(
    commit: &SessionCommit,
    thread_id: &ThreadId,
    run_id: &RunId,
    answered_correlation: Option<&str>,
) -> Result<Option<RunState>, RunError> {
    let record = commit
        .authoritative_run(run_id)
        .await
        .map_err(RunError::internal)?;
    match record {
        Some(
            record @ RunRecord {
                state: RunState::Ended(_),
                ..
            },
        ) => Ok(Some(record.state)),
        Some(
            record @ RunRecord {
                state: RunState::Awaiting,
                ..
            },
        ) => {
            let Some(answered_correlation) = answered_correlation else {
                return Ok(Some(record.state));
            };
            let current = commit
                .open_wait_for_thread(thread_id)
                .await
                .map_err(RunError::internal)?;
            Ok(awaiting_ticket_advanced(
                run_id,
                answered_correlation,
                current
                    .as_ref()
                    .map(|(current_run, ticket)| (current_run, ticket.correlation_id.as_str())),
            )
            .then_some(record.state))
        }
        _ => Ok(None),
    }
}

fn awaiting_ticket_advanced(
    observed_run: &RunId,
    answered_correlation: &str,
    current_wait: Option<(&RunId, &str)>,
) -> bool {
    current_wait.is_some_and(|(current_run, current_correlation)| {
        current_run == observed_run && current_correlation != answered_correlation
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_completion_waits_for_exact_ticket_advancement() {
        // Cause/effect graph: C1 initial durable submit vs resumed wait; C2 the
        // committed Run is still Awaiting; C3 open ticket is the answered ticket,
        // a new ticket, absent, or belongs to another Run. Effects: E1 old truth
        // cannot prematurely release a resume waiter; E2 a new ticket releases it;
        // E3 missing/inconsistent truth remains fail-closed. Ended and initial
        // Awaiting are handled by the enclosing read-settled state match.
        //
        // | Rule | observed Run | current ticket | Effect |
        // | R1 | run-1 | same correlation | E1 false |
        // | R2 | run-1 | new correlation | E2 true |
        // | R3 | run-1 | absent | E3 false |
        // | R4 | run-1 | ticket for run-2 | E3 false |
        let run_1 = RunId("run-1".into());
        let run_2 = RunId("run-2".into());
        assert!(
            !awaiting_ticket_advanced(&run_1, "ticket-1", Some((&run_1, "ticket-1"))),
            "R1"
        );
        assert!(
            awaiting_ticket_advanced(&run_1, "ticket-1", Some((&run_1, "ticket-2"))),
            "R2"
        );
        assert!(!awaiting_ticket_advanced(&run_1, "ticket-1", None), "R3");
        assert!(
            !awaiting_ticket_advanced(&run_1, "ticket-1", Some((&run_2, "ticket-2"))),
            "R4"
        );
    }

    #[test]
    fn reservation_activation_outcomes_follow_their_decision_table() {
        // A2/A3: the store's outcomes the application observes.
        let rows = [
            (
                SessionRunReservationActivation::Activated,
                SessionRunActivation::Activated,
            ),
            (
                SessionRunReservationActivation::AlreadyActivated {
                    session_activity_epoch: 3,
                },
                SessionRunActivation::AlreadyActivated {
                    session_activity_epoch: 3,
                },
            ),
            (
                SessionRunReservationActivation::RecoveryClaimed,
                SessionRunActivation::RecoveryClaimed,
            ),
            (
                SessionRunReservationActivation::Completed,
                SessionRunActivation::Completed,
            ),
        ];
        for (store, application) in rows {
            assert_eq!(session_run_activation(store).expect("A2/A3"), application);
        }
        // A4: a missing, rejected or conflicting reservation is refused.
        for refused in [
            SessionRunReservationActivation::MissingOrRejected,
            SessionRunReservationActivation::Conflict,
        ] {
            assert_eq!(
                session_run_activation(refused).expect_err("A4").code,
                "invalid_request"
            );
        }
    }
}
