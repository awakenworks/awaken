//! The A2A protocol's Org admission encoding: which A2A routes consume which
//! Org bucket, and the A2A error envelope every refused request is answered
//! with. A JSON-RPC call is refused before its body is read, so it is metered
//! as a Create (it may send a message) and answered with a JSON-RPC error whose
//! `id` is `null`; every other binding answers the `{code, message}` fault. The
//! bucket decision is the access edge's; a composition root mounts it over this
//! router with these functions.

use awaken_runtime_contract::runs::request::{
    OrgAdmissionDecision, OrgAdmissionUnavailable, OrgOperation,
};
use axum::Json;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::router::JSONRPC_PATH;

/// The JSON-RPC server-error code for an edge refusal; A2A reserves
/// -32001..-32009 for its own errors and defines none for request limits.
const ORG_ADMISSION_REFUSED: i32 = -32000;

/// Sending a message (any binding, tenant-scoped or not) and every JSON-RPC
/// call consume the Create bucket; reads consume the Read bucket; task
/// cancellation and push-configuration writes pass through unmetered.
#[must_use]
pub fn org_admission_class(method: &Method, path: &str) -> Option<(OrgOperation, &'static str)> {
    if method == Method::GET || method == Method::HEAD {
        return Some((OrgOperation::Read, "tasks"));
    }
    if method != Method::POST {
        return None;
    }
    let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    (path == JSONRPC_PATH || last == "message:send" || last == "message:stream")
        .then_some((OrgOperation::Create, "messages"))
}

fn refusal(status: StatusCode, json_rpc: bool, message: String) -> Response {
    if json_rpc {
        (
            status,
            Json(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": ORG_ADMISSION_REFUSED, "message": message }
            })),
        )
            .into_response()
    } else {
        (
            status,
            Json(json!({ "code": ORG_ADMISSION_REFUSED, "message": message })),
        )
            .into_response()
    }
}

/// A metered request without an edge-authored Workspace scope.
#[must_use]
pub fn org_admission_scope_missing(path: &str) -> Response {
    refusal(
        StatusCode::SERVICE_UNAVAILABLE,
        path == JSONRPC_PATH,
        "request has no trusted Workspace scope".into(),
    )
}

/// The Org limiter could not decide.
#[must_use]
pub fn org_admission_unavailable(path: &str, error: &OrgAdmissionUnavailable) -> Response {
    refusal(
        StatusCode::SERVICE_UNAVAILABLE,
        path == JSONRPC_PATH,
        error.to_string(),
    )
}

/// The Org bucket refused the request: `429` with `retry-after`, in the
/// JSON-RPC envelope on the JSON-RPC endpoint.
#[must_use]
pub fn org_admission_rejected(
    path: &str,
    operation: OrgOperation,
    decision: &OrgAdmissionDecision,
) -> Response {
    let mut response = refusal(
        StatusCode::TOO_MANY_REQUESTS,
        path == JSONRPC_PATH,
        format!("Org {operation:?} request limit exceeded"),
    );
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

    #[tokio::test]
    async fn a2a_routes_classify_into_org_buckets_and_refuse_in_their_binding() {
        // | Rule | route | effect |
        // | A1 | POST JSON-RPC endpoint | Create; refusal is a JSON-RPC error, id null |
        // | A2 | POST message:send / message:stream (any binding, tenant) | Create; {code,message} fault |
        // | A3 | GET/HEAD anything | Read |
        // | A4 | POST cancel / push config, DELETE | unmetered |
        use http_body_util::BodyExt as _;
        assert_eq!(
            org_admission_class(&Method::POST, JSONRPC_PATH),
            Some((OrgOperation::Create, "messages")),
            "A1"
        );
        for path in [
            "/v1/a2a/message:send",
            "/v1/a2a/message:stream",
            "/v1/message:send",
            "/message:stream",
            "/tenant_1/message:send",
            "/v1/a2a/agents/a_1/message:stream",
        ] {
            assert_eq!(
                org_admission_class(&Method::POST, path),
                Some((OrgOperation::Create, "messages")),
                "A2 {path}"
            );
        }
        for method in [Method::GET, Method::HEAD] {
            assert_eq!(
                org_admission_class(&method, "/v1/a2a/tasks/t_1"),
                Some((OrgOperation::Read, "tasks")),
                "A3"
            );
        }
        for (method, path) in [
            (Method::POST, "/v1/a2a/tasks/t_1/cancel"),
            (Method::POST, "/v1/a2a/tasks/t_1/pushNotificationConfigs"),
            (
                Method::DELETE,
                "/v1/a2a/tasks/t_1/pushNotificationConfigs/c_1",
            ),
        ] {
            assert_eq!(org_admission_class(&method, path), None, "A4 {path}");
        }
        let decision = OrgAdmissionDecision {
            allowed: false,
            limit: 3,
            remaining: 0,
            retry_after: Some(20),
            reset_after: 60,
            reset_at_unix_ms: 0,
        };
        let rpc = org_admission_rejected(JSONRPC_PATH, OrgOperation::Create, &decision);
        assert_eq!(rpc.status(), StatusCode::TOO_MANY_REQUESTS, "A1");
        assert_eq!(rpc.headers()[header::RETRY_AFTER], "20", "A1");
        let body: serde_json::Value =
            serde_json::from_slice(&rpc.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body["jsonrpc"], "2.0", "A1");
        assert!(body["id"].is_null(), "A1");
        assert_eq!(body["error"]["code"], ORG_ADMISSION_REFUSED, "A1");
        let rest = org_admission_rejected("/v1/message:send", OrgOperation::Create, &decision);
        let body: serde_json::Value =
            serde_json::from_slice(&rest.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body["code"], ORG_ADMISSION_REFUSED, "A2");
        assert!(body.get("jsonrpc").is_none(), "A2");
    }
}
