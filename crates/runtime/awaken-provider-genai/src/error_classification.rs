use awaken_runtime_contract::llm::Error;
use awaken_runtime_contract::model_operation::{
    ModelExecutionEvidence, ResponseEnvelope, TerminalReceipt,
};

fn terminal_http_status(status: reqwest::StatusCode) -> bool {
    status.is_client_error() || status.is_server_error()
}

fn complete_web_error(error: &genai::webc::Error) -> bool {
    matches!(error,
        genai::webc::Error::ResponseFailedStatus { status, .. }
            if terminal_http_status(*status))
}

fn complete_http_error(error: &genai::Error) -> bool {
    match error {
        genai::Error::HttpError { status, body, .. } => {
            // genai retains a failed body read in this sentinel string. A
            // status line alone is not a complete response envelope.
            terminal_http_status(*status) && !body.starts_with("Failed to read error body:")
        }
        genai::Error::WebAdapterCall { webc_error, .. }
        | genai::Error::WebModelCall { webc_error, .. } => complete_web_error(webc_error),
        genai::Error::WebStream { error, .. } => {
            error
                .downcast_ref::<genai::Error>()
                .is_some_and(complete_http_error)
                || error
                    .downcast_ref::<genai::webc::Error>()
                    .is_some_and(complete_web_error)
        }
        _ => false,
    }
}

pub(crate) fn classify_genai_error(error: genai::Error) -> Error {
    let complete = complete_http_error(&error);
    let classified = classify_error(&error.to_string());
    if complete {
        classified.with_execution_evidence(ModelExecutionEvidence::Terminated(
            TerminalReceipt::from_response_envelope(ResponseEnvelope::CompleteErrorBody),
        ))
    } else {
        classified
    }
}

