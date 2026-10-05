// @formal surface requirement=session.lifecycle.session_state
// @formal reason: The neutral fault decision of a committed Run boundary, moved from the Managed
// @formal reason: error classifier: a classified failure, an indeterminate end, or a cancellation that
// @formal reason: left a non-reentrant model operation is a fault; everything else is not. Pure over
// @formal reason: the committed prefix; wire error families stay with the protocol adapter.
//! Whether a committed Run boundary is a fault the timeline must surface.

use awaken_agent_contract::RunLifecycleEvent;
use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;

use crate::ProjectionError;

/// A Run fault with the neutral classification its committed state records.
///
/// Consumer: the Thread error fact; the protocol adapter classifies it into
/// its wire error family.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunFault {
    /// The Run ended in a classified failure.
    Failed { code: String, message: String },
    /// The physical result is unknown and must not be repeated automatically.
    ExecutionUnknown,
}

/// The fault recorded by a Run's committed state alone.
#[must_use]
pub fn run_state_fault(state: &RunState) -> Option<RunFault> {
    match state {
        RunState::Ended(EndCause::Error(failure)) => Some(RunFault::Failed {
            code: failure.code().to_string(),
            message: failure.message(),
        }),
        RunState::Ended(EndCause::Indeterminate) => Some(RunFault::ExecutionUnknown),
        _ => None,
    }
}

/// One root/child classifier over the same lifecycle and recovery prefix.
/// A cancellation that left a model operation which may not be re-entered
/// has an unknown physical result; cancellation intent never supplies
/// termination evidence, and a later receipt cannot rewrite an already
/// published historical diagnosis.
pub fn run_fault(
    terminal: &RunLifecycleEvent,
    snapshot: Option<&RunRecoverySnapshot>,
) -> Result<Option<RunFault>, ProjectionError> {
    if matches!(terminal.state, RunState::Ended(EndCause::Cancelled))
        && let Some(snapshot) = snapshot
    {
        let operations =
            awaken_runtime_contract::model_operation::ActiveModelOperation::recorded_at_commit(
                &snapshot.state,
                &snapshot.state_commit_cursors,
                &terminal.run_id,
                terminal.source_commit_cursor,
            )
            .map_err(|error| ProjectionError::corrupt(error.to_string()))?;
        if operations
            .iter()
            .any(|operation| !operation.execution.permits_reentry())
        {
            return Ok(Some(RunFault::ExecutionUnknown));
        }
    }
    Ok(run_state_fault(&terminal.state))
}
