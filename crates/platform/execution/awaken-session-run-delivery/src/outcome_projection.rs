// @formal surface exclude=application_adapter
// @formal owner crates/platform/execution/awaken-session-executor/src/outcome_controller.rs
// @formal reason: Pure field-preserving projections of the extension-owned Outcome controller results and
// @formal reason: errors onto the Session outcome vocabulary; they choose no Outcome transition.
//! Projections of the Outcome controller onto the Session vocabulary: a
//! committed terminal Outcome onto its Session projection, and the controller
//! error onto the one Run error class. Pure functions; the Outcome
//! transitions stay in `awaken-ext-goal`.

use awaken_ext_goal::controller::{CommittedOutcome, Error as ControllerError, Report};
use awaken_runtime_contract::runs::RunError;
use awaken_session_contract::OUTCOME_BUSY_CODE;
use awaken_session_execution_contract::{
    CommittedOutcomeProjection, OutcomeFailure, OutcomeIteration, OutcomeReport,
};

/// The one classification of an Outcome controller failure: a busy Outcome is
/// retryable, a domain refusal is the caller's, the rest are internal.
#[must_use]
pub fn outcome_controller_error(error: ControllerError) -> RunError {
    match error {
        ControllerError::Admission(source) => {
            RunError::unavailable_classified(source.code(), source.to_string())
        }
        ControllerError::Busy { .. } | ControllerError::WorkerRunning { .. } => {
            RunError::unavailable_classified(OUTCOME_BUSY_CODE, error.to_string())
        }
        ControllerError::ActiveDefinitionConflict { .. }
        | ControllerError::WorkerAwaiting { .. }
        | ControllerError::Domain(_) => RunError::bad_request(error.to_string()),
        ControllerError::Persistence(_)
        | ControllerError::Execution(_)
        | ControllerError::Serialization(_) => RunError::internal(error.to_string()),
    }
}

/// The Session report of one controller report, iteration by iteration.
#[must_use]
pub fn outcome_report(report: Report) -> OutcomeReport {
    OutcomeReport {
        iterations: report
            .iterations
            .into_iter()
            .map(|iteration| OutcomeIteration {
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

/// The Session projection of one committed terminal Outcome.
#[must_use]
pub fn committed_outcome_projection(terminal: CommittedOutcome) -> CommittedOutcomeProjection {
    match terminal {
        CommittedOutcome::Completed(completed) => {
            CommittedOutcomeProjection::Completed(outcome_report(completed))
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
