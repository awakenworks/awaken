// @formal surface obligation=session_run.reservation_precedes_activity
// @formal reason: Prepared inputs use the existing reservation, root receipt,
// @formal reason: claim fence and completion owners. This is not a SQL refinement proof.
//! The one prepared-activation reservation and observation composition.

use std::sync::Arc;

use awaken_agent_contract::agent::run::RunState;
use awaken_dispatch_contract::{
    DispatchClaims as _, SessionReservationAuthority as _, SessionRunAdmissionObservation,
    SessionRunAdmissionReads as _, SessionRunReservationOutcome, SessionRunReservationResolution,
    SessionRunReservations as _, SettleOutcome,
};
use awaken_dispatch_erasure::AnyCoordinatorDispatch;
use awaken_dispatch_execution::{DispatchPublication, session_dispatch::DispatchPreparation};
use awaken_session_execution_contract::{
    AdmitPreparedSessionRun, AdmittedSessionRun, SessionRunActivation, SessionRunActivityAdmitter,
    SessionRunReservation,
};
use awaken_session_types::RunError;

use crate::session_run_activation::{activate_session_run_reservation, read_settled_phase};
use crate::{CompletionRegistry, SessionCommit, activate_and_await_session_run};

fn pending(message: impl Into<String>) -> RunError {
    RunError::unavailable_classified("session_run_admission_pending", message)
}

/// The existing owners borrowed by one prepared delivery, not another
/// coordinator, queue or lifetime binding.
pub struct PreparedSessionRunDelivery<'a> {
    pub owner: Arc<AnyCoordinatorDispatch>,
    pub publication: &'a DispatchPublication,
    pub completion: &'a Arc<CompletionRegistry>,
    pub commit: Arc<SessionCommit>,
    pub clock: &'a dyn awaken_time::WallClock,
}

/// All processes supply only their existing projection compiler and owners.
/// Registration is shared with ordinary Session Run observation and precedes
/// adoption resolution: an adopted row cannot become Pending before the
/// selected completion/streaming observer exists.
pub async fn run_prepared_session_run<'p>(
    command: AdmitPreparedSessionRun,
    prepare: impl Fn(awaken_runtime_contract::RunActivation) -> DispatchPreparation<'p>,
    delivery: PreparedSessionRunDelivery<'_>,
    activity: &dyn SessionRunActivityAdmitter,
) -> Result<RunState, RunError> {
    let PreparedSessionRunDelivery {
        owner,
        publication,
        completion,
        commit,
        clock,
    } = delivery;
    let fingerprint = command.command_fingerprint();
    let run_id = command.activation.run_id.clone();
    let thread = command.activation.thread_id.clone();
    let request = super::session_run_reservation::prepared_session_run_request(&command, &prepare)?;
    let original_activation = command.activation;
    activate_and_await_session_run(
        completion,
        &run_id,
        None,
        std::time::Duration::from_millis(250),
        || async {
            let reserved = super::session_run_reservation::reserve_session_run_request(
                request,
                owner.as_ref(),
            )
            .await?;
            let admitted = match reserved {
                SessionRunReservationOutcome::Conflict => {
                    // Compile the former production envelope only for its
                    // upgrade cut. Fresh prepared input has no legacy fallback.
                    let legacy = prepare(original_activation).extension()?;
                    let original = match owner
                        .observe_session_run_admission(&run_id)
                        .await
                        .map_err(|error| pending(error.to_string()))?
                    {
                        SessionRunAdmissionObservation::Live(original)
                            if original.same_original_dispatch(&legacy) =>
                        {
                            *original
                        }
                        _ => {
                            return Err(pending(
                                "prepared Run conflicts with original dispatch evidence",
                            ));
                        }
                    };
                    let claim = owner
                        .adopt_pending_session_run(
                            original,
                            fingerprint,
                            "session-prepared-admission",
                            super::SESSION_RUN_RESERVATION_TTL_MS,
                        )
                        .await
                        .map_err(|error| pending(error.to_string()))?
                        .ok_or_else(|| {
                            pending("legacy Run is not an adoptable unstarted Pending dispatch")
                        })?;
                    let guard = owner
                        .lock_session_run_reservation_epoch(&claim)
                        .await
                        .map_err(|error| pending(error.to_string()))?
                        .ok_or_else(|| pending("legacy Run adoption claim is stale"))?;
                    // The remote projection cannot certify historical absence.
                    // The canonical guarded composition owns the exact read,
                    // root CAS, expiry recheck and asynchronous release.
                    let recovery = if matches!(commit.as_ref(), SessionCommit::Local(_)) {
                        Ok(Some(commit.as_ref() as &dyn awaken_agent_contract::thread::read::recovery::RunRecoverySource))
                    } else {
                        Err(pending("legacy Run adoption requires original committed authority"))
                    };
                    let admission = guard
                        .admit_prepared_session_run(
                            clock,
                            recovery,
                            |cursor, cancelled| {
                                activity.admit(
                                    SessionRunReservation::AlreadyReserved,
                                    cancelled,
                                    Some(cursor),
                                )
                            },
                            pending,
                        )
                        .await
                        .map_err(pending)?;
                    let resolution = match &admission {
                        Ok(admitted) => SessionRunReservationResolution::Admitted {
                            session_activity_epoch: admitted
                                .delivery()
                                .ok_or_else(|| pending("adopted Run has no exact activity delivery"))?
                                .session_activity_epoch,
                        },
                        // This error is the retained canonical negative root
                        // receipt. Cancellation or an unavailable read alone
                        // cannot certify refusal or erase a possible receipt.
                        Err(error) if error.code == "budget_reached" => {
                            SessionRunReservationResolution::Rejected
                        }
                        Err(_) => SessionRunReservationResolution::Retry {
                            reservation_ttl_ms: super::SESSION_RUN_RESERVATION_TTL_MS,
                        },
                    };
                    let resolved = owner
                        .resolve_claimed_session_run_reservation(&claim, resolution)
                        .await;
                    if admission
                        .as_ref()
                        .is_err_and(|error| error.code == "budget_reached")
                    {
                        return Err(admission.expect_err("retained canonical refusal is an error"));
                    }
                    if resolved.map_err(|error| pending(error.to_string()))? == SettleOutcome::Fenced {
                        return Err(pending("legacy Run adoption resolution lost its claim; recover the exact receipt"));
                    }
                    admission?
                }
                outcome => {
                    activity
                        .admit(
                            super::session_run_reservation::session_run_reservation(outcome)?,
                            false,
                            None,
                        )
                        .await?
                }
            };
            match admitted {
                AdmittedSessionRun::Reserved(delivery)
                | AdmittedSessionRun::AlreadyReserved(delivery)
                | AdmittedSessionRun::AlreadyActivated(delivery) => {
                    // Resolution may already have bound the adopted epoch.
                    activate_session_run_reservation(&owner, publication, delivery).await
                }
                AdmittedSessionRun::RecoveryClaimed { .. } => {
                    Ok(SessionRunActivation::RecoveryClaimed)
                }
                AdmittedSessionRun::Completed { .. } => Ok(SessionRunActivation::Completed),
            }
        },
        || async {
            let state = read_settled_phase(&commit, &thread, &run_id, None).await?;
            if state.is_none() {
                // Same committed-truth wait, plus receipt-only refusal replay.
                // A peer may reject while this caller waits; no second loop or
                // completion fact is invented for a never-executed Run.
                let _ = activity
                    .admit(SessionRunReservation::RecoveryClaimed, true, None)
                    .await?;
            }
            Ok(state)
        },
    )
    .await
}
