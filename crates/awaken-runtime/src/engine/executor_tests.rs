use std::time::Duration;

use awaken_contract::contract::executor::InferenceExecutionError;
use reqwest::StatusCode;

use super::executor::GenaiExecutor;

fn make_webstream_with_http_error(status: StatusCode, body: &str) -> genai::Error {
    use genai::ModelIden;
    use genai::adapter::AdapterKind;

    genai::Error::WebStream {
        model_iden: ModelIden::new(AdapterKind::Vertex, "vertex::gemini-2.5-flash"),
        cause: format!("HTTP error.\nStatus: {status} ...\nBody: {body}"),
        error: Box::new(genai::Error::HttpError {
            status,
            canonical_reason: status.canonical_reason().unwrap_or("Unknown").into(),
            body: body.into(),
        }),
    }
}

fn make_webstream_with_webc_status(
    status: StatusCode,
    body: &str,
    retry_after_secs: Option<u64>,
) -> genai::Error {
    use genai::ModelIden;
    use genai::adapter::AdapterKind;

    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(secs) = retry_after_secs {
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_str(&secs.to_string()).unwrap(),
        );
    }
    genai::Error::WebStream {
        model_iden: ModelIden::new(AdapterKind::OpenAI, "gpt-4o-mini"),
        cause: format!("HTTP error.\nStatus: {status}"),
        error: Box::new(genai::webc::Error::ResponseFailedStatus {
            status,
            body: body.into(),
            headers: Box::new(headers),
        }),
    }
}

#[test]
fn map_error_webstream_403_quota_pre_consume_no_retry() {
    let body = r#"{"error":{"code":403,"message":"pre_consume_token_quota_failed: quota exhausted before token consumption","type":"pre_consume_token_quota_failed"}}"#;
    let err = make_webstream_with_http_error(StatusCode::FORBIDDEN, body);
    let mapped = GenaiExecutor::map_error(err);

    assert!(
        matches!(mapped, InferenceExecutionError::Unauthorized(_)),
        "expected quota 403 to be permanent Unauthorized, got {mapped:?}"
    );
    assert!(
        !mapped.is_retryable(),
        "quota 403 must not enter the transient retry loop"
    );
    assert!(
        mapped
            .to_string()
            .contains("pre_consume_token_quota_failed"),
        "quota classifier must preserve the upstream quota reason"
    );
}

#[test]
fn map_error_webstream_429_rate_limited_with_retry_after_regression() {
    let err = make_webstream_with_webc_status(
        StatusCode::TOO_MANY_REQUESTS,
        r#"{"error":"rate limited"}"#,
        Some(42),
    );
    let mapped = GenaiExecutor::map_error(err);
    let retry_after = match &mapped {
        InferenceExecutionError::RateLimited { retry_after, .. } => *retry_after,
        other => panic!("expected RateLimited, got {other:?}"),
    };

    assert_eq!(retry_after, Some(Duration::from_secs(42)));
    assert!(mapped.is_retryable());
}
