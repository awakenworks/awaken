//! The one place a failed model exchange becomes a contract [`Error`].
//!
//! Both shipped dialects (the genai-routed chat adapters and the dedicated
//! Responses adapter) describe what the transport observed as a
//! [`WireFailure`] and hand it to [`classify_wire_failure`]. That function is
//! the only classifier and the only minting site of the evidence a wire
//! failure establishes (a complete error body, or a connection that never
//! carried the request), so the two dialects cannot drift apart again.
//!
//! The HTTP status decides first. A response body is provider-authored free
//! text (it may quote a request id, a token count or another status), so body
//! markers only *refine* the statuses whose meaning genuinely depends on them
//! (400/413/422 and the auth/quota variants of 401/403/429). Substring
//! matching over the whole message remains only for failures that carry no
//! status at all.
use std::borrow::Cow;
use std::time::Duration;

use awaken_runtime_contract::llm::Error;
use awaken_runtime_contract::model_operation::{
    ModelExecutionEvidence, NoDispatchProof, ResponseEnvelope, TerminalReceipt,
};
use reqwest::StatusCode;
use reqwest::header::HeaderMap;

/// One failed exchange as the transport observed it. Built by the genai
/// projection below and by `OpenAiResponsesExecutor::execute`; consumed only by
/// [`classify_wire_failure`].
pub(crate) struct WireFailure<'a> {
    /// The response status, when a response line arrived at all.
    pub(crate) status: Option<StatusCode>,
    /// The response headers, when the transport retained them.
    pub(crate) headers: Option<&'a HeaderMap>,
    /// The error body, or the transport's own description when no response
    /// body exists.
    pub(crate) body: Cow<'a, str>,
    /// The whole error body was read, so the response envelope is complete.
    pub(crate) complete: bool,
    /// The transport failed while establishing its connection (DNS, TCP or
    /// TLS), so no request byte was written.
    pub(crate) unsent: bool,
}

impl WireFailure<'static> {
    /// A failure with no HTTP response behind it (connect, TLS, decode,
    /// stream parse); only its description is known.
    pub(crate) fn without_response(description: String) -> Self {
        Self {
            status: None,
            headers: None,
            body: Cow::Owned(description),
            complete: false,
            unsent: false,
        }
    }

    /// A failure the transport itself reported; a connect-phase failure
    /// carried no request byte.
    pub(crate) fn transport(error: &reqwest::Error) -> Self {
        Self {
            status: error.status(),
            headers: None,
            body: Cow::Owned(error.to_string()),
            complete: false,
            unsent: error.is_connect(),
        }
    }
}

fn terminal_http_status(status: StatusCode) -> bool {
    status.is_client_error() || status.is_server_error()
}

/// Classify one wire failure and attach the evidence it establishes. A
/// complete 4xx/5xx error body proves the provider terminated this attempt; a
/// connect-phase failure proves the request never left; every other shape
/// stays `Unknown`.
pub(crate) fn classify_wire_failure(failure: WireFailure<'_>) -> Error {
    let WireFailure {
        status,
        headers,
        body,
        complete,
        unsent,
    } = failure;
    let classified = match status {
        Some(status) if terminal_http_status(status) => classify_status(
            status,
            headers.and_then(retry_after_from_headers),
            &body,
            format!("HTTP {status}: {body}"),
        ),
        // A redirect or informational status surfaced as a failure is an
        // unexpected response shape, not something its body can reclassify.
        Some(status) => Error::Provider(format!("HTTP {status}: {body}")),
        None => classify_error(&body),
    };
    if complete && status.is_some_and(terminal_http_status) {
        classified.with_execution_evidence(ModelExecutionEvidence::Terminated(
            TerminalReceipt::from_response_envelope(ResponseEnvelope::CompleteErrorBody),
        ))
    } else if unsent && status.is_none() {
        classified.with_execution_evidence(ModelExecutionEvidence::NotDispatched(
            NoDispatchProof::transport_connect(),
        ))
    } else {
        classified
    }
}

/// Project a genai failure onto the wire description, then classify it.
pub(crate) fn classify_genai_error(error: genai::Error) -> Error {
    match genai_wire_failure(&error) {
        Some(failure) => classify_wire_failure(failure),
        None => classify_wire_failure(WireFailure::without_response(error.to_string())),
    }
}

