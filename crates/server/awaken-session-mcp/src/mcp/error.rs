//! The failure every Session MCP operation reports. It carries the same fault
//! classes and stable codes the Runtime Host's own error uses, so the Host's
//! conversion and the Managed `RunError` projection below stay lossless.

use awaken_session_error_contract::RunError;

/// A Session MCP failure classified by fault: `BadRequest` is the caller's,
/// `Internal` the runtime's, `Unavailable` retryable.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct McpError {
    pub message: String,
    pub kind: McpErrorKind,
    /// Stable neutral classification. Adapters project this value and never
    /// infer fault identity from human-readable text.
    pub code: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpErrorKind {
    Internal,
    BadRequest,
    Unavailable,
}

impl McpError {
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: McpErrorKind::Internal,
            code: "internal".into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: McpErrorKind::BadRequest,
            code: "invalid_request".into(),
        }
    }

    pub fn unavailable_classified(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: McpErrorKind::Unavailable,
            code: code.into(),
        }
    }

    pub fn classified(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: McpErrorKind::Internal,
            code: code.into(),
        }
    }
}

/// Project an MCP failure onto the neutral Run failure exactly as the Runtime
/// Host projects its own error: a bad request keeps only its message, and the
/// default `internal`/`unavailable` codes select the unclassified forms.
pub(crate) fn run_error(error: McpError) -> RunError {
    match error.kind {
        McpErrorKind::BadRequest => RunError::bad_request(error.message),
        McpErrorKind::Unavailable if error.code == "unavailable" => {
            RunError::unavailable(error.message)
        }
        McpErrorKind::Unavailable => RunError::unavailable_classified(error.code, error.message),
        McpErrorKind::Internal if error.code == "internal" => RunError::internal(error.message),
        McpErrorKind::Internal => RunError::classified(error.code, error.message),
    }
}
