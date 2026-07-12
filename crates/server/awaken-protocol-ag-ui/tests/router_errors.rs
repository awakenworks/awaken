//! Fail-closed resume over the real AG-UI router: a tool result delivered when no
//! run is parked must produce a RUN_ERROR, not a silent success.

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
    async fn run_turn(
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
        unreachable!("no run is parked, so resume must never be reached")
    }

    // No run is ever parked.
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
    let app = awaken_protocol_ag_ui::router::router(Arc::new(NoParkRuntime));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ag-ui")
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
        .map(|d| serde_json::from_str(d).unwrap())
        .collect()
}

#[tokio::test]
async fn a_tool_result_with_no_parked_run_fails_closed_with_run_error() {
    // A resume-only input (only a tool result, no new user turn) with nothing parked.
    let frames = frames(json!({
        "threadId": "t1",
        "runId": "r1",
        "messages": [{ "role": "tool", "toolCallId": "c1", "content": "sneaky result" }],
    }))
    .await;
    let types: Vec<&str> = frames
        .iter()
        .map(|f| f["type"].as_str().unwrap_or_default())
        .collect();
    assert!(
        types.contains(&"RUN_ERROR"),
        "a stray tool result must fail closed: {types:?}"
    );
    assert!(
        !types.contains(&"RUN_FINISHED"),
        "a fail-closed resume must not report a finished run: {types:?}"
    );
}

/// A runtime parked on the built-in tool `c1`; resume drives it to completion.
struct ParkedRuntime;

#[async_trait::async_trait]
impl ProtocolRuntime for ParkedRuntime {
    async fn run_turn(
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
            input: Value::Null,
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
async fn a_matching_tool_result_resumes_a_parked_run_to_completion() {
    let app = awaken_protocol_ag_ui::router::router(Arc::new(ParkedRuntime));
    let body = json!({
        "threadId": "t1",
        "runId": "r1",
        "messages": [{ "role": "tool", "toolCallId": "c1", "content": "approved" }],
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ag-ui")
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
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter_map(|f| f["type"].as_str().map(str::to_string))
        .collect();
    assert!(types.contains(&"RUN_FINISHED".to_string()), "{types:?}");
    assert!(!types.contains(&"RUN_ERROR".to_string()), "{types:?}");
}

#[tokio::test]
async fn the_scoped_agent_route_streams_a_fresh_turn() {
    let app = awaken_protocol_ag_ui::router::router(Arc::new(NoParkRuntime));
    let body = json!({ "threadId": "t1", "runId": "r1", "messages": [{ "role": "user", "content": "go" }] });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ag-ui/agents/coder")
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
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter_map(|f| f["type"].as_str().map(str::to_string))
        .collect();
    assert!(types.contains(&"RUN_FINISHED".to_string()), "{types:?}");
}

use std::sync::atomic::{AtomicBool, Ordering};

/// A runtime parked on a built-in tool that records whether it was resumed denied.
struct DenyRecordingRuntime {
    denied: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl ProtocolRuntime for DenyRecordingRuntime {
    async fn run_turn(
        &self,
        _t: &str,
        _a: Option<String>,
        _m: Vec<Message>,
    ) -> Result<StepOutcome, DriverError> {
        unreachable!()
    }
    async fn resume(
        &self,
        _t: &str,
        _id: &str,
        resume: Resume,
    ) -> Result<StepOutcome, DriverError> {
        if let Resume::Confirm { allow: false, .. } = resume {
            self.denied.store(true, Ordering::SeqCst);
        }
        Ok(StepOutcome {
            new_messages: Vec::new(),
            waiting: false,
            exhausted: false,
            pending: None,
        })
    }
    async fn pending(&self, _t: &str) -> Option<Pending> {
        Some(Pending {
            tool_use_id: "c1".into(),
            name: "write".into(),
            input: Value::Null,
            client_executed: false,
        })
    }
    async fn history(&self, _t: &str) -> Vec<Message> {
        Vec::new()
    }
    fn model(&self) -> String {
        "test".into()
    }
}

#[tokio::test]
async fn an_error_flagged_tool_result_denies_a_parked_builtin_tool() {
    let denied = Arc::new(AtomicBool::new(false));
    let app = awaken_protocol_ag_ui::router::router(Arc::new(DenyRecordingRuntime {
        denied: denied.clone(),
    }));
    let body = json!({
        "threadId": "t1",
        "runId": "r1",
        "messages": [{ "role": "tool", "toolCallId": "c1", "content": "", "error": "not permitted" }],
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ag-ui")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(
        denied.load(Ordering::SeqCst),
        "an error-flagged tool result must deny the parked built-in tool (allow:false)"
    );
}

/// A runtime whose turn panics, so the spawned turn task dies (JoinError).
struct AgUiPanickingRuntime;

#[async_trait::async_trait]
impl ProtocolRuntime for AgUiPanickingRuntime {
    async fn run_turn(
        &self,
        _t: &str,
        _a: Option<String>,
        _m: Vec<Message>,
    ) -> Result<StepOutcome, DriverError> {
        panic!("the turn task died");
    }
    async fn resume(&self, _t: &str, _id: &str, _r: Resume) -> Result<StepOutcome, DriverError> {
        unreachable!()
    }
    async fn pending(&self, _t: &str) -> Option<Pending> {
        None
    }
    async fn history(&self, _t: &str) -> Vec<Message> {
        Vec::new()
    }
    fn model(&self) -> String {
        "test".into()
    }
}

async fn ag_ui_frame_types(runtime: Arc<dyn ProtocolRuntime>, body: String) -> Vec<String> {
    let response = awaken_protocol_ag_ui::router::router(runtime)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ag-ui")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec())
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter_map(|f| f["type"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn a_dead_turn_task_surfaces_a_run_error_not_a_hang() {
    let app = awaken_protocol_ag_ui::router::router(Arc::new(AgUiPanickingRuntime));
    let body = json!({ "threadId": "t1", "runId": "r1", "messages": [{ "role": "user", "content": "go" }] });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ag-ui")
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
        .any(|f| f["type"] == "RUN_ERROR");
    assert!(has_error, "a dead turn task must surface RUN_ERROR: {text}");
}

#[tokio::test]
async fn an_oversized_body_is_refused_in_stream() {
    // AG-UI surfaces the request-body-limit rejection as an in-stream RUN_ERROR
    // (its streaming errors are events, not HTTP status) — the body is bounded, and
    // the run does not complete.
    let big = "x".repeat(3 * 1024 * 1024);
    let body = format!(r#"{{"threadId":"t1","messages":[{{"role":"user","content":"{big}"}}]}}"#);
    let types = ag_ui_frame_types(Arc::new(NoParkRuntime), body).await;
    assert!(types.contains(&"RUN_ERROR".to_string()), "{types:?}");
    assert!(!types.contains(&"RUN_FINISHED".to_string()), "{types:?}");
}
