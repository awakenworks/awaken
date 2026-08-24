//! Managed Session projection for the Host-owned Outcome controller.

use awaken_ext_goal::controller::CommittedOutcome;
use awaken_session_contract::{
    CommittedOutcomeProjection, OutcomeDrive, OutcomeFailure, OutcomeIteration, OutcomeReport,
    RunError,
};

use crate::host::{HostOutcomeDrive, HostOutcomeReport, SharedHost};
use crate::managed_adapter_error::to_run_error;
use crate::outcome_controller::project_report;

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

pub(crate) async fn prepare(
    host: &SharedHost,
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
    host: &SharedHost,
    thread: &str,
) -> Result<Option<OutcomeDrive>, RunError> {
    host.continue_outcome(thread)
        .await
        .map_err(to_run_error)
        .map(|state| state.map(progress))
}

pub(crate) async fn committed_projection(
    host: &SharedHost,
    thread: &str,
    outcome_id: &str,
) -> Result<Option<CommittedOutcomeProjection>, RunError> {
    host.committed_outcome_projection(thread, outcome_id)
        .await
        .map_err(to_run_error)
        .map(|state| {
            state.map(|terminal| match terminal {
                CommittedOutcome::Completed(completed) => {
                    CommittedOutcomeProjection::Completed(report(project_report(completed)))
                }
                CommittedOutcome::Errored {
                    failure,
                    source_run_id,
                } => CommittedOutcomeProjection::Errored(OutcomeFailure {
                    code: failure.code().to_string(),
                    message: failure.message().to_string(),
                    source_run_id,
                }),
            })
        })
}
