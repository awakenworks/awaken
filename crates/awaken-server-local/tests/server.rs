//! Server integration tests: drive the Managed Agents surface through the *real*
//! kernel (not a fake runtime). Proves the echo path end-to-end and that a
//! built-in hand tool (`read`) actually executes in-process under the runtime.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_server_local::{EchoModel, build_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn json_call(
    app: &Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(serde_json::to_vec(&body).unwrap())
        })
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{method} {uri}");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn event_types(list: &serde_json::Value) -> Vec<String> {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn echo_turn_end_to_end() {
    let app = build_router(Arc::new(EchoModel), "echo-model");
    let session = json_call(
        &app,
        "POST",
        "/v1/sessions",
        serde_json::json!({ "agent": "assistant" }),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_string();

    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi there" }] }] }),
    )
    .await;

    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        event_types(&list),
        vec!["agent.message", "session.status_idle"]
    );
    let msg = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.message")
        .unwrap();
    assert_eq!(msg["content"][0]["text"], "Echo: hi there");
    let idle = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "session.status_idle")
        .unwrap();
    assert_eq!(idle["stop_reason"]["type"], "end_turn");
}

/// A model that reads one file then replies — proves the `read` built-in tool
/// runs in-process through the real runtime and its result reaches the wire.
struct ReadThenReply {
    path: String,
    step: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for ReadThenReply {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let output = if self.step.fetch_add(1, Ordering::SeqCst) == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "r1".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({ "path": self.path }),
            }])
        } else {
            AssistantOutput::text("done")
        };
        Ok(ChatResponse {
            output,
            usage: None,
        })
    }
}

#[tokio::test]
async fn builtin_read_tool_runs_in_process() {
    let path = std::env::temp_dir().join("awaken_m1_read_probe.txt");
    std::fs::write(&path, "SANDBOX_PROBE_CONTENT").unwrap();

    let model = ReadThenReply {
        path: path.to_string_lossy().to_string(),
        step: AtomicUsize::new(0),
    };
    let app = build_router(Arc::new(model), "scripted");
    let session = json_call(
        &app,
        "POST",
        "/v1/sessions",
        serde_json::json!({ "agent": "assistant" }),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_string();

    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "read it" }] }] }),
    )
    .await;

    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        event_types(&list),
        vec![
            "agent.tool_use",
            "agent.tool_result",
            "agent.message",
            "session.status_idle"
        ]
    );
    // The tool actually ran: its result carries the file's content.
    let result = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.tool_result")
        .unwrap();
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("SANDBOX_PROBE_CONTENT"),
        "tool_result was: {text}"
    );

    let _ = std::fs::remove_file(&path);
}
