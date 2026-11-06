//! The executor's settled steps as the neutral application outcome.

use awaken_runtime_contract::runs::{RunError, StepOutcome};

use awaken_session_run_delivery::{CommittedStepReceipt, settled_step};

use crate::SessionExecutionError;
use crate::session_execution_error::to_run_error;

/// Cross the shared Host receipt boundary once, preserving the existing error
/// classifier and the canonical settled-step projection.
pub(crate) fn finish_managed_step(
    result: Result<CommittedStepReceipt, SessionExecutionError>,
) -> Result<StepOutcome, RunError> {
    result.map_err(to_run_error).and_then(settled_step)
}