/// Classify a provider error string into the contract's error taxonomy. The
/// SDK surfaces errors as strings, so classification matches lowercase
/// substrings, most-specific class first.
pub fn classify_error(message: &str) -> Error {
    fn hits(lower: &str, needles: &[&str]) -> bool {
        needles.iter().any(|needle| lower.contains(needle))
    }

    let lower = message.to_lowercase();
    const OVERFLOW: &[&str] = &[
        "prompt is too long",
        "context_length_exceeded",
        "context length",
        "input is too long",
        "maximum context length",
        "reduce the length",
        "too many tokens",
        "request too large",
        "413",
    ];
    const CONTENT_FILTER: &[&str] = &["content_filter", "content filter", "content policy"];
    const UNAUTHORIZED: &[&str] = &[
        "401",
        "403",
        "unauthorized",
        "forbidden",
        "invalid api key",
        "authentication",
        "permission denied",
    ];
    const MODEL_NOT_FOUND: &[&str] = &["404", "model not found", "model_not_found"];
    const RATE_LIMITED: &[&str] = &["rate limit", "ratelimit", "429", "too many requests"];
    const OVERLOADED: &[&str] = &["overloaded", "529", "503", "unavailable"];
    const TIMEOUT: &[&str] = &["timeout", "timed out", "408", "504"];
    const INVALID_REQUEST: &[&str] = &["400", "422", "invalid request", "invalid_request_error"];
    const USAGE_LIMIT: &[&str] = &[
        "usage limit",
        "quota",
        "insufficient_quota",
        "insufficient balance",
        "insufficient_balance",
        "payment required",
        "billing",
        "credit balance",
        "out of credit",
        "spending limit",
        "weekly limit",
        "monthly limit",
        "exceeded your current",
    ];
    const LOGIN_REQUIRED: &[&str] = &[
        "login required",
        "please log in",
        "session expired",
        "token expired",
        "token has expired",
        "invalid_grant",
        "reauthenticate",
        "re-authenticate",
    ];

    let message = message.to_string();
    if hits(&lower, CONTENT_FILTER) {
        Error::ContentFiltered(message)
    } else if hits(&lower, OVERFLOW) {
        Error::ContextOverflow(message)
    } else if hits(&lower, LOGIN_REQUIRED) {
        Error::LoginRequired(message)
    } else if hits(&lower, UNAUTHORIZED) {
        Error::Unauthorized(message)
    } else if hits(&lower, USAGE_LIMIT) {
        Error::UsageLimit {
            message,
            reset_after: None,
        }
    } else if hits(&lower, MODEL_NOT_FOUND) {
        Error::ModelNotFound(message)
    } else if hits(&lower, RATE_LIMITED) {
        Error::RateLimited {
            message,
            retry_after: None,
        }
    } else if hits(&lower, OVERLOADED) {
        Error::Overloaded {
            message,
            retry_after: None,
        }
    } else if hits(&lower, TIMEOUT) {
        Error::Timeout(message)
    } else if hits(&lower, INVALID_REQUEST) {
        Error::InvalidRequest(message)
    } else {
        Error::Provider(message)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use awaken_runtime_contract::model_operation::{
        ModelExecutionEvidence, ResponseEnvelope, TerminalReceipt,
    };
    use reqwest::StatusCode;

    use super::{classify_error, classify_genai_error};
    use crate::DEFAULT_IDLE_TIMEOUT;

    #[test]
    fn structured_terminal_http_response_does_not_promote_redirects_or_opaque_failures() {
        // Causes: C1 complete 503 body, C2 complete 307 body, C3 opaque 503
        // text. Effects: E1 only C1 mints CompleteErrorBody; E2 C2/C3 remain
        // Unknown. Rules R1=C1=>E1, R2=C2|C3=>E2.
        let terminal = classify_genai_error(genai::Error::HttpError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            canonical_reason: "Service Unavailable".into(),
            body: r#"{"error":{"code":"service_unavailable_error"}}"#.into(),
            headers: Default::default(),
        });
        assert_eq!(terminal.code(), "overloaded", "R1/E1");
        assert_eq!(
            terminal.execution_evidence(),
            ModelExecutionEvidence::Terminated(TerminalReceipt::from_response_envelope(
                ResponseEnvelope::CompleteErrorBody,
            )),
            "R1/E1"
        );

        let redirect = classify_genai_error(genai::Error::HttpError {
            status: StatusCode::TEMPORARY_REDIRECT,
            canonical_reason: "Temporary Redirect".into(),
            body: "{}".into(),
            headers: Default::default(),
        });
        assert_eq!(
            redirect.execution_evidence(),
            ModelExecutionEvidence::Unknown,
            "R2/E2"
        );
        let opaque = classify_genai_error(genai::Error::Internal("503 Service Unavailable".into()));
        assert_eq!(
            opaque.execution_evidence(),
            ModelExecutionEvidence::Unknown,
            "R2/E2"
        );
    }

    #[test]
    fn default_stream_idle_window_allows_long_reasoning_prefill() {
        assert!(DEFAULT_IDLE_TIMEOUT >= Duration::from_secs(300));
    }

    #[test]
    fn provider_error_taxonomy_precedence_decision_table() {
        // C1 hard usage exhaustion, C2 transient rate limit, C3 login expiry,
        // C4 plain authorization rejection. R1=C1=>terminal usage_limit;
        // R2=C2=>retryable rate_limited; R3=C3=>login_required; R4=C4=>auth.
        for message in [
            "You have exceeded your current quota",
            "429 insufficient_quota",
            r#"402 Payment Required: {"error":{"message":"Insufficient Balance","type":"unknown_error","code":"invalid_request_error"}}"#,
            "Your credit balance is too low",
            "weekly limit reached",
            "monthly spending limit exceeded",
        ] {
            let error = classify_error(message);
            assert_eq!(error.code(), "usage_limit", "R1: {message:?}");
            assert!(!error.is_retryable(), "R1: {message:?}");
        }
        let rate_limit = classify_error("429 Too Many Requests: rate limit exceeded");
        assert_eq!(rate_limit.code(), "rate_limited", "R2");
        assert!(rate_limit.is_retryable(), "R2");
        for message in [
            "Session expired, please log in",
            "invalid_grant",
            "token has expired",
        ] {
            assert_eq!(classify_error(message).code(), "login_required", "R3");
        }
        assert_eq!(
            classify_error("401 Unauthorized").code(),
            "unauthorized",
            "R4"
        );
    }
}
