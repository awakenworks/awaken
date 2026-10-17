// @formal evidence session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
//! Why dispatch could not admit, place, control or resolve a request.
//!
//! Dispatch reports every failure of its public operations as an
//! [`AdmissionError`]. A Session realization failure reaches dispatch through
//! the realization's one Run projection, which carries the class its
//! disposition implies; the claim resolver projects that class and never reads
//! a message.

use std::borrow::Cow;

use awaken_dispatch_contract::DispatchError;
use awaken_session_realization_contract::SessionRealizationControlDisposition;
use awaken_session_types::RunError;

#[derive(Debug, thiserror::Error)]
pub enum AdmissionError {
    /// The request is invalid for the current dispatch state; an identical
    /// request is refused the same way.
    #[error("{0}")]
    Rejected(Cow<'static, str>),
    /// The dispatch store refused or failed the durable operation.
    #[error(transparent)]
    Dispatch(#[from] DispatchError),
    /// A live Run control command found no target or failed to deliver.
    #[error(transparent)]
    LiveControl(#[from] crate::LiveRunControlError),
    /// The Session realization or the Run assembly over it failed with a
    /// neutral, classified Run error.
    #[error("{}", .0.message)]
    Run(#[from] RunError),
    /// The Session resource manifest could not be encoded into its envelope.
    #[error("serialize Session resource manifest: {0}")]
    ResourceEnvelope(serde_json::Error),
    /// The Session runtime projection could not be encoded for dispatch.
    #[error(transparent)]
    RuntimeEnvelope(#[from] awaken_session_configuration_contract::SessionRuntimeDispatchError),
    /// A one-shot composition role was bound twice.
    #[error("{0} already bound")]
    AlreadyBound(&'static str),
}

impl AdmissionError {
    pub fn rejected(message: impl Into<Cow<'static, str>>) -> Self {
        Self::Rejected(message.into())
    }

    /// The Worker effect class a claim takes for this failure. A Run failure
    /// (including a realization failure's Run projection) uses the contract's
    /// classification, so the claim resolver and the realization driver reach
    /// the same outcome for the same failure.
    #[must_use]
    pub fn claim_disposition(&self) -> SessionRealizationControlDisposition {
        match self {
            Self::Rejected(_)
            | Self::AlreadyBound(_)
            | Self::ResourceEnvelope(_)
            | Self::RuntimeEnvelope(_)
            | Self::Dispatch(DispatchError::Rejected(_))
            | Self::LiveControl(
                crate::LiveRunControlError::NotFound(_)
                | crate::LiveRunControlError::NoSubscriber(_),
            ) => SessionRealizationControlDisposition::Terminal,
            Self::Dispatch(DispatchError::Conflict(_) | DispatchError::Unavailable(_))
            | Self::LiveControl(crate::LiveRunControlError::Dispatch(_)) => {
                SessionRealizationControlDisposition::Retryable
            }
            Self::Run(error) => {
                awaken_session_realization_contract::effect_failure_disposition(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_failures_project_one_claim_class() {
        // Decision table: cause = the admission failure; effect = the claim
        // class. A Run failure keeps the contract's class (a conflict and an
        // unclassified internal fault relinquish within the retry budget);
        // dispatch-local refusals absorb; store and delivery faults
        // relinquish. The
        // realization's projection into this class is the realization's table.
        use SessionRealizationControlDisposition::{NotReady, Retryable, Terminal};
        let rows = [
            (
                "A1 invalid request",
                AdmissionError::rejected("no awaiting run"),
                Terminal,
            ),
            (
                "A2 role bound twice",
                AdmissionError::AlreadyBound("Worker evidence"),
                Terminal,
            ),
            (
                "A3 store rejected",
                AdmissionError::Dispatch(DispatchError::Rejected("x".into())),
                Terminal,
            ),
            (
                "A4 store unavailable",
                AdmissionError::Dispatch(DispatchError::Unavailable("x".into())),
                Retryable,
            ),
            (
                "A4 store conflict",
                AdmissionError::Dispatch(DispatchError::Conflict("x".into())),
                Retryable,
            ),
            (
                "A5 unknown run",
                AdmissionError::LiveControl(crate::LiveRunControlError::NotFound("r".into())),
                Terminal,
            ),
            (
                "A6 live delivery failed",
                AdmissionError::LiveControl(crate::LiveRunControlError::Dispatch("x".into())),
                Retryable,
            ),
            (
                "A8 Run capacity unavailable",
                AdmissionError::from(RunError::unavailable_classified(
                    awaken_session_realization_contract::ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE,
                    "quota",
                )),
                NotReady,
            ),
            (
                "A8 Run unavailable",
                AdmissionError::from(RunError::unavailable("u")),
                Retryable,
            ),
            (
                "A8 Run unclassified internal",
                AdmissionError::from(RunError::internal("i")),
                Retryable,
            ),
            (
                "A8 Run classified internal",
                AdmissionError::from(RunError::classified(
                    "session_realization_invariant_violated",
                    "i",
                )),
                Terminal,
            ),
            (
                "A8 Run state conflict",
                AdmissionError::from(RunError::conflict("c")),
                Retryable,
            ),
            (
                "A8 Run invalid",
                AdmissionError::from(RunError::bad_request("b")),
                Terminal,
            ),
        ];
        for (rule, error, expected) in rows {
            assert_eq!(error.claim_disposition(), expected, "{rule}");
        }
    }
}
