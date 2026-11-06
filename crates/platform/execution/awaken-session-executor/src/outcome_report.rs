// @formal surface exclude=application_adapter
// @formal owner crates/platform/execution/awaken-session-executor/src/outcome_controller.rs
// @formal reason: Pure field-preserving projections of the extension-owned Outcome controller results and
// @formal reason: errors onto the Host and Session outcome vocabularies; they choose no Outcome transition.
//! Projections of Outcome controller results: the controller report onto the
//! Host report, the Host report onto the Session outcome vocabulary, and the
//! controller error onto the executor error. Pure functions; the Outcome transitions stay in
//! `awaken-ext-goal`.

use awaken_ext_goal::controller::Error as ControllerError;
use awaken_session_execution_contract::{OutcomeDrive, OutcomeIteration, OutcomeReport};

use crate::{HostOutcomeDrive, HostOutcomeIteration, HostOutcomeReport, SessionExecutionError};

pub(crate) fn project_report(report: awaken_ext_goal::controller::Report) -> HostOutcomeReport {
    HostOutcomeReport {
        iterations: report
            .iterations
            .into_iter()
            .map(|iteration| HostOutcomeIteration {
                messages: iteration.messages,
                outcome_id: iteration.outcome_id.0,
                description: iteration.description,
                iteration: iteration.iteration,
                result: iteration.result.token().into(),
                explanation: iteration.explanation,
                usage: iteration.usage,
            })
            .collect(),
    }
}

/// The controller error as this executor's error, classified once by
/// `awaken_session_run_delivery::outcome_controller_error`.
pub(crate) fn controller_error(error: ControllerError) -> SessionExecutionError {
    crate::session_execution_error::from_run_error(
        awaken_session_run_delivery::outcome_controller_error(error),
    )
}

pub(crate) fn report(report: HostOutcomeReport) -> OutcomeReport {
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
                usage: iteration.usage,
            })
            .collect(),
    }
}

pub(crate) fn progress(progress: HostOutcomeDrive) -> OutcomeDrive {
    match progress {
        HostOutcomeDrive::Awaiting => OutcomeDrive::Awaiting,
        HostOutcomeDrive::Completed(completed) => OutcomeDrive::Completed(report(completed)),
    }
}
