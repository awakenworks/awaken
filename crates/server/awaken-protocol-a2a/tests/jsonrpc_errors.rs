//! A2A JSON-RPC error-envelope conformance over the real router: unknown methods,
//! invalid params, malformed bodies, and one happy `message/send` — driven through
//! axum with a no-op runtime (the error paths never reach it).

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_protocol_a2a::router;
use awaken_protocol_transport::{
    DriverError, Pending, ProtocolRuntime, Resume, StepOutcome, Terminal,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

struct NoopRuntime;

#[async_trait]
impl ProtocolRuntime for NoopRuntime {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, DriverError> {
        Ok(StepOutcome {
            new_messages: Vec::new(),
            terminal: Terminal::Finished,
        })
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _resume: Resume,
    ) -> Result<StepOutcome, DriverError> {
        unreachable!("resume is not exercised by these error-path tests")
    }

    async fn pending(&self, _thread: &str) -> Option<Pending> {
        None
    }

    async fn history(&self, _thread: &str) -> Vec<Message> {
        Vec::new()
    }

    fn model(&self) -> String {
        "test".into()
    }
}

async fn post(raw: String) -> (StatusCode, Value) {
    let app = router(Arc::new(NoopRuntime));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/a2a")
                .header("content-type", "application/json")
                .body(Body::from(raw))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn rpc(body: Value) -> Value {
    post(body.to_string()).await.1
}

#[tokio::test]
async fn unknown_method_is_method_not_found() {
    let r = rpc(json!({ "jsonrpc": "2.0", "id": 1, "method": "tasks/noSuchMethod", "params": {} }))
        .await;
    assert_eq!(r["error"]["code"], -32601, "{r}");
    assert_eq!(r["id"], 1);
}

#[tokio::test]
async fn tasks_get_rejects_an_unknown_task() {
    let r = rpc(json!({ "jsonrpc": "2.0", "id": 5, "method": "tasks/get", "params": { "id": "task-ctxA" } })).await;
    assert_eq!(r["id"], 5);
    assert_eq!(r["error"]["code"], -32001, "{r}");
}

#[tokio::test]
async fn tasks_get_without_an_id_is_invalid_params() {
    let r = rpc(json!({ "jsonrpc": "2.0", "id": 6, "method": "tasks/get", "params": {} })).await;
    assert_eq!(r["error"]["code"], -32602, "{r}");
}

#[tokio::test]
async fn message_send_with_missing_message_is_invalid_params() {
    let r = rpc(json!({ "jsonrpc": "2.0", "id": 2, "method": "message/send", "params": {} })).await;
    assert_eq!(r["error"]["code"], -32602, "{r}");
    assert_eq!(r["id"], 2);
}

#[tokio::test]
async fn a_well_formed_message_send_returns_a_task_result() {
    let r = rpc(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "message/send",
        "params": {
            "message": {
                "messageId": "m1",
                "contextId": "c1",
                "role": "user",
                "parts": [{ "kind": "text", "text": "hi" }]
            }
        }
    }))
    .await;
    assert!(r["result"].is_object(), "expected a task result: {r}");
    assert_eq!(r["id"], 3);
}

#[tokio::test]
async fn a_malformed_body_is_rejected_with_a_bad_request_envelope() {
    let (status, body) = post("{ not json".into()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], -32600, "{body}");
}

#[tokio::test]
async fn the_response_envelope_echoes_the_jsonrpc_marker_and_a_string_id() {
    let r =
        rpc(json!({ "jsonrpc": "2.0", "id": "abc-1", "method": "message/stream", "params": {} }))
            .await;
    assert_eq!(r["jsonrpc"], "2.0", "{r}");
    assert_eq!(r["id"], "abc-1");
    assert!(r["error"].is_object());
}

#[tokio::test]
async fn a_request_without_an_id_echoes_a_null_id() {
    let r = rpc(json!({ "jsonrpc": "2.0", "method": "message/stream", "params": {} })).await;
    assert_eq!(r["id"], Value::Null, "{r}");
}

#[tokio::test]
async fn tasks_cancel_without_an_id_is_invalid_params() {
    // Symmetric with `tasks/get`: a cancel that names no task `id` is a caller
    // fault (-32602), never a silent no-op.
    let r = rpc(json!({ "jsonrpc": "2.0", "id": 7, "method": "tasks/cancel", "params": {} })).await;
    assert_eq!(r["error"]["code"], -32602, "{r}");
    assert_eq!(r["id"], 7);
}

#[tokio::test]
async fn tasks_cancel_rejects_an_unknown_task() {
    let r = rpc(
        json!({ "jsonrpc": "2.0", "id": 8, "method": "tasks/cancel", "params": { "id": "task-idle" } }),
    )
    .await;
    assert_eq!(r["error"]["code"], -32001, "{r}");
}

/// A runtime whose fresh turn always faults with the configured driver error, so
/// the JSON-RPC `message/send` error-mapping (`rpc_fault`) is exercised end to end.
struct FaultingRuntime {
    internal: bool,
}

#[async_trait]
impl ProtocolRuntime for FaultingRuntime {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, DriverError> {
        Err(if self.internal {
            DriverError::Internal("upstream is down".into())
        } else {
            DriverError::BadRequest("empty message".into())
        })
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _resume: Resume,
    ) -> Result<StepOutcome, DriverError> {
        unreachable!()
    }

    async fn pending(&self, _thread: &str) -> Option<Pending> {
        None
    }

    async fn history(&self, _thread: &str) -> Vec<Message> {
        Vec::new()
    }

    fn model(&self) -> String {
        "test".into()
    }
}

async fn rpc_on(rt: Arc<dyn ProtocolRuntime>, body: Value) -> Value {
    let app = router(rt);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/a2a")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn send_body(id: i64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "message/send",
        "params": { "message": { "messageId": "m1", "contextId": "c", "role": "user", "parts": [{ "kind": "text", "text": "hi" }] } }
    })
}

#[tokio::test]
async fn message_send_internal_fault_maps_to_jsonrpc_internal_error() {
    // A2A `message/send` succeeded as a transport call but the run faulted: the
    // internal driver fault becomes JSON-RPC -32603 inside the 200 envelope (not
    // an HTTP 500), and echoes the request id.
    let r = rpc_on(Arc::new(FaultingRuntime { internal: true }), send_body(11)).await;
    assert_eq!(r["error"]["code"], -32603, "{r}");
    assert_eq!(r["id"], 11);
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("upstream is down"),
        "{r}"
    );
    assert!(r.get("result").is_none(), "a fault carries no result: {r}");
}

#[tokio::test]
async fn message_send_bad_request_fault_maps_to_jsonrpc_invalid_params() {
    // A caller-fault driver error maps to JSON-RPC -32602 (invalid params).
    let r = rpc_on(Arc::new(FaultingRuntime { internal: false }), send_body(12)).await;
    assert_eq!(r["error"]["code"], -32602, "{r}");
    assert_eq!(r["id"], 12);
}

#[tokio::test]
async fn an_oversized_body_is_bounded_not_oom() {
    // A body past the request-body limit is refused (413, or the A2A 400 envelope
    // when the custom extractor maps the length-limit rejection) — never buffered
    // unboundedly.
    let pad = "x".repeat(3 * 1024 * 1024);
    let raw =
        format!(r#"{{"jsonrpc":"2.0","id":1,"method":"message/send","params":{{"pad":"{pad}"}}}}"#);
    let (status, _) = post(raw).await;
    assert!(
        status == StatusCode::PAYLOAD_TOO_LARGE || status == StatusCode::BAD_REQUEST,
        "an oversized body must be refused, got {status}"
    );
}
