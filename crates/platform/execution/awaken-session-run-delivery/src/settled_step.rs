// @formal entrypoint runtime.settled_step_projection_is_closed_exact_and_non_widening
//! Canonical projection from one settled Session Run step into the neutral
//! Session/Run application vocabulary.

use awaken_agent_contract::agent::run::RunState;
use awaken_runtime_contract::runs::{RunError, StepOutcome};

use crate::CommittedStepReceipt;

/// Closed projection plan consumed by [`settled_step`]. The plan carries only
/// the authority-relevant choices: whether the boundary is open at all,
/// whether a pending value may be forwarded.
/// Payloads stay in the production types and are never mirrored for proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettledStepPlan {
    RejectRunning,
    Awaiting { forward_pending: bool },
    Ended,
}

#[must_use]
fn settled_step_plan(state: &RunState, has_pending: bool) -> SettledStepPlan {
    match state {
        RunState::Running => SettledStepPlan::RejectRunning,
        RunState::Awaiting => SettledStepPlan::Awaiting {
            forward_pending: has_pending,
        },
        RunState::Ended(_) => SettledStepPlan::Ended,
    }
}

pub fn settled_step(result: CommittedStepReceipt) -> Result<StepOutcome, RunError> {
    let delegated_runs = result.delegated_runs;
    let await_reason = result.await_reason;
    let run_id = result.run_id;
    let plan = settled_step_plan(&result.state, result.pending.is_some());
    match (result.state, plan) {
        (RunState::Awaiting, SettledStepPlan::Awaiting { forward_pending }) => {
            let outcome = StepOutcome::awaiting(
                result.new_messages,
                if forward_pending {
                    result.pending
                } else {
                    None
                },
            )
            .with_delegated_runs(delegated_runs)
            .with_run_id(run_id);
            Ok(match await_reason {
                Some(reason) => outcome.with_await_reason(reason),
                None => outcome,
            })
        }
        (RunState::Ended(cause), SettledStepPlan::Ended) => {
            Ok(StepOutcome::ended(result.new_messages, cause)
                .with_delegated_runs(delegated_runs)
                .with_run_id(run_id))
        }
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
    fn settled_step_plan_is_closed_and_copies_only_step_authority() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect decision table: Running -> reject; Awaiting -> preserve
        // exactly whether committed pending exists; Ended -> terminal. Audit
        // observations are recovered from the committed snapshot, never copied
        // through this process-local projection.
        assert_eq!(
            settled_step_plan(&RunState::Running, true),
            SettledStepPlan::RejectRunning
        );
        for has_pending in [false, true] {
            assert_eq!(
                settled_step_plan(&RunState::Awaiting, has_pending),
                SettledStepPlan::Awaiting {
                    forward_pending: has_pending,
                }
            );
            assert_eq!(
                settled_step_plan(&RunState::Ended(EndCause::NaturalEnd), has_pending),
                SettledStepPlan::Ended
            );
        }
    }
}

#[cfg(test)]
mod exhaustive {
    use super::*;
    use awaken_agent_contract::agent::run::EndCause;

    fn every_state() -> [RunState; 3] {
        [
            RunState::Running,
            RunState::Awaiting,
            RunState::Ended(EndCause::NaturalEnd),
        ]
    }

    /// @exhaustive RunState{Running,Awaiting,Ended(NaturalEnd)}(3) x bool(2) = 6
    /// @obligation runtime.settled_step_projection_is_closed_exact_and_non_widening
    #[test]
    fn unsettled_running_is_always_rejected_and_settled_kinds_never_interchange() {
        // R1 Running=>reject; R2 Awaiting=>Awaiting; R3 Ended=>Ended, whatever
        // the pending presence: it cannot interchange terminal kinds.
        for state in every_state() {
            for has_pending in [false, true] {
                let plan = settled_step_plan(&state, has_pending);
                assert_eq!(
                    matches!(plan, SettledStepPlan::RejectRunning),
                    matches!(state, RunState::Running)
                );
                assert_eq!(
                    matches!(plan, SettledStepPlan::Awaiting { .. }),
                    matches!(state, RunState::Awaiting)
                );
                assert_eq!(
                    matches!(plan, SettledStepPlan::Ended),
                    matches!(state, RunState::Ended(_))
                );
            }
        }
    }

    /// @exhaustive RunState{Running,Awaiting,Ended(NaturalEnd)}(3) x bool(2) = 6
    /// @obligation runtime.settled_step_projection_is_closed_exact_and_non_widening
    #[test]
    fn settled_step_projection_cannot_invent_pending_or_observations() {
        // R1 Running=>reject; R2 Awaiting+C2=>Awaiting(forward=C2); R3
        // Ended=>Ended. Only Awaiting forwards precisely the pending presence.
        for state in every_state() {
            for has_pending in [false, true] {
                match settled_step_plan(&state, has_pending) {
                    SettledStepPlan::RejectRunning => {
                        assert!(matches!(state, RunState::Running))
                    }
                    SettledStepPlan::Awaiting { forward_pending } => {
                        assert!(matches!(state, RunState::Awaiting));
                        assert_eq!(forward_pending, has_pending);
                    }
                    SettledStepPlan::Ended => assert!(matches!(state, RunState::Ended(_))),
                }
            }
        }
    }
}
