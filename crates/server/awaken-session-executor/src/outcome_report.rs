// @formal surface exclude=application_adapter
// @formal owner crates/server/awaken-session-executor/src/outcome_controller.rs
// @formal reason: Pure field-preserving projections of the extension-owned Outcome controller results and
// @formal reason: errors onto the Host and Session outcome vocabularies; they choose no Outcome transition.
//! Projections of Outcome controller results: the controller report onto the
//! Host report, the Host report onto the Session outcome vocabulary, a
//! committed terminal Outcome onto its Session projection, and the controller
//! error onto the Host error. Pure functions; the Outcome transitions stay in
//! `awaken-ext-goal`.

use awaken_ext_goal::controller::{CommittedOutcome, Error as ControllerError};
use awaken_session_event_contract::OUTCOME_BUSY_CODE;
use awaken_session_execution_contract::{
    CommittedOutcomeProjection, OutcomeDrive, OutcomeFailure, OutcomeIteration, OutcomeReport,
};

use crate::{HostError, HostOutcomeDrive, HostOutcomeIteration, HostOutcomeReport};

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

pub(crate) fn controller_error(error: ControllerError) -> HostError {
    match error {
        ControllerError::Busy { .. } | ControllerError::WorkerRunning { .. } => {
            HostError::unavailable_classified(OUTCOME_BUSY_CODE, error.to_string())
        }
        ControllerError::ActiveDefinitionConflict { .. }
        | ControllerError::WorkerAwaiting { .. }
        | ControllerError::Domain(_) => HostError::bad_request(error.to_string()),
        ControllerError::Persistence(_)
        | ControllerError::Execution(_)
        | ControllerError::Serialization(_) => HostError::internal(error.to_string()),
    }
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

/// The Session projection of one committed terminal Outcome.
pub(crate) fn committed_projection(terminal: CommittedOutcome) -> CommittedOutcomeProjection {
    match terminal {
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
    }
}
