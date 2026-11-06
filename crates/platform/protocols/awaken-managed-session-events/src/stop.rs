// @formal entrypoint session.runtime_projection_distinguishes_pending_resolving_and_processed_tools_and_fails_closed
//! Why a Run's Thread, or the aggregate Session, paused or stopped.

use crate::{OrderIndex, ProjectionError, ToolResultSource};
use awaken_agent_contract::RunLifecycleEvent;
use awaken_agent_contract::agent::awaiting::AwaitReason;
use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_runtime_contract::runs::Pending;

/// The timeline's stop classification of one Run boundary.
///
/// Consumers: Thread and aggregate idle facts; the protocol adapter encodes it
/// as its wire stop reason, resolving each answerable call to its public event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopFact {
    EndTurn,
    RetriesExhausted,
    BudgetReached,
    /// Exact committed ToolUse sources of the pause; empty when no ticket is known.
    RequiresAction {
        calls: Vec<ToolResultSource>,
    },
}

/// Project a Run's sole committed disposition to the one stop classification
/// used by foreground and cold lifecycle reconstruction.
///
/// # Panics
///
/// A `Running` state is not a boundary; callers pass only Run boundaries.
pub fn terminal_stop(
    terminal: &RunLifecycleEvent,
    pending: Option<&Pending>,
    aggregate_budget_reached: bool,
    index: &OrderIndex<'_>,
) -> Result<StopFact, ProjectionError> {
    let await_reason = terminal.await_reason.as_ref();
    Ok(match &terminal.state {
        RunState::Awaiting if await_reason == Some(&AwaitReason::BudgetReached) => {
            StopFact::BudgetReached
        }
        RunState::Awaiting => StopFact::RequiresAction {
            calls: pending
                .map(|pending| {
                    index
                        .pending_tool_source(terminal, pending)
                        .map(|source| vec![source])
                })
                .transpose()?
                .unwrap_or_default(),
        },
        RunState::Ended(EndCause::MaxSteps | EndCause::Error(_)) => StopFact::RetriesExhausted,
        RunState::Ended(EndCause::NaturalEnd) if aggregate_budget_reached => {
            StopFact::BudgetReached
        }
        RunState::Ended(_) => StopFact::EndTurn,
        RunState::Running => unreachable!("StepOutcome cannot contain Running"),
    })
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
                    calls: vec![ToolResultSource {
                        source_id: "run-thread".into(),
                        call_id: "call".into(),
                        name: "read".into(),
                    }],
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
            let kind = match &state {
                RunState::Awaiting => awaken_agent_contract::RunLifecycleEventKind::Awaiting,
                RunState::Ended(EndCause::Cancelled) => {
                    awaken_agent_contract::RunLifecycleEventKind::Cancelled
                }
                _ => awaken_agent_contract::RunLifecycleEventKind::Completed,
            };
            let mut terminal = crate::lifecycle::tests::event(2, "thread", kind, state);
            terminal.await_reason = reason;
            let mut snapshot = crate::index::snapshot(Vec::new(), Vec::new());
            snapshot.claimed_run_id = terminal.run_id.clone();
            snapshot.latest_run_id = Some(terminal.run_id.clone());
            snapshot.runs[0].id = terminal.run_id.clone();
            snapshot.runs[0].state = terminal.state.clone();
            let lifecycle = [terminal.clone()];
            let historical = ticket
                .map(|ticket| (terminal.cursor, ticket.clone()))
                .into_iter()
                .collect();
            let children = std::collections::HashMap::new();
            let index = OrderIndex::new(crate::OrderEvidence {
                updates: None,
                session_id: "thread",
                batches: &[],
                closed_intervals: &[],
                budget_reach: &[],
                parent_terminal: Default::default(),
                lifecycle: &lifecycle,
                root: Some(&snapshot),
                children: &children,
                links: &[],
                outcomes: &[],
                historical_pending: &historical,
            });
            assert_eq!(
                terminal_stop(&terminal, ticket, budget, &index).unwrap(),
                expected,
                "{rule}"
            );
        }
    }
}
