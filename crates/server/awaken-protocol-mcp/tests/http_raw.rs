//! Raw Streamable-HTTP transport behavior for the MCP router (driven through axum
//! with tower, not the handshake client): malformed/non-JSON-RPC bodies, the
//! notification ack, the initialize session header, stale-session 404, and the
//! GET bearer guard.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_protocol_mcp::{McpExportedTool, McpHttpConfig, McpToolService, StaticExports, router};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

struct EchoTool;

#[async_trait]
impl RawTool for EchoTool {
    fn id(&self) -> &str {
        "echo"
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok(call.call_id, "ok"))
    }
}

fn app(bearer: Option<&str>) -> axum::Router {
    let source = StaticExports::new(vec![McpExportedTool::plain(
        ToolDescriptor::pinned("test", "echo", "echoes", json!({ "type": "object" })),
        Arc::new(EchoTool),
    )]);
    let service = Arc::new(McpToolService::new("raw-test", "0.0.0", Arc::new(source)));
    router(
        service,
        McpHttpConfig {
            path: "/mcp".to_string(),
            bearer_token: bearer.map(str::to_string),
        },
    )
}

fn post(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn malformed_json_is_bad_request() {
    let resp = app(None).oneshot(post("{ not json")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_body_without_a_method_is_not_a_jsonrpc_message() {
    let resp = app(None)
        .oneshot(post(r#"{"jsonrpc":"2.0","foo":1}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_notification_is_accepted_with_202() {
    // method, no id → a notification: acknowledged, no body.
    let resp = app(None)
        .oneshot(post(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn initialize_issues_a_session_header() {
    let resp = app(None)
        .oneshot(post(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers().contains_key("Mcp-Session-Id"),
        "initialize must issue a session id header"
    );
}

#[tokio::test]
async fn a_call_on_an_unknown_session_is_not_found() {
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .header("Mcp-Session-Id", "never-opened")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#.to_string(),
        ))
        .unwrap();
    let resp = app(None).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_tears_down_the_session() {
    let app = app(None);
    // Open a session.
    let init = app
        .clone()
        .oneshot(post(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        ))
        .await
        .unwrap();
    let session = init
        .headers()
        .get("Mcp-Session-Id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Delete it → 204.
    let del = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/mcp")
                .header("Mcp-Session-Id", &session)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del.status(), StatusCode::NO_CONTENT);

    // A call on the torn-down session is now a stale-session 404.
    let call = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .header("Mcp-Session-Id", &session)
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#.to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(call.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_get_stream_enforces_the_bearer() {
    let req = Request::builder()
        .method("GET")
        .uri("/mcp")
        .body(Body::empty())
        .unwrap();
    let resp = app(Some("secret")).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_post_without_the_bearer_is_unauthorized() {
    // The bearer guard runs before dispatch: an unauthenticated `tools/list`
    // POST is refused (401 + challenge), never executed.
    let resp = app(Some("secret"))
        .oneshot(post(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        resp.headers().contains_key("www-authenticate"),
        "a 401 carries the bearer challenge the client's refresh path consumes"
    );
}

#[tokio::test]
async fn a_delete_without_the_bearer_is_unauthorized() {
    // DELETE (session teardown) is guarded too — a caller cannot end a session
    // without presenting the bearer.
    let req = Request::builder()
        .method("DELETE")
        .uri("/mcp")
        .header("Mcp-Session-Id", "whatever")
        .body(Body::empty())
        .unwrap();
    let resp = app(Some("secret")).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_oversized_body_is_rejected() {
    // A POST body beyond axum's default request-body limit (2 MiB) is refused by
    // the extractor before dispatch, bounding memory per request.
    let big = "x".repeat(3 * 1024 * 1024);
    let resp = app(None).oneshot(post(&big)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}
