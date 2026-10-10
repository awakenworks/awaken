//! The error vocabulary every Session operation returns, and its one
//! projection to and from the neutral Run error. Realization and the run path
//! both return it, so it sits below both; the crate root re-exports the public
//! `HostError` path.

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

pub(crate) fn to_run_error(error: HostError) -> RunError {
    match error.kind {
        HostErrorKind::BadRequest | HostErrorKind::Conflict => RunError::bad_request(error.message),
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
pub(crate) fn from_run_error(error: RunError) -> HostError {
    HostError {
        message: error.message,
        kind: match error.kind {
            awaken_session_types::RunErrorKind::BadRequest => HostErrorKind::BadRequest,
            awaken_session_types::RunErrorKind::Internal => HostErrorKind::Internal,
            awaken_session_types::RunErrorKind::Unavailable => HostErrorKind::Unavailable,
        },
        code: error.code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_and_host_error_projection_preserves_fault_and_code() {
        // Cause/effect table: C1 Run fault is BadRequest/Internal/Unavailable;
        // C2 its code is default/custom. Every row must preserve both fields
        // through Run -> Host -> Run so lifecycle adapters cannot turn a stale
        // lease into an absorbing Internal error or erase a stable diagnosis.
        for original in [
            RunError::bad_request("bad"),
            RunError::classified("custom_internal", "internal"),
            RunError::unavailable_classified("session_realization_stale", "retry"),
        ] {
            let expected_kind = original.kind;
            let expected_code = original.code.clone();
            let projected = to_run_error(from_run_error(original));
            assert_eq!(projected.kind, expected_kind);
            assert_eq!(projected.code, expected_code);
        }
    }
}