fn genai_wire_failure(error: &genai::Error) -> Option<WireFailure<'_>> {
    match error {
        genai::Error::HttpError {
            status,
            body,
            headers,
            ..
        } => Some(WireFailure {
            status: Some(*status),
            headers: Some(headers),
            body: Cow::Borrowed(body),
            // genai retains a failed body read in this sentinel string. A
            // status line alone is not a complete response envelope.
            complete: !body.starts_with("Failed to read error body:"),
            unsent: false,
        }),
        genai::Error::WebAdapterCall { webc_error, .. }
        | genai::Error::WebModelCall { webc_error, .. } => webc_wire_failure(webc_error),
        // The streaming client surfaces its send failure as the bare reqwest
        // error, so a connect failure is recognized here too.
        genai::Error::WebStream { error, .. } => error
            .downcast_ref::<genai::Error>()
            .and_then(genai_wire_failure)
            .or_else(|| {
                error
                    .downcast_ref::<genai::webc::Error>()
                    .and_then(webc_wire_failure)
            })
            .or_else(|| {
                error
                    .downcast_ref::<reqwest::Error>()
                    .map(WireFailure::transport)
            }),
        _ => None,
    }
}

fn webc_wire_failure(error: &genai::webc::Error) -> Option<WireFailure<'_>> {
    match error {
        genai::webc::Error::ResponseFailedStatus {
            status,
            body,
            headers,
        } => Some(WireFailure {
            status: Some(*status),
            headers: Some(headers),
            body: Cow::Borrowed(body),
            complete: true,
            unsent: false,
        }),
        genai::webc::Error::Reqwest(reqwest_error) => Some(WireFailure::transport(reqwest_error)),
        _ => None,
    }
}

/// The provider's requested delay before the next attempt. Precedence:
/// `retry-after-ms` (sent by OpenAI and Anthropic, millisecond precision),
/// then RFC 9110 `Retry-After` as delta-seconds or as an HTTP-date. An
/// HTTP-date is measured against the same response's `Date` header — the
/// server's own clock — so no local wall clock is read and client/server skew
/// cannot distort it; without a `Date` there is no reference and no hint. A
/// date already in the past asks for no delay.
fn retry_after_from_headers(headers: &HeaderMap) -> Option<Duration> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
    };
    if let Some(millis) = header("retry-after-ms").and_then(|value| value.parse::<u64>().ok()) {
        return Some(Duration::from_millis(millis));
    }
    let value = header("retry-after")?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let retry_at = httpdate::parse_http_date(value).ok()?;
    let sent_at = httpdate::parse_http_date(header("date")?).ok()?;
    Some(retry_at.duration_since(sent_at).unwrap_or(Duration::ZERO))
}

fn hits(lower: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| lower.contains(needle))
}

const OVERFLOW: &[&str] = &[
    "prompt is too long",
    "context_length_exceeded",
    "context length",
    "input is too long",
    "maximum context length",
    "reduce the length",
    "too many tokens",
    "request too large",
];
const CONTENT_FILTER: &[&str] = &["content_filter", "content filter", "content policy"];
const UNAUTHORIZED: &[&str] = &[
    "unauthorized",
    "forbidden",
    "invalid api key",
    "authentication",
    "permission denied",
];
const MODEL_NOT_FOUND: &[&str] = &["model not found", "model_not_found"];
const RATE_LIMITED: &[&str] = &["rate limit", "ratelimit", "too many requests"];
const OVERLOADED: &[&str] = &["overloaded", "unavailable"];
const TIMEOUT: &[&str] = &["timeout", "timed out"];
const INVALID_REQUEST: &[&str] = &["invalid request", "invalid_request_error"];
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

