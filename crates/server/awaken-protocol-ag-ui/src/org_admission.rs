//! The AG-UI protocol's Org admission encoding: which AG-UI routes consume
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
        ["v1", "ag-ui"] | ["v1", "ag-ui", "agents", _] if method == Method::POST => {
            Some((OrgOperation::Create, "runs"))
        }
        ["v1", "ag-ui", "threads", _, "messages"]
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
    fn ag_ui_routes_classify_into_org_buckets() {
        // | Rule | route | effect |
        // | S1 | POST run / Agent-scoped run | Create |
        // | S2 | GET/HEAD Thread messages | Read |
        // | S3 | DELETE Thread runs (interrupt), other paths | unmetered |
        for path in ["/v1/ag-ui", "/v1/ag-ui/agents/a_1"] {
            assert_eq!(
                org_admission_class(&Method::POST, path),
                Some((OrgOperation::Create, "runs")),
                "S1 {path}"
            );
        }
        for method in [Method::GET, Method::HEAD] {
            assert_eq!(
                org_admission_class(&method, "/v1/ag-ui/threads/t_1/messages"),
                Some((OrgOperation::Read, "threads")),
                "S2"
            );
        }
        assert_eq!(
            org_admission_class(&Method::DELETE, "/v1/ag-ui/threads/t_1/runs"),
            None,
            "S3"
        );
        assert_eq!(
            org_admission_class(&Method::GET, "/v1/ai-sdk/x"),
            None,
            "S3"
        );
    }

    #[tokio::test]
    async fn a_refused_request_keeps_the_adapter_plain_text_error_shape() {
        use http_body_util::BodyExt as _;

        // Causes: C1 missing trusted scope; C2 unavailable limiter; C3 refused
        // Create/Read bucket; C4 Retry-After present/absent. All errors retain
        // this adapter's plain-text body and Content-Type, not only its status.
        // | Rule | Cause       | Status | Body                         | Retry |
        // | E1   | C1          | 503    | exact missing-scope message  | none  |
        // | E2   | C2          | 503    | exact limiter error          | none  |
        // | E3   | C3 x C4     | 429    | exact operation-specific text| C4    |
        // E3 crosses both operations with both optional-header branches.
        let mut cases = vec![
            (
                org_admission_scope_missing("/v1/ag-ui"),
                StatusCode::SERVICE_UNAVAILABLE,
                None,
                "request has no trusted Workspace scope",
            ),
            (
                org_admission_unavailable(
                    "/v1/ag-ui",
                    &OrgAdmissionUnavailable {
                        message: "down".into(),
                    },
                ),
                StatusCode::SERVICE_UNAVAILABLE,
                None,
                "Org request limiter is unavailable: down",
            ),
        ];
        for (operation, body) in [
            (OrgOperation::Create, "Org Create request limit exceeded"),
            (OrgOperation::Read, "Org Read request limit exceeded"),
        ] {
            for retry_after in [Some(20), None] {
                let decision = OrgAdmissionDecision {
                    allowed: false,
                    limit: 3,
                    remaining: 0,
                    retry_after,
                    reset_after: 60,
                    reset_at_unix_ms: 0,
                };
                cases.push((
                    org_admission_rejected("/v1/ag-ui", operation, &decision),
                    StatusCode::TOO_MANY_REQUESTS,
                    retry_after.map(|_| "20"),
                    body,
                ));
            }
        }
        for (response, status, retry_after, body) in cases {
            assert_eq!(response.status(), status, "{body}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "text/plain; charset=utf-8",
                "{body}"
            );
            assert_eq!(
                response
                    .headers()
                    .get(header::RETRY_AFTER)
                    .map(|value| value.to_str().unwrap()),
                retry_after,
                "{body}",
            );
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(bytes.as_ref(), body.as_bytes(), "exact public error body");
        }
    }
}
