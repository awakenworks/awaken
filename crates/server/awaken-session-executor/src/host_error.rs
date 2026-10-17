//! The error vocabulary every Session operation of this executor returns, and
//! its one projection to and from the neutral Run error. A Session realization
//! failure enters it in exactly one place, `From<RealizationError>`, through
//! the realization's own Run projection, so its disposition survives.

use awaken_session_mcp::{McpError, McpErrorKind};
use awaken_session_types::RunError;

/// A host failure classified by fault: `BadRequest` is the caller's (bad id,
/// wrong binding, no await), `Internal` is the runtime's. Each adapter maps this
/// to its own public error shape.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct HostError {
    pub message: String,
    pub kind: HostErrorKind,
    /// Stable neutral classification. Protocol adapters project this value and
    /// never infer fault identity from human-readable text.
    pub code: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostErrorKind {
    Internal,
    BadRequest,
    Conflict,
    Unavailable,
}

impl HostError {
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: HostErrorKind::Internal,
            code: "internal".into(),
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: HostErrorKind::BadRequest,
            code: "invalid_request".into(),
        }
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: HostErrorKind::Conflict,
            code: "conflict".into(),
        }
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: HostErrorKind::Unavailable,
            code: "unavailable".into(),
        }
    }
    pub fn unavailable_classified(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: HostErrorKind::Unavailable,
            code: code.into(),
        }
    }
    pub fn classified(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: HostErrorKind::Internal,
            code: code.into(),
        }
    }
}

pub fn to_run_error(error: HostError) -> RunError {
    match error.kind {
        HostErrorKind::BadRequest => RunError::bad_request(error.message),
        HostErrorKind::Conflict => RunError::conflict_classified(error.code, error.message),
        HostErrorKind::Unavailable if error.code == "unavailable" => {
            RunError::unavailable(error.message)
        }
        HostErrorKind::Unavailable => RunError::unavailable_classified(error.code, error.message),
        HostErrorKind::Internal if error.code == "internal" => RunError::internal(error.message),
        HostErrorKind::Internal => RunError::classified(error.code, error.message),
    }
}

/// Preserve the neutral Run classification when a durable Session port calls
/// back into Host lifecycle orchestration. This is the sole inverse projector;
/// authorization, receipt persistence, and readback must not each reinterpret
/// retryability from error strings.
pub fn from_run_error(error: RunError) -> HostError {
    HostError {
        message: error.message,
        kind: match error.kind {
            awaken_session_types::RunErrorKind::BadRequest => HostErrorKind::BadRequest,
            awaken_session_types::RunErrorKind::Internal => HostErrorKind::Internal,
            awaken_session_types::RunErrorKind::Unavailable => HostErrorKind::Unavailable,
            awaken_session_types::RunErrorKind::Conflict => HostErrorKind::Conflict,
        },
        code: error.code,
    }
}

/// The one entry of a realization failure: its Run projection already carries
/// the fault and code its disposition implies.
impl From<awaken_session_realization::realization_error::RealizationError> for HostError {
    fn from(error: awaken_session_realization::realization_error::RealizationError) -> Self {
        from_run_error(RunError::from(error))
    }
}

/// An executor failure inside a realization port (substrate, frozen
/// projection install) keeps its neutral Run class.
impl From<HostError> for awaken_session_realization::realization_error::RealizationError {
    fn from(error: HostError) -> Self {
        Self::Run(to_run_error(error))
    }
}

/// Dispatch admission failures keep their Run classification; a realization
/// failure reaches dispatch as its one Run projection.
impl From<awaken_dispatch_execution::AdmissionError> for HostError {
    fn from(error: awaken_dispatch_execution::AdmissionError) -> Self {
        use awaken_dispatch_execution::AdmissionError;
        match error {
            AdmissionError::Rejected(message) => Self::bad_request(message),
            AdmissionError::Run(error) => from_run_error(error),
            AdmissionError::LiveControl(
                error @ (awaken_dispatch_execution::LiveRunControlError::NotFound(_)
                | awaken_dispatch_execution::LiveRunControlError::NoSubscriber(_)),
            ) => Self::bad_request(error.to_string()),
            error @ (AdmissionError::Dispatch(_)
            | AdmissionError::LiveControl(_)
            | AdmissionError::ResourceEnvelope(_)
            | AdmissionError::RuntimeEnvelope(_)
            | AdmissionError::AlreadyBound(_)) => Self::internal(error.to_string()),
        }
    }
}

/// An executor failure crossing into dispatch keeps its neutral Run class.
impl From<HostError> for awaken_dispatch_execution::AdmissionError {
    fn from(error: HostError) -> Self {
        Self::Run(to_run_error(error))
    }
}

impl From<McpError> for HostError {
    fn from(error: McpError) -> Self {
        Self {
            message: error.message,
            kind: match error.kind {
                McpErrorKind::Internal => HostErrorKind::Internal,
                McpErrorKind::BadRequest => HostErrorKind::BadRequest,
                McpErrorKind::Unavailable => HostErrorKind::Unavailable,
            },
            code: error.code,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_and_host_error_projection_preserves_fault_and_code() {
        // Cause/effect table: C1 Run fault is BadRequest/Internal/Unavailable/
        // Conflict; C2 its code is default/custom. Every row must preserve both
        // fields through Run -> Host -> Run so lifecycle adapters cannot turn a
        // stale lease into an absorbing Internal error, flatten a conflict into
        // a request fault or erase a stable diagnosis.
        for original in [
            RunError::bad_request("bad"),
            RunError::internal("unknown"),
            RunError::classified("custom_internal", "internal"),
            RunError::unavailable_classified("session_realization_stale", "retry"),
            RunError::conflict("raced"),
        ] {
            let expected_kind = original.kind;
            let expected_code = original.code.clone();
            let projected = to_run_error(from_run_error(original));
            assert_eq!(projected.kind, expected_kind);
            assert_eq!(projected.code, expected_code);
        }
    }

    #[test]
    fn a_host_conflict_stays_a_conflict_across_the_run_boundary() {
        // Cause: an executor state conflict. Effects: its Run projection is a
        // Conflict with the Host's code, and the inverse restores the Host's
        // Conflict, so every adapter answers 409 conflict.
        let projected = to_run_error(HostError::conflict("raced"));
        assert_eq!(projected.kind, awaken_session_types::RunErrorKind::Conflict);
        assert_eq!(projected.code, "conflict");
        let host = from_run_error(projected);
        assert_eq!(host.kind, HostErrorKind::Conflict);
        assert_eq!(host.code, "conflict");
        assert_eq!(host.message, "raced");
    }
}
