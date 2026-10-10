//! The AI SDK protocol's Org admission encoding: which AI SDK routes consume
//! which Org bucket, and the plain-text HTTP shape this adapter answers every
//! refused request with (the same shape as its other request errors). The
//! bucket decision is the access edge's; a composition root mounts it over this
//! router with these functions.

use awaken_tenancy::org_admission::{OrgAdmissionDecision, OrgAdmissionUnavailable, OrgOperation};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// Starting a Run consumes the Create bucket; reading a Thread consumes the
/// Read bucket; an interrupt passes through unmetered.
#[must_use]
pub fn org_admission_class(method: &Method, path: &str) -> Option<(OrgOperation, &'static str)> {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match segments.as_slice() {
        ["v1", "ai-sdk", "chat"]
        | ["v1", "ai-sdk", "threads", _, "runs"]
        | ["v1", "ai-sdk", "agents", _, "runs"]
            if method == Method::POST =>
        {
            Some((OrgOperation::Create, "runs"))
        }
        ["v1", "ai-sdk", "threads", _, "messages"]
            if method == Method::GET || method == Method::HEAD =>
        {
            Some((OrgOperation::Read, "threads"))
        }
        _ => None,
    }
}

/// A metered request without an edge-authored Workspace scope.
#[must_use]
pub fn org_admission_scope_missing(_path: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "request has no trusted Workspace scope",
    )
        .into_response()
}

/// The Org limiter could not decide.
#[must_use]
pub fn org_admission_unavailable(_path: &str, error: &OrgAdmissionUnavailable) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response()
}

/// The Org bucket refused the request: `429` with `retry-after`.
#[must_use]
pub fn org_admission_rejected(
    _path: &str,
    operation: OrgOperation,
    decision: &OrgAdmissionDecision,
) -> Response {
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        format!("Org {operation:?} request limit exceeded"),
    )
        .into_response();
    if let Some(retry_after) = decision.retry_after {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(retry_after));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_sdk_routes_classify_into_org_buckets() {
        // | Rule | route | effect |
        // | S1 | POST chat / Thread runs / Agent runs | Create |
        // | S2 | GET/HEAD Thread messages | Read |
        // | S3 | DELETE Thread runs (interrupt), other paths | unmetered |
        for path in [
            "/v1/ai-sdk/chat",
            "/v1/ai-sdk/threads/t_1/runs",
            "/v1/ai-sdk/agents/a_1/runs",
        ] {
            assert_eq!(
                org_admission_class(&Method::POST, path),
                Some((OrgOperation::Create, "runs")),
                "S1 {path}"
            );
        }
        for method in [Method::GET, Method::HEAD] {
            assert_eq!(
                org_admission_class(&method, "/v1/ai-sdk/threads/t_1/messages"),
                Some((OrgOperation::Read, "threads")),
                "S2"
            );
        }
        assert_eq!(
            org_admission_class(&Method::DELETE, "/v1/ai-sdk/threads/t_1/runs"),
            None,
            "S3"
        );
        assert_eq!(org_admission_class(&Method::GET, "/v1/ag-ui/x"), None, "S3");
    }

    #[test]
    fn a_refused_request_keeps_the_adapter_plain_text_error_shape() {
        let decision = OrgAdmissionDecision {
            allowed: false,
            limit: 3,
            remaining: 0,
            retry_after: Some(20),
            reset_after: 60,
            reset_at_unix_ms: 0,
        };
        let response = org_admission_rejected("/", OrgOperation::Create, &decision);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "20");
        assert_eq!(
            org_admission_scope_missing("/").status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            org_admission_unavailable(
                "/",
                &OrgAdmissionUnavailable {
                    message: "down".into()
                }
            )
            .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
