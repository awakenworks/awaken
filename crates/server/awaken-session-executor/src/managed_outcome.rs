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

use crate::outcome_report::progress;
use crate::session_execution_error::to_run_error;

pub(crate) async fn define(
    host: &crate::Outcomes,
    thread: &str,
    description: &str,
    rubric: &str,
    max_iterations: u32,
    admission: &dyn awaken_session_execution_contract::PreparedSessionRunAdmission,
) -> Result<OutcomeDrive, RunError> {
    let dispatch = host
        .session_runs
        .coordinator_dispatch()
        .map_err(to_run_error)?;
    host.define_outcome(
        thread,
        description,
        rubric,
        max_iterations,
        &awaken_session_run_delivery::OutcomeRunExecutor::new(admission, dispatch.as_ref()),
    )
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
    admission: &dyn awaken_session_execution_contract::PreparedSessionRunAdmission,
) -> Result<Option<OutcomeDrive>, RunError> {
    let dispatch = host
        .session_runs
        .coordinator_dispatch()
        .map_err(to_run_error)?;
    host.continue_outcome(
        thread,
        &awaken_session_run_delivery::OutcomeRunExecutor::new(admission, dispatch.as_ref()),
    )
    .await
    .map_err(to_run_error)
    .map(|state| state.map(progress))
}
