//! Canonical projection from one settled Runtime Host step into the neutral
//! Session/Run application vocabulary.

use awaken_agent_contract::agent::run::RunState;
use awaken_session_contract::{RunError, StepOutcome};

use crate::host::CommittedStepReceipt;

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

pub(crate) fn settled_step(result: CommittedStepReceipt) -> Result<StepOutcome, RunError> {
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
        // Causes: C1 the committed Run state is symbolically Running, Awaiting,
        // or Ended; C2 pending presence is arbitrary. Effects: E1 Running maps
        // only to rejection; E2 Awaiting maps only to Awaiting; E3 Ended maps
        // only to Ended. Constraint/invariant: committed RunState is the sole
        // state-classification authority, so pending presence cannot interchange
        // terminal kinds. Decision rules: R1 Running=>E1; R2 Awaiting=>E2;
        // R3 Ended=>E3.
        let state = symbolic_state(kani::any());
        let plan = settled_step_plan(&state, kani::any());

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

    #[kani::proof]
    fn settled_step_projection_cannot_invent_pending_or_observations() {
        // Causes: C1 the committed Run state spans Running/Awaiting/Ended; C2 a
        // committed pending value is absent or present. Effects: E1 each state
        // selects its exact closed plan; E2 only Awaiting forwards precisely C2;
        // E3 Running and Ended cannot synthesize pending or observation fields.
        // Constraint/invariant: the projection reads only committed RunState and
        // pending presence; audit observations retain their separate committed
        // snapshot authority. Decision rules: R1 Running=>reject+E3; R2
        // Awaiting+C2=>Awaiting(forward=C2); R3 Ended=>Ended+E3.
        let state = symbolic_state(kani::any());
        let has_pending: bool = kani::any();
        let plan = settled_step_plan(&state, has_pending);

        match plan {
            SettledStepPlan::RejectRunning => assert!(matches!(state, RunState::Running)),
            SettledStepPlan::Awaiting { forward_pending } => {
                assert!(matches!(state, RunState::Awaiting));
                assert_eq!(forward_pending, has_pending);
            }
            SettledStepPlan::Ended => assert!(matches!(state, RunState::Ended(_))),
        }
    }
}
