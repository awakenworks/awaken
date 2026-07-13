//! Fail-closed resume over the real AI SDK router: a tool decision delivered when
//! no run is parked yields an `error` stream frame, not a silent success.

use std::sync::Arc;

use awaken_agent_contract::agent::message::Message;
use awaken_protocol_transport::{DriverError, Pending, ProtocolRuntime, Resume, StepOutcome};
use axum::body::{Body, to_bytes};
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt;

struct NoParkRuntime;

#[async_trait::async_trait]
impl ProtocolRuntime for NoParkRuntime {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, DriverError> {
        Ok(StepOutcome {
            new_messages: Vec::new(),
            waiting: false,
            exhausted: false,
            pending: None,
        })
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _resume: Resume,
    ) -> Result<StepOutcome, DriverError> {
        unreachable!("nothing is parked, so resume must never be reached")
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

async fn frames(body: Value) -> Vec<Value> {
    frames_at("/v1/ai-sdk/chat", body).await
}

async fn frames_at(uri: &str, body: Value) -> Vec<Value> {
    let app = awaken_protocol_ai_sdk::router::router(Arc::new(NoParkRuntime));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    text.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect()
}

#[tokio::test]
async fn a_tool_decision_with_no_parked_run_yields_an_error_frame() {
    // Resume-only input (an assistant tool decision, no new user turn) with nothing parked.
    let frames = frames(json!({
        "threadId": "t1",
        "messages": [{
            "id": "a1",
            "role": "assistant",
            "parts": [{
                "type": "tool-probe",
                "toolCallId": "c1",
                "state": "output-available",
                "output": "sneaky"
            }]
        }]
    }))
    .await;
    let types: Vec<&str> = frames
        .iter()
        .map(|f| f["type"].as_str().unwrap_or_default())
        .collect();
    assert!(
        types.contains(&"error"),
        "a stray tool decision must fail closed with an error frame: {types:?}"
    );
}

fn user_turn() -> serde_json::Value {
    json!({ "messages": [{ "id": "u1", "role": "user", "parts": [{ "type": "text", "text": "go" }] }] })
}

#[tokio::test]
async fn the_thread_scoped_run_route_streams_a_turn() {
    let frames = frames_at("/v1/ai-sdk/threads/t9/runs", user_turn()).await;
    assert!(
        frames.iter().any(|f| f["type"] == "finish"),
        "thread-scoped run should stream to a finish: {frames:?}"
    );
}

#[tokio::test]
async fn the_agent_scoped_run_route_streams_a_turn() {
    let frames = frames_at("/v1/ai-sdk/agents/coder/runs", user_turn()).await;
    assert!(
        frames.iter().any(|f| f["type"] == "finish"),
        "agent-scoped run should stream to a finish: {frames:?}"
    );
}

/// A runtime parked on a built-in tool `c1`; resume drives it to completion.
struct ParkedRuntime;

#[async_trait::async_trait]
impl ProtocolRuntime for ParkedRuntime {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, DriverError> {
        unreachable!("this test only drives the resume path")
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _resume: Resume,
    ) -> Result<StepOutcome, DriverError> {
        use awaken_agent_contract::agent::message::{Id, Role};
        Ok(StepOutcome {
            new_messages: vec![Message::text(Id("a1".into()), Role::Assistant, "done")],
            waiting: false,
            exhausted: false,
            pending: None,
        })
    }

    async fn pending(&self, _thread: &str) -> Option<Pending> {
        Some(Pending {
            tool_use_id: "c1".into(),
            name: "write".into(),
            input: serde_json::Value::Null,
            client_executed: false,
        })
    }

    async fn history(&self, _thread: &str) -> Vec<Message> {
        Vec::new()
    }

    fn model(&self) -> String {
        "test".into()
    }
}

#[tokio::test]
async fn a_matching_tool_decision_resumes_a_parked_run_to_a_finish() {
    let app = awaken_protocol_ai_sdk::router::router(Arc::new(ParkedRuntime));
    let body = json!({
        "threadId": "t1",
        "messages": [{
            "id": "a1",
            "role": "assistant",
            "parts": [{ "type": "tool-write", "toolCallId": "c1", "state": "output-available", "output": "ok" }]
        }]
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/chat")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let types: Vec<String> = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter_map(|f| f["type"].as_str().map(str::to_string))
        .collect();
    assert!(types.contains(&"finish".to_string()), "{types:?}");
    assert!(!types.contains(&"error".to_string()), "{types:?}");
}

/// A runtime whose turn panics, so the spawned turn task dies (JoinError).
struct PanickingRuntime;

#[async_trait::async_trait]
impl ProtocolRuntime for PanickingRuntime {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, DriverError> {
        panic!("the turn task died");
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

#[tokio::test]
async fn a_dead_turn_task_surfaces_an_error_frame_not_a_hang() {
    // The turn runs on its own task; if it dies (panic/cancel → JoinError) the
    // stream must still close with an `error` frame rather than hanging.
    let app = awaken_protocol_ai_sdk::router::router(Arc::new(PanickingRuntime));
    let body = json!({ "messages": [{ "id": "u1", "role": "user", "parts": [{ "type": "text", "text": "go" }] }] });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/chat")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let has_error = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .any(|f| f["type"] == "error");
    assert!(
        has_error,
        "a dead turn task must surface an error frame: {text}"
    );
}
