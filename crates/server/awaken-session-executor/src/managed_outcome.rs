//! Managed Session projection for the Host-owned Outcome controller.

use awaken_session_contract::{OutcomeDrive, OutcomeIteration, OutcomeReport, RunError};

use crate::host::{HostOutcomeDrive, HostOutcomeReport, SharedHost};
use crate::managed_adapter_error::to_run_error;

fn report(report: HostOutcomeReport) -> OutcomeReport {
    OutcomeReport {
        iterations: report
            .iterations
            .into_iter()
            .map(|iteration| OutcomeIteration {
                messages: iteration.messages,
                outcome_id: iteration.outcome_id,
                description: iteration.description,
                iteration: iteration.iteration,
                result: iteration.result,
                explanation: iteration.explanation,
            })
            .collect(),
    }
}

fn progress(progress: HostOutcomeDrive) -> OutcomeDrive {
    match progress {
        HostOutcomeDrive::Awaiting => OutcomeDrive::Awaiting,
        HostOutcomeDrive::Completed(completed) => OutcomeDrive::Completed(report(completed)),
    }
}

pub(crate) async fn define(
    host: &SharedHost,
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

pub(crate) async fn resume(
    host: &SharedHost,
    thread: &str,
) -> Result<Option<OutcomeDrive>, RunError> {
    host.continue_outcome(thread)
        .await
        .map_err(to_run_error)
        .map(|state| state.map(progress))
}
