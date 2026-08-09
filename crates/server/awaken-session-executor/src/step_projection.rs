//! Canonical projection from one settled Runtime Host step into the neutral
//! Session/Run application vocabulary.

use awaken_agent_contract::agent::run::RunState;
use awaken_session_contract::{Pending, RunError, StepOutcome};

use crate::host::{CommittedStepReceipt, PendingTool};

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
