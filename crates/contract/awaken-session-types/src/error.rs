//! Neutral Session runtime failure classification.

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
}

/// Stable application facts. Protocol adapters project these without parsing prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RunErrorClassification {
    InvalidRequest,
    NotFound,
    UnsupportedOutputModes,
    Unavailable,
    Internal,
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
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::Internal,
            code: "internal".into(),
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
        // R1 ordinary errors retain the three stable classifications/codes;
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
        ] {
            assert_eq!(error.kind, kind);
            assert_eq!(error.code, code);
            assert_eq!(error.message, "failure");
            assert_eq!(error.to_string(), "run failed: failure");
        }
    }
}
