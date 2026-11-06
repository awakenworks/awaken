// @formal surface requirement=protocol.ui_adapters.stream_projection
// @formal reason: Stable Run failure kinds and codes are shared protocol inputs;
// @formal reason: adapters project them without inventing lifecycle or retry facts.
//! Neutral Run failure classification.

#[derive(Debug, thiserror::Error)]
#[error("run failed: {message}")]
pub struct RunError {
    pub message: String,
    pub kind: RunErrorKind,
    pub code: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunErrorKind {
    Internal,
    BadRequest,
    Unavailable,
    /// The request raced a concurrent change of the state it reads (an
    /// optimistic-concurrency loss or a busy owner). The same request may
    /// succeed once that change settles.
    Conflict,
}

/// The code of an internal fault no producer classified. Any other code on an
/// Internal fault names a known, unrecoverable cause.
pub const UNCLASSIFIED_INTERNAL_CODE: &str = "internal";

/// The code of a state conflict no producer classified further.
pub const CONFLICT_CODE: &str = "conflict";

/// Stable application facts. Protocol adapters project these without parsing prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RunErrorClassification {
    InvalidRequest,
    NotFound,
    UnsupportedOutputModes,
    Unavailable,
    Internal,
    Conflict,
}

impl RunError {
    /// Classify the existing kind/code pair; unknown codes retain their kind.
    pub fn classification(&self) -> RunErrorClassification {
        match (self.kind, self.code.as_str()) {
            (RunErrorKind::BadRequest, "not_found") => RunErrorClassification::NotFound,
            (RunErrorKind::BadRequest, "unsupported_output_modes") => {
                RunErrorClassification::UnsupportedOutputModes
            }
            (RunErrorKind::BadRequest, _) => RunErrorClassification::InvalidRequest,
            (RunErrorKind::Unavailable, _) => RunErrorClassification::Unavailable,
            (RunErrorKind::Internal, _) => RunErrorClassification::Internal,
            (RunErrorKind::Conflict, _) => RunErrorClassification::Conflict,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::Internal,
            code: UNCLASSIFIED_INTERNAL_CODE.into(),
        }
    }

    /// An internal fault whose cause no producer classified.
    #[must_use]
    pub fn is_unclassified_internal(&self) -> bool {
        self.kind == RunErrorKind::Internal && self.code == UNCLASSIFIED_INTERNAL_CODE
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::conflict_classified(CONFLICT_CODE, message)
    }

    pub fn conflict_classified(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::Conflict,
            code: code.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::BadRequest,
            code: "invalid_request".into(),
        }
    }

    pub fn bad_request_classified(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::BadRequest,
            code: code.into(),
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::Unavailable,
            code: "unavailable".into(),
        }
    }

    pub fn unavailable_classified(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::Unavailable,
            code: code.into(),
        }
    }

    pub fn classified(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::Internal,
            code: code.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_error_display_and_kind_mapping() {
        // R1 ordinary errors retain the four stable classifications/codes;
        // R2 classified errors preserve the caller's code without changing kind.
        // HTTP status selection stays with each transport adapter.
        for (error, kind, code) in [
            (
                RunError::internal("failure"),
                RunErrorKind::Internal,
                "internal",
            ),
            (
                RunError::bad_request("failure"),
                RunErrorKind::BadRequest,
                "invalid_request",
            ),
            (
                RunError::unavailable("failure"),
                RunErrorKind::Unavailable,
                "unavailable",
            ),
            (
                RunError::classified("provider", "failure"),
                RunErrorKind::Internal,
                "provider",
            ),
            (
                RunError::bad_request_classified("input", "failure"),
                RunErrorKind::BadRequest,
                "input",
            ),
            (
                RunError::unavailable_classified("dependency", "failure"),
                RunErrorKind::Unavailable,
                "dependency",
            ),
            (
                RunError::conflict("failure"),
                RunErrorKind::Conflict,
                "conflict",
            ),
            (
                RunError::conflict_classified("binding", "failure"),
                RunErrorKind::Conflict,
                "binding",
            ),
        ] {
            assert_eq!(error.kind, kind);
            assert_eq!(error.code, code);
            assert_eq!(error.message, "failure");
            assert_eq!(error.to_string(), "run failed: failure");
        }
    }

    #[test]
    fn only_the_unclassified_internal_code_is_an_unknown_internal_fault() {
        // Decision table: C1 the fault kind; C2 the code is the unclassified
        // internal code. Effect: whether the fault is an unknown internal one
        // (bounded retry) rather than a classified, unrecoverable cause.
        // | Rule | C1 | C2 | unknown internal | classification |
        // | U1 | Internal | yes | yes | Internal |
        // | U2 | Internal | no | no | Internal |
        // | U3 | BadRequest/Unavailable/Conflict | yes | no | own kind |
        let rows = [
            (
                "U1",
                RunError::internal("m"),
                true,
                RunErrorClassification::Internal,
            ),
            (
                "U2",
                RunError::classified("session_realization_invariant_violated", "m"),
                false,
                RunErrorClassification::Internal,
            ),
            (
                "U3",
                RunError::bad_request_classified(UNCLASSIFIED_INTERNAL_CODE, "m"),
                false,
                RunErrorClassification::InvalidRequest,
            ),
            (
                "U3",
                RunError::unavailable_classified(UNCLASSIFIED_INTERNAL_CODE, "m"),
                false,
                RunErrorClassification::Unavailable,
            ),
            (
                "U3",
                RunError::conflict("m"),
                false,
                RunErrorClassification::Conflict,
            ),
        ];
        for (rule, error, unknown, classification) in rows {
            assert_eq!(error.is_unclassified_internal(), unknown, "{rule}");
            assert_eq!(error.classification(), classification, "{rule}");
        }
    }
}
