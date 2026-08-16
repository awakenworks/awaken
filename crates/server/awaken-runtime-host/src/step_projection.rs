//! Canonical projection from one settled Runtime Host step into the neutral
//! Session/Run application vocabulary.

use awaken_agent_contract::agent::run::RunState;
use awaken_session_contract::{Pending, RunError, StepOutcome};

use crate::host::{CommittedStepReceipt, PendingTool};

/// Closed projection plan consumed by [`settled_step`]. The plan carries only
/// the authority-relevant choices: whether the boundary is open at all,
/// whether a pending value may be forwarded, and the two observation flags.
/// Payloads stay in the production types and are never mirrored for proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettledStepPlan {
    RejectRunning,
    Awaiting {
        forward_pending: bool,
        compacted: bool,
        rescheduled: bool,
    },
    Ended {
        compacted: bool,
        rescheduled: bool,
    },
}

#[must_use]
fn settled_step_plan(
    state: &RunState,
    has_pending: bool,
    compacted: bool,
    rescheduled: bool,
) -> SettledStepPlan {
    match state {
        RunState::Running => SettledStepPlan::RejectRunning,
        RunState::Awaiting => SettledStepPlan::Awaiting {
            forward_pending: has_pending,
            compacted,
            rescheduled,
        },
        RunState::Ended(_) => SettledStepPlan::Ended {
            compacted,
            rescheduled,
        },
    }
}

pub(crate) fn pending(pending: Option<PendingTool>) -> Option<Pending> {
    pending.map(|pending| Pending {
        tool_use_id: pending.tool_use_id,
        name: pending.name,
        input: pending.input,
        client_executed: pending.client_executed,
    })
}

pub(crate) fn settled_step(result: CommittedStepReceipt) -> Result<StepOutcome, RunError> {
    let delegated_runs = result.delegated_runs;
    let model_requests = result.model_requests;
    let rescheduled_delegated_run_ids = result.rescheduled_delegated_run_ids;
    let run_id = result.run_id;
    let plan = settled_step_plan(
        &result.state,
        result.pending.is_some(),
        result.compacted,
        result.rescheduled,
    );
    match (result.state, plan) {
        (
            RunState::Awaiting,
            SettledStepPlan::Awaiting {
                forward_pending,
                compacted,
                rescheduled,
            },
        ) => Ok(StepOutcome::awaiting(
            result.new_messages,
            forward_pending.then(|| pending(result.pending)).flatten(),
            compacted,
            rescheduled,
        )
        .with_delegated_runs(delegated_runs)
        .with_model_requests(model_requests)
        .with_rescheduled_delegated_runs(rescheduled_delegated_run_ids)
        .with_run_id(run_id)),
        (
            RunState::Ended(cause),
            SettledStepPlan::Ended {
                compacted,
                rescheduled,
            },
        ) => Ok(
            StepOutcome::ended(result.new_messages, cause, compacted, rescheduled)
                .with_delegated_runs(delegated_runs)
                .with_model_requests(model_requests)
                .with_rescheduled_delegated_runs(rescheduled_delegated_run_ids)
                .with_run_id(run_id),
        ),
        (RunState::Running, SettledStepPlan::RejectRunning) => Err(RunError::internal(
            "runtime returned an unsettled Running state at the application boundary",
        )),
        // The plan is derived immediately above from the same state. Retain a
        // fail-closed arm so a future change cannot turn a mismatch into an
        // accidentally widened successful outcome.
        _ => Err(RunError::internal(
            "runtime settled-step projection plan did not match committed state",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::EndCause;

    #[test]
    fn settled_step_plan_is_closed_and_copies_only_committed_signals() {
        for (compacted, rescheduled) in [(false, false), (false, true), (true, false), (true, true)]
        {
            assert_eq!(
                settled_step_plan(&RunState::Running, true, compacted, rescheduled),
                SettledStepPlan::RejectRunning
            );
            for has_pending in [false, true] {
                assert_eq!(
                    settled_step_plan(&RunState::Awaiting, has_pending, compacted, rescheduled,),
                    SettledStepPlan::Awaiting {
                        forward_pending: has_pending,
                        compacted,
                        rescheduled,
                    }
                );
                assert_eq!(
                    settled_step_plan(
                        &RunState::Ended(EndCause::NaturalEnd),
                        has_pending,
                        compacted,
                        rescheduled,
                    ),
                    SettledStepPlan::Ended {
                        compacted,
                        rescheduled,
                    }
                );
            }
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;
    use awaken_agent_contract::agent::run::EndCause;

    fn symbolic_state(tag: u8) -> RunState {
        match tag % 3 {
            0 => RunState::Running,
            1 => RunState::Awaiting,
            _ => RunState::Ended(EndCause::NaturalEnd),
        }
    }

    #[kani::proof]
    fn unsettled_running_is_always_rejected_and_settled_kinds_never_interchange() {
        let state = symbolic_state(kani::any());
        let plan = settled_step_plan(&state, kani::any(), kani::any(), kani::any());

        assert_eq!(
            matches!(plan, SettledStepPlan::RejectRunning),
            matches!(state, RunState::Running)
        );
        assert_eq!(
            matches!(plan, SettledStepPlan::Awaiting { .. }),
            matches!(state, RunState::Awaiting)
        );
        assert_eq!(
            matches!(plan, SettledStepPlan::Ended { .. }),
            matches!(state, RunState::Ended(_))
        );
    }

    #[kani::proof]
    fn settled_step_projection_cannot_invent_pending_or_observation_flags() {
        let state = symbolic_state(kani::any());
        let has_pending: bool = kani::any();
        let compacted: bool = kani::any();
        let rescheduled: bool = kani::any();
        let plan = settled_step_plan(&state, has_pending, compacted, rescheduled);

        match plan {
            SettledStepPlan::RejectRunning => assert!(matches!(state, RunState::Running)),
            SettledStepPlan::Awaiting {
                forward_pending,
                compacted: projected_compacted,
                rescheduled: projected_rescheduled,
            } => {
                assert!(matches!(state, RunState::Awaiting));
                assert_eq!(forward_pending, has_pending);
                assert_eq!(projected_compacted, compacted);
                assert_eq!(projected_rescheduled, rescheduled);
            }
            SettledStepPlan::Ended {
                compacted: projected_compacted,
                rescheduled: projected_rescheduled,
            } => {
                assert!(matches!(state, RunState::Ended(_)));
                assert_eq!(projected_compacted, compacted);
                assert_eq!(projected_rescheduled, rescheduled);
            }
        }
    }
}
