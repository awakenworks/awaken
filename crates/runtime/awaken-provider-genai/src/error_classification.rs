use awaken_runtime_contract::llm::Error;
use awaken_runtime_contract::model_operation::{
    ModelExecutionEvidence, ResponseEnvelope, TerminalReceipt,
};

use super::classify_error;

pub(crate) fn classify_genai_error(error: genai::Error) -> Error {
    // Only a structured 4xx/5xx whose response body genai fully consumed proves
    // exact return. Redirects, timeouts, EOF and opaque transport causes do not.
    fn complete_http_response(error: &genai::Error) -> bool {
        let terminal =
            |status: reqwest::StatusCode| status.is_client_error() || status.is_server_error();
        let webc = |error: &genai::webc::Error| {
            matches!(error,
            genai::webc::Error::ResponseFailedStatus { status, .. } if terminal(*status))
        };
        match error {
            genai::Error::HttpError { status, .. } => terminal(*status),
            genai::Error::WebAdapterCall { webc_error, .. }
            | genai::Error::WebModelCall { webc_error, .. } => webc(webc_error),
            genai::Error::WebStream { error, .. } => {
                error
                    .downcast_ref::<genai::Error>()
                    .is_some_and(complete_http_response)
                    || error.downcast_ref::<genai::webc::Error>().is_some_and(webc)
            }
            _ => false,
        }
    }

    let complete = complete_http_response(&error);
    let classified = classify_error(&error.to_string());
    if complete {
        classified.with_execution_evidence(ModelExecutionEvidence::Terminated(
            TerminalReceipt::from_response_envelope(ResponseEnvelope::CompleteBody),
        ))
    } else {
        classified
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use awaken_runtime_contract::model_operation::{
        ModelExecutionEvidence, ResponseEnvelope, TerminalReceipt,
    };
    use reqwest::StatusCode;

    use super::classify_error;
    use super::classify_genai_error;
    use crate::DEFAULT_IDLE_TIMEOUT;

    #[test]
    fn complete_http_error_body_proves_return_without_promoting_opaque_failures() {
        // Structured 503 + complete body -> overloaded plus terminal receipt;
        // identical opaque text -> overloaded plus Unknown. Status text alone
        // never unlocks replay.
        let receipt = TerminalReceipt::from_response_envelope(ResponseEnvelope::CompleteBody);
        let structured = classify_genai_error(genai::Error::HttpError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            canonical_reason: "Service Unavailable".into(),
            body: r#"{"error":{"code":"service_unavailable_error"}}"#.into(),
            headers: Default::default(),
        });
        assert_eq!(structured.code(), "overloaded");
        assert!(structured.is_retryable());
        assert_eq!(
            structured.execution_evidence(),
            ModelExecutionEvidence::Terminated(receipt)
        );

        let opaque = classify_genai_error(genai::Error::Internal("503 Service Unavailable".into()));
        assert_eq!(opaque.code(), "overloaded");
        assert!(opaque.is_retryable());
        assert_eq!(opaque.execution_evidence(), ModelExecutionEvidence::Unknown);
    }

    #[test]
    fn default_stream_idle_window_allows_long_reasoning_prefill() {
        assert!(DEFAULT_IDLE_TIMEOUT >= Duration::from_secs(300));
    }

    #[test]
    fn a_hard_usage_limit_is_recognised_and_not_retryable() {
        for message in [
            "You have exceeded your current quota",
            "429 insufficient_quota",
            r#"402 Payment Required: {"error":{"message":"Insufficient Balance","type":"unknown_error","code":"invalid_request_error"}}"#,
            "Your credit balance is too low",
            "weekly limit reached",
            "monthly spending limit exceeded",
        ] {
            let error = classify_error(message);
            assert_eq!(error.code(), "usage_limit", "{message}");
            assert!(!error.is_retryable(), "{message}");
        }
    }

    #[test]
    fn a_transient_rate_limit_stays_rate_limited_not_usage_limit() {
        let error = classify_error("429 Too Many Requests: rate limit exceeded");
        assert_eq!(error.code(), "rate_limited");
        assert!(error.is_retryable());
    }

    #[test]
    fn a_reauth_signal_precedes_generic_unauthorized() {
        for message in [
            "Session expired, please log in",
            "invalid_grant",
            "token has expired",
        ] {
            assert_eq!(
                classify_error(message).code(),
                "login_required",
                "{message}"
            );
        }
        assert_eq!(classify_error("401 Unauthorized").code(), "unauthorized");
    }
}
