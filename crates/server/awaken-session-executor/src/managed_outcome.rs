// @formal surface exclude=application_adapter
// @formal owner crates/server/awaken-session-executor/src/outcome_controller.rs
// @formal reason: Static owner: field-preserving adapter between Session outcome ports and the Host
// @formal reason: outcome controller. Dynamic path: define and resume calls
// @formal reason: delegate to ExecutorComponents and project the existing controller result without choosing
// @formal reason: outcome transitions. Evidence:
// @formal reason: crates/server/awaken-session-executor/src/outcome_controller.rs.
//! Managed Session projection for the Host-owned Outcome controller.

use awaken_session_execution_contract::OutcomeDrive;
use awaken_session_types::RunError;

use crate::managed_adapter_error::to_run_error;
use crate::outcome_report::progress;

pub(crate) async fn define(
    host: &crate::Outcomes,
    thread: &str,
    description: &str,
    rubric: &str,
    max_iterations: u32,
) -> Result<OutcomeDrive, RunError> {
    host.define_outcome(thread, description, rubric, max_iterations)
        .await
        .map(progress)
        .map_err(to_run_error)
}

pub(crate) async fn prepare(
    host: &crate::Outcomes,
    thread: &str,
    outcome_id: &str,
    description: &str,
    rubric: &str,
    max_iterations: u32,
) -> Result<u64, RunError> {
    host.prepare_outcome(thread, outcome_id, description, rubric, max_iterations)
        .await
        .map_err(to_run_error)
}

pub(crate) async fn resume(
    host: &crate::Outcomes,
    thread: &str,
) -> Result<Option<OutcomeDrive>, RunError> {
    host.continue_outcome(thread)
        .await
        .map_err(to_run_error)
        .map(|state| state.map(progress))
}
