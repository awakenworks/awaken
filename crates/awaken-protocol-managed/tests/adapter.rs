//! Adapter integration tests with fake runtimes: the happy path, and the HITL
//! park -> `requires_action` -> `user.tool_confirmation` -> resume round-trip.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_protocol_managed::dto::StopReason;
use awaken_protocol_managed::{
    Decision, ManagedState, OutcomeIteration, OutcomeReport, RunError, SessionRuntime, TurnOutcome,
    router,
};
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

fn types(list: &serde_json::Value) -> Vec<String> {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect()
}

async fn create(app: &Router) -> String {
    let s = json_call(
        app,
        "POST",
        "/v1/sessions",
        serde_json::json!({ "agent": "coder" }),
    )
    .await;
    s["id"].as_str().unwrap().to_string()
}

/// The happy path: one assistant text reply, no tools.
struct EchoFake;

#[async_trait::async_trait]
impl SessionRuntime for EchoFake {
    async fn run_turn(
        &self,
        _agent: &str,
        _thread: &str,
        user_text: &str,
    ) -> Result<TurnOutcome, RunError> {
        Ok(TurnOutcome {
            messages: vec![Message::text(
                Id("a".into()),
                Role::Assistant,
                format!("echo: {user_text}"),
            )],
            stop: StopReason::EndTurn,
            pending: None,
        })
    }
    async fn resume(&self, _thread: &str, _decision: Decision) -> Result<TurnOutcome, RunError> {
        Err(RunError("no parked run".into()))
    }
    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeReport, RunError> {
        Err(RunError("no outcome".into()))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

#[tokio::test]
async fn happy_path_projects_message_and_idle() {
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let id = create(&app).await;
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }),
    )
    .await;
    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(types(&list), vec!["agent.message", "session.status_idle"]);
}

/// A runtime that parks on a tool needing approval, then completes on resume.
struct ParkingFake;

#[async_trait::async_trait]
impl SessionRuntime for ParkingFake {
    async fn run_turn(
        &self,
        _agent: &str,
        _thread: &str,
        _user_text: &str,
    ) -> Result<TurnOutcome, RunError> {
        // The assistant asked to run a tool; the run parked before executing it.
        Ok(TurnOutcome {
            messages: vec![Message {
                id: Id("a1".into()),
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: "write".into(),
                    input: serde_json::json!({ "path": "x.txt", "content": "hi" }),
                }],
            }],
            stop: StopReason::RequiresAction {
                event_ids: Vec::new(),
            },
            pending: Some("call-1".into()),
        })
    }
    async fn resume(&self, _thread: &str, decision: Decision) -> Result<TurnOutcome, RunError> {
        assert!(decision.allow);
        Ok(TurnOutcome {
            messages: vec![
                Message {
                    id: Id("t1".into()),
                    role: Role::Tool,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call-1".into(),
                        content: vec![ContentBlock::text("wrote x.txt")],
                    }],
                },
                Message::text(Id("a2".into()), Role::Assistant, "done"),
            ],
            stop: StopReason::EndTurn,
            pending: None,
        })
    }
    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeReport, RunError> {
        Err(RunError("no outcome".into()))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

/// A fake outcome loop: one revision then satisfied.
struct OutcomeFake;

#[async_trait::async_trait]
impl SessionRuntime for OutcomeFake {
    async fn run_turn(&self, _a: &str, _t: &str, _u: &str) -> Result<TurnOutcome, RunError> {
        Err(RunError("no turn".into()))
    }
    async fn resume(&self, _t: &str, _d: Decision) -> Result<TurnOutcome, RunError> {
        Err(RunError("no resume".into()))
    }
    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeReport, RunError> {
        Ok(OutcomeReport {
            iterations: vec![
                OutcomeIteration {
                    messages: Vec::new(),
                    outcome_id: "outc_1".into(),
                    iteration: 1,
                    result: "needs_revision".into(),
                    explanation: "add FINAL".into(),
                },
                OutcomeIteration {
                    messages: vec![Message::text(
                        Id("r".into()),
                        Role::Assistant,
                        "FINAL answer",
                    )],
                    outcome_id: "outc_1".into(),
                    iteration: 2,
                    result: "satisfied".into(),
                    explanation: "ok".into(),
                },
            ],
        })
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

#[tokio::test]
async fn outcome_loop_projects_evaluations() {
    let app = router(Arc::new(ManagedState::new(OutcomeFake)));
    let id = create(&app).await;
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.define_outcome", "description": "finish", "rubric": "FINAL", "max_iterations": 3 }] }),
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
        types(&list),
        vec![
            "span.outcome_evaluation_start",
            "span.outcome_evaluation_end",
            "agent.message",
            "span.outcome_evaluation_start",
            "span.outcome_evaluation_end",
            "session.status_idle"
        ]
    );
    let ends: Vec<&serde_json::Value> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "span.outcome_evaluation_end")
        .collect();
    assert_eq!(ends[0]["result"], "needs_revision");
    assert_eq!(ends[1]["result"], "satisfied");
}

#[tokio::test]
async fn hitl_park_confirm_resume() {
    let app = router(Arc::new(ManagedState::new(ParkingFake)));
    let id = create(&app).await;

    // 1. A message -> the run parks with requires_action.
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "write it" }] }] }),
    )
    .await;
    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(types(&list), vec!["agent.tool_use", "session.status_idle"]);

    let tool_use = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.tool_use")
        .unwrap();
    assert_eq!(tool_use["id"], "call-1");
    assert_eq!(tool_use["evaluated_permission"], "ask");
    let idle = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "session.status_idle")
        .unwrap();
    assert_eq!(idle["stop_reason"]["type"], "requires_action");
    assert_eq!(idle["stop_reason"]["event_ids"][0], "call-1");

    // 2. Confirm the tool -> the run resumes and completes.
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.tool_confirmation", "tool_use_id": "call-1", "result": "allow" }] }),
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
        types(&list),
        vec![
            "agent.tool_use",
            "session.status_idle",
            "agent.tool_result",
            "agent.message",
            "session.status_idle"
        ]
    );
    let last_idle = list["data"].as_array().unwrap().last().unwrap();
    assert_eq!(last_idle["stop_reason"]["type"], "end_turn");
}

#[tokio::test]
async fn retrieve_session_and_sse_event_names() {
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let id = create(&app).await;

    // GET /v1/sessions/{id} returns the session.
    let session = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(session["id"], id);
    assert_eq!(session["type"], "session");

    // Run a turn, then the SSE stream carries `event:` lines named by type.
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }),
    )
    .await;
    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/sessions/{id}/events/stream"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let sse = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(sse.contains("event: agent.message"), "sse: {sse}");
    assert!(sse.contains("event: session.status_idle"), "sse: {sse}");
}

#[tokio::test]
async fn unknown_session_is_404() {
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let req = Request::builder()
        .method("GET")
        .uri("/v1/sessions/nope/events")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