/// The status decision table. `body` only refines statuses whose meaning the
/// provider qualifies in the body; it never moves a failure across the
/// server-error / client-error boundary.
fn classify_status(
    status: StatusCode,
    retry_after: Option<Duration>,
    body: &str,
    message: String,
) -> Error {
    let lower = body.to_lowercase();
    match status.as_u16() {
        429 if hits(&lower, USAGE_LIMIT) => Error::UsageLimit {
            message,
            reset_after: retry_after,
        },
        429 => Error::RateLimited {
            message,
            retry_after,
        },
        402 => Error::UsageLimit {
            message,
            reset_after: retry_after,
        },
        503 | 529 => Error::Overloaded {
            message,
            retry_after,
        },
        408 | 504 => Error::Timeout(message),
        401 | 403 if hits(&lower, LOGIN_REQUIRED) => Error::LoginRequired(message),
        401 | 403 => Error::Unauthorized(message),
        404 => Error::ModelNotFound(message),
        413 if hits(&lower, CONTENT_FILTER) => Error::ContentFiltered(message),
        413 => Error::ContextOverflow(message),
        _ if status.is_server_error() => Error::Provider(message),
        // 400, 422 and every other client rejection: the body names which
        // permanent rejection this is.
        _ if hits(&lower, CONTENT_FILTER) => Error::ContentFiltered(message),
        _ if hits(&lower, OVERFLOW) => Error::ContextOverflow(message),
        _ if hits(&lower, LOGIN_REQUIRED) => Error::LoginRequired(message),
        _ if hits(&lower, USAGE_LIMIT) => Error::UsageLimit {
            message,
            reset_after: retry_after,
        },
        _ => Error::InvalidRequest(message),
    }
}

/// Status codes an opaque, status-less description may name. Only these exact
/// standalone tokens are read as a status — never a digit run inside a request
/// id, a token count or a longer number.
const DESCRIBED_STATUSES: &[u16] = &[
    400, 401, 402, 403, 404, 408, 413, 422, 429, 500, 502, 503, 504, 529,
];

fn described_status(lower: &str) -> Option<StatusCode> {
    lower
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| token.len() == 3)
        .filter_map(|token| token.parse::<u16>().ok())
        .find(|code| DESCRIBED_STATUSES.contains(code))
        .and_then(|code| StatusCode::from_u16(code).ok())
}

