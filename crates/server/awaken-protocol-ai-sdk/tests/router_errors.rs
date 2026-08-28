//! Fail-closed resume over the real AI SDK router: a tool decision delivered when
//! no run is awaiting yields an `error` stream frame, not a silent success.

use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_session_contract::{
    Pending, RunApplication, RunApplicationError, RunResume, StepOutcome, blocks_text,
};
use axum::body::{Body, to_bytes};
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt;

struct NoAwaitingRuntime;

#[async_trait::async_trait]
impl RunApplication for NoAwaitingRuntime {
    async fn run(
        &self,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    async fn resume(
        &self,
        _operation_id: &str,
        _thread: &str,
        _tool_use_id: &str,
        _resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        unreachable!("nothing is awaiting, so resume must never be reached")
    }

    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunApplicationError> {
        Ok(None)
    }

    async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunApplicationError> {
        Ok(Vec::new())
    }

    fn model(&self) -> String {
        "test".into()
    }
}

#[derive(Default)]
struct ReplayRecordingRuntime {
    runs: Mutex<Vec<(String, Vec<Message>)>>,
}

#[async_trait::async_trait]
impl RunApplication for ReplayRecordingRuntime {
    async fn run(
        &self,
        operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.runs
            .lock()
            .unwrap()
            .push((operation_id.to_string(), messages));
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    async fn resume(
        &self,
        _operation_id: &str,
        _thread: &str,
        _tool_use_id: &str,
        _resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        unreachable!("a known turn replay is a Run admission, not a tool resume")
    }

    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunApplicationError> {
        Ok(None)
    }

    async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunApplicationError> {
        Ok(vec![Message::text(Id("u1".into()), Role::User, "original")])
    }

    fn model(&self) -> String {
        "replay-recording".into()
    }
}

async fn frames(body: Value) -> Vec<Value> {
    frames_at("/v1/ai-sdk/chat", body).await
}

async fn frames_at(uri: &str, body: Value) -> Vec<Value> {
    let app = awaken_protocol_ai_sdk::router::router(Arc::new(NoAwaitingRuntime));
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
async fn a_tool_decision_with_no_awaiting_run_yields_an_error_frame() {
    // RunResume-only input (an assistant tool decision, no new user turn) with nothing awaiting.
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

#[tokio::test]
async fn a_known_operation_replays_the_run_while_history_only_fails_closed() {
    // Router classification decision table. C1 the request's current operation
    // is a known user input; C2 a tool decision is present. Effects: E1 C1
    // forwards the same operation/message to the existing Run authority; E2
    // !C1+!C2 rejects without Run or resume; E3 !C1+C2 uses resume. Constraint:
    // committed-history dedup may remove only older history, never the current
    // operation whose durable equality check is downstream. Rules: R1=C1=>E1;
    // R2=!C1+!C2=>E2; R3=!C1+C2=>E3 (the adjacent resume tests own R3).
    let runtime = Arc::new(ReplayRecordingRuntime::default());
    let app = awaken_protocol_ai_sdk::router::router(runtime.clone());
    let replay = json!({
        "threadId": "t1",
        "messages": [{
            "id": "u1",
            "role": "user",
            "parts": [{ "type": "text", "text": "original" }]
        }]
    });
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/chat")
                .header("content-type", "application/json")
                .body(Body::from(replay.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(body.contains("\"type\":\"finish\""), "R1/E1: {body}");
    assert!(!body.contains("\"type\":\"error\""), "R1/E1: {body}");

    let history_only = json!({
        "threadId": "t1",
        "messages": [
            {
                "id": "u1",
                "role": "user",
                "parts": [{ "type": "text", "text": "original" }]
            },
            {
                "id": "a1",
                "role": "assistant",
                "parts": [{ "type": "text", "text": "already answered" }]
            }
        ]
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/chat")
                .header("content-type", "application/json")
                .body(Body::from(history_only.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(body.contains("\"type\":\"error\""), "R2/E2: {body}");

    let runs = runtime.runs.lock().unwrap();
    assert_eq!(runs.len(), 1, "R1-R2 exactly one Run admission");
    assert_eq!(runs[0].0, "u1", "R1/E1 stable operation");
    assert_eq!(runs[0].1.len(), 1, "R1/E1 one operation input");
    assert_eq!(runs[0].1[0].id.0, "u1", "R1/E1 stable message");
    assert_eq!(blocks_text(&runs[0].1[0].content), "original", "R1/E1");
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

/// A runtime awaiting on a built-in tool `c1`; resume drives it to completion.
struct AwaitingRuntime;

#[async_trait::async_trait]
impl RunApplication for AwaitingRuntime {
    async fn run(
        &self,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        unreachable!("this test only drives the resume path")
    }

    async fn resume(
        &self,
        _operation_id: &str,
        _thread: &str,
        _tool_use_id: &str,
        _resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        use awaken_agent_contract::agent::message::{Id, Role};
        Ok(StepOutcome::ended(
            vec![Message::text(Id("a1".into()), Role::Assistant, "done")],
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunApplicationError> {
        Ok(Some(Pending {
            tool_use_id: "c1".into(),
            name: "write".into(),
            input: serde_json::Value::Null,
            client_executed: false,
        }))
    }

    async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunApplicationError> {
        Ok(Vec::new())
    }

    fn model(&self) -> String {
        "test".into()
    }
}

#[tokio::test]
async fn a_matching_tool_decision_resumes_an_awaiting_run_to_a_finish() {
    let app = awaken_protocol_ai_sdk::router::router(Arc::new(AwaitingRuntime));
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
impl RunApplication for PanickingRuntime {
    async fn run(
        &self,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        panic!("the turn task died");
    }
    async fn resume(
        &self,
        _operation_id: &str,
        _thread: &str,
        _tool_use_id: &str,
        _resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        unreachable!()
    }
    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunApplicationError> {
        Ok(None)
    }
    async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunApplicationError> {
        Ok(Vec::new())
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
