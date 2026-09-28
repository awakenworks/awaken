// @formal entrypoint session.runtime_projection_distinguishes_pending_resolving_and_processed_tools_and_fails_closed
//! Why a Run's Thread, or the aggregate Session, paused or stopped.

use awaken_agent_contract::agent::awaiting::AwaitReason;
use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_session_application_contract::Pending;

/// The timeline's stop classification of one Run boundary.
///
/// Consumers: Thread and aggregate idle facts; the protocol adapter encodes it
/// as its wire stop reason, resolving each answerable call to its public event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopFact {
    EndTurn,
    RetriesExhausted,
    BudgetReached,
    /// Paused on these answerable Runtime calls; empty when no ticket is known.
    RequiresAction {
        calls: Vec<String>,
    },
}

/// Project a Run's sole committed disposition to the one stop classification
/// used by foreground and cold lifecycle reconstruction.
///
/// # Panics
///
/// A `Running` state is not a boundary; callers pass only Run boundaries.
#[must_use]
pub fn terminal_stop(
    state: &RunState,
    pending: Option<&Pending>,
    await_reason: Option<&AwaitReason>,
    aggregate_budget_reached: bool,
) -> StopFact {
    match state {
        RunState::Awaiting if await_reason == Some(&AwaitReason::BudgetReached) => {
            StopFact::BudgetReached
        }
        RunState::Awaiting => StopFact::RequiresAction {
            calls: pending
                .map(|pending| vec![pending.tool_use_id.clone()])
                .unwrap_or_default(),
        },
        RunState::Ended(EndCause::MaxSteps | EndCause::Error(_)) => StopFact::RetriesExhausted,
        RunState::Ended(EndCause::NaturalEnd) if aggregate_budget_reached => {
            StopFact::BudgetReached
        }
        RunState::Ended(_) => StopFact::EndTurn,
        RunState::Running => unreachable!("StepOutcome cannot contain Running"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::Failure;

    #[test]
    fn stop_classification_decision_table() {
        // Causes: the boundary state, its await reason, a known ticket and the
        // aggregate budget. Effects: one stop fact per row.
        let pending = Pending {
            tool_use_id: "call".into(),
            name: "read".into(),
            input: serde_json::json!({}),
            client_executed: false,
        };
        for (rule, state, reason, ticket, budget, expected) in [
            (
                "S1 budget pause",
                RunState::Awaiting,
                Some(AwaitReason::BudgetReached),
                None,
                false,
                StopFact::BudgetReached,
            ),
            (
                "S2 answerable pause",
                RunState::Awaiting,
                None,
                Some(&pending),
                false,
                StopFact::RequiresAction {
                    calls: vec!["call".into()],
                },
            ),
            (
                "S3 pause without ticket",
                RunState::Awaiting,
                None,
                None,
                false,
                StopFact::RequiresAction { calls: vec![] },
            ),
            (
                "S4 step limit",
                RunState::Ended(EndCause::MaxSteps),
                None,
                None,
                false,
                StopFact::RetriesExhausted,
            ),
            (
                "S5 failure",
                RunState::Ended(EndCause::Error(Failure::StateConflict)),
                None,
                None,
                false,
                StopFact::RetriesExhausted,
            ),
            (
                "S6 natural end",
                RunState::Ended(EndCause::NaturalEnd),
                None,
                None,
                false,
                StopFact::EndTurn,
            ),
            (
                "S7 natural end over budget",
                RunState::Ended(EndCause::NaturalEnd),
                None,
                None,
                true,
                StopFact::BudgetReached,
            ),
            (
                "S8 cancellation",
                RunState::Ended(EndCause::Cancelled),
                None,
                None,
                true,
                StopFact::EndTurn,
            ),
        ] {
            assert_eq!(
                terminal_stop(&state, ticket, reason.as_ref(), budget),
                expected,
                "{rule}"
            );
        }
    }
}
