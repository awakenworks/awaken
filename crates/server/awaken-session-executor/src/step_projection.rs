//! Canonical projection from one settled Runtime Host step into the neutral
//! Session/Run application vocabulary.

use awaken_agent_contract::agent::run::RunState;
use awaken_session_contract::{Pending, RunError, StepOutcome};

use crate::host::{PendingTool, RunResult};

pub(crate) fn pending(pending: Option<PendingTool>) -> Option<Pending> {
    pending.map(|pending| Pending {
        tool_use_id: pending.tool_use_id,
        name: pending.name,
        input: pending.input,
        client_executed: pending.client_executed,
    })
}

pub(crate) fn settled_step(result: RunResult) -> Result<StepOutcome, RunError> {
    let delegated_runs = result.delegated_runs;
    let run_id = result.run_id;
    match result.state {
        RunState::Awaiting => Ok(StepOutcome::awaiting(
            result.new_messages,
            pending(result.pending),
            result.compacted,
            result.rescheduled,
        )
        .with_delegated_runs(delegated_runs)
        .with_run_id(run_id)),
        RunState::Ended(cause) => Ok(StepOutcome::ended(
            result.new_messages,
            cause,
            result.compacted,
            result.rescheduled,
        )
        .with_delegated_runs(delegated_runs)
        .with_run_id(run_id)),
        RunState::Running => Err(RunError::internal(
            "runtime returned an unsettled Running state at the application boundary",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::{EndCause, Id as RunId};

    fn result(state: RunState, pending: Option<PendingTool>) -> RunResult {
        RunResult {
            run_id: RunId("run-projection".into()),
            new_messages: Vec::new(),
            state,
            pending,
            compacted: false,
            rescheduled: false,
            delegated_runs: Vec::new(),
        }
    }

    #[test]
    fn settled_step_projection_follows_the_state_decision_table() {
        // Cause/effect graph: C1 the Host result is Awaiting; C2 it is Ended;
        // C3 it is the forbidden unsettled Running state; C4 an awaiting tool
        // exists. Effects are E1 an Awaiting application result with the exact
        // pending identity, E2 an Ended result, and E3 a fail-closed internal
        // error. Rules P1 C1+C4=>E1, P2 C2=>E2, P3 C3=>E3 exhaust RunState and
        // make both public Run adapters share this single projection owner.
        let awaiting = settled_step(result(
            RunState::Awaiting,
            Some(PendingTool {
                tool_use_id: "tool-1".into(),
                name: "lookup".into(),
                input: serde_json::json!({"query": "awaken"}),
                client_executed: true,
            }),
        ))
        .expect("P1/E1");
        assert_eq!(
            awaiting
                .pending()
                .map(|pending| pending.tool_use_id.as_str()),
            Some("tool-1"),
            "P1/E1"
        );

        let ended =
            settled_step(result(RunState::Ended(EndCause::NaturalEnd), None)).expect("P2/E2");
        assert!(ended.pending().is_none(), "P2/E2");

        let error = settled_step(result(RunState::Running, None)).expect_err("P3/E3");
        assert_eq!(error.code, "internal", "P3/E3");
    }
}