/// Classify a failure that carries no HTTP status (the SDK surfaced only a
/// description). The first standalone status token in the description is read
/// as its status and decided by the same table as a real one; otherwise
/// lowercase markers decide, most-specific class first.
pub fn classify_error(message: &str) -> Error {
    let lower = message.to_lowercase();
    if let Some(status) = described_status(&lower) {
        return classify_status(status, None, message, message.to_string());
    }
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
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    use super::{WireFailure, classify_error, classify_genai_error, classify_wire_failure};

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| {
                (
                    HeaderName::from_static(name),
                    HeaderValue::from_static(value),
                )
            })
            .collect()
    }

    fn terminated() -> ModelExecutionEvidence {
        ModelExecutionEvidence::Terminated(TerminalReceipt::from_response_envelope(
            ResponseEnvelope::CompleteErrorBody,
        ))
    }

    #[test]
    fn wire_failure_status_decides_before_body_decision_table() {
        // Causes: HTTP status (or none), headers, body text, body complete.
        // Effects: E1 contract code, E2 retry-after/reset hint, E3 whether
        // policy may retry, E4 execution evidence. Rule: the status family
        // decides; body markers refine only 400/401/403/413/422/429;
        // Retry-After = retry-after-ms > delta-seconds > HTTP-date - Date.
        type Row = (
            &'static str,
            Option<u16>,
            &'static [(&'static str, &'static str)],
            &'static str,
            bool,
            &'static str,
            Option<Duration>,
            bool,
        );
        let rows: &[Row] = &[
            // Regression: a 5xx whose body quotes "413" is a retryable provider
            // failure, never a permanent context overflow.
            (
                "R1 500 quoting 413",
                Some(500),
                &[],
                r#"{"error":"upstream returned 413 after 413 bytes"}"#,
                true,
                "provider_error",
                None,
                true,
            ),
            (
                "R2 502 quoting overflow text",
                Some(502),
                &[],
                "request too large for upstream buffer",
                true,
                "provider_error",
                None,
                true,
            ),
            (
                "R3 429 delta-seconds",
                Some(429),
                &[("retry-after", "7")],
                "slow down",
                true,
                "rate_limited",
                Some(Duration::from_secs(7)),
                true,
            ),
            (
                "R4 429 HTTP-date against Date",
                Some(429),
                &[
                    ("retry-after", "Wed, 21 Oct 2026 07:28:30 GMT"),
                    ("date", "Wed, 21 Oct 2026 07:28:00 GMT"),
                ],
                "",
                true,
                "rate_limited",
                Some(Duration::from_secs(30)),
                true,
            ),
            (
                "R5 429 HTTP-date without Date",
                Some(429),
                &[("retry-after", "Wed, 21 Oct 2026 07:28:30 GMT")],
                "",
                true,
                "rate_limited",
                None,
                true,
            ),
            (
                "R6 429 HTTP-date already past",
                Some(429),
                &[
                    ("retry-after", "Wed, 21 Oct 2026 07:27:00 GMT"),
                    ("date", "Wed, 21 Oct 2026 07:28:00 GMT"),
                ],
                "",
                true,
                "rate_limited",
                Some(Duration::ZERO),
                true,
            ),
            (
                "R7 retry-after-ms wins",
                Some(429),
                &[("retry-after-ms", "1500"), ("retry-after", "2")],
                "",
                true,
                "rate_limited",
                Some(Duration::from_millis(1500)),
                true,
            ),
            (
                "R8 unparsable Retry-After",
                Some(429),
                &[("retry-after", "soon")],
                "",
                true,
                "rate_limited",
                None,
                true,
            ),
            (
                "R9 429 hard quota",
                Some(429),
                &[("retry-after", "3600")],
                r#"{"error":{"code":"insufficient_quota"}}"#,
                true,
                "usage_limit",
                Some(Duration::from_secs(3600)),
                false,
            ),
            (
                "R10 503",
                Some(503),
                &[("retry-after", "4")],
                "",
                true,
                "overloaded",
                Some(Duration::from_secs(4)),
                true,
            ),
            (
                "R11 529",
                Some(529),
                &[],
                "",
                true,
                "overloaded",
                None,
                true,
            ),
            ("R12 408", Some(408), &[], "", true, "timeout", None, true),
            ("R13 504", Some(504), &[], "", true, "timeout", None, true),
            (
                "R14 401",
                Some(401),
                &[],
                "bad key",
                true,
                "unauthorized",
                None,
                false,
            ),
            (
                "R15 401 expired grant",
                Some(401),
                &[],
                "invalid_grant",
                true,
                "login_required",
                None,
                false,
            ),
            (
                "R16 403",
                Some(403),
                &[],
                "",
                true,
                "unauthorized",
                None,
                false,
            ),
            (
                "R17 404",
                Some(404),
                &[],
                "",
                true,
                "model_not_found",
                None,
                false,
            ),
            (
                "R18 402",
                Some(402),
                &[],
                "",
                true,
                "usage_limit",
                None,
                false,
            ),
            (
                "R19 413 bare",
                Some(413),
                &[],
                "",
                true,
                "context_overflow",
                None,
                false,
            ),
            (
                "R20 400 overflow",
                Some(400),
                &[],
                "prompt is too long",
                true,
                "context_overflow",
                None,
                false,
            ),
            (
                "R21 400 content filter",
                Some(400),
                &[],
                r#"{"code":"content_filter"}"#,
                true,
                "content_filtered",
                None,
                false,
            ),
            (
                "R22 400 credit balance",
                Some(400),
                &[],
                "Your credit balance is too low",
                true,
                "usage_limit",
                None,
                false,
            ),
            (
                "R23 400 plain",
                Some(400),
                &[],
                "missing field",
                true,
                "invalid_request",
                None,
                false,
            ),
            (
                "R24 422 quoting 429",
                Some(422),
                &[],
                "field 429 invalid",
                true,
                "invalid_request",
                None,
                false,
            ),
            (
                "R25 no status, described 503",
                None,
                &[],
                "503 Service Unavailable",
                false,
                "overloaded",
                None,
                true,
            ),
            (
                "R26 no status, digits inside an id",
                None,
                &[],
                "connection reset, request id req_4130",
                false,
                "provider_error",
                None,
                true,
            ),
            (
                "R27 no status, timed out",
                None,
                &[],
                "operation timed out",
                false,
                "timeout",
                None,
                true,
            ),
            (
                "R28 incomplete body",
                Some(503),
                &[],
                "",
                false,
                "overloaded",
                None,
                true,
            ),
        ];
        for (row, status, header_pairs, body, complete, code, hint, retryable) in
            rows.iter().copied()
        {
            let header_map = headers(header_pairs);
            let error = classify_wire_failure(WireFailure {
                status: status.map(|code| StatusCode::from_u16(code).unwrap()),
                headers: Some(&header_map),
                body: body.into(),
                complete,
                unsent: false,
            });
            assert_eq!(error.code(), code, "{row}: E1");
            assert_eq!(
                error.retry_after().or(error.reset_hint()),
                hint,
                "{row}: E2"
            );
            assert_eq!(error.is_retryable(), retryable, "{row}: E3");
            let evidence = if complete && status.is_some() {
                terminated()
            } else {
                ModelExecutionEvidence::Unknown
            };
            assert_eq!(error.execution_evidence(), evidence, "{row}: E4");
        }
    }

    #[test]
    fn genai_failures_project_onto_the_wire_decision() {
        // Causes: C1 complete 503 body with Retry-After, C2 complete 307
        // body, C3 opaque status-less 503 text, C4 genai failed-body-read
        // sentinel, C5 webc status failure. Effects: E1 only complete
        // 4xx/5xx mints CompleteErrorBody; E2 the status and header decide
        // through the same table as the Responses adapter.
        let terminal = classify_genai_error(genai::Error::HttpError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            canonical_reason: "Service Unavailable".into(),
            body: r#"{"error":{"code":"service_unavailable_error"}}"#.into(),
            headers: Box::new(headers(&[("retry-after", "3")])),
        });
        assert_eq!(terminal.code(), "overloaded", "C1/E2");
        assert_eq!(
            terminal.retry_after(),
            Some(Duration::from_secs(3)),
            "C1/E2"
        );
        assert_eq!(terminal.execution_evidence(), terminated(), "C1/E1");

        let redirect = classify_genai_error(genai::Error::HttpError {
            status: StatusCode::TEMPORARY_REDIRECT,
            canonical_reason: "Temporary Redirect".into(),
            body: "{}".into(),
            headers: Default::default(),
        });
        assert_eq!(redirect.code(), "provider_error", "C2/E2");
        assert_eq!(
            redirect.execution_evidence(),
            ModelExecutionEvidence::Unknown,
            "C2/E1"
        );

        let opaque = classify_genai_error(genai::Error::Internal("503 Service Unavailable".into()));
        assert_eq!(opaque.code(), "overloaded", "C3/E2");
        assert_eq!(
            opaque.execution_evidence(),
            ModelExecutionEvidence::Unknown,
            "C3/E1"
        );

        let unread = classify_genai_error(genai::Error::HttpError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            canonical_reason: "Internal Server Error".into(),
            body: "Failed to read error body: 413".into(),
            headers: Default::default(),
        });
        assert_eq!(unread.code(), "provider_error", "C4/E2");
        assert_eq!(
            unread.execution_evidence(),
            ModelExecutionEvidence::Unknown,
            "C4/E1"
        );

        let webc = classify_genai_error(genai::Error::WebModelCall {
            model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "gpt-4o"),
            webc_error: genai::webc::Error::ResponseFailedStatus {
                status: StatusCode::TOO_MANY_REQUESTS,
                body: "rate limited".into(),
                headers: Box::new(headers(&[("retry-after-ms", "250")])),
            },
        });
        assert_eq!(webc.code(), "rate_limited", "C5/E2");
        assert_eq!(
            webc.retry_after(),
            Some(Duration::from_millis(250)),
            "C5/E2"
        );
        assert_eq!(webc.execution_evidence(), terminated(), "C5/E1");
    }

    #[test]
    fn provider_error_taxonomy_precedence_decision_table() {
        // Status-less descriptions. C1 hard usage exhaustion, C2 transient
        // rate limit, C3 login expiry, C4 plain authorization rejection.
        // R1=C1=>terminal usage_limit; R2=C2=>retryable rate_limited;
        // R3=C3=>login_required; R4=C4=>auth.
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
