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

impl RunError {
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
