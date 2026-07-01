//! Adapter integration test: drive the four routes with a fake `SessionRuntime`,
//! proving DTO decode, projection, and SSE without a runtime or a network.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_protocol_managed::dto::StopReason;
use awaken_protocol_managed::{ManagedState, RunError, SessionRuntime, TurnOutcome, router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// A fixed turn: an assistant tool call, its result, and a text reply.
struct FakeRuntime;

#[async_trait::async_trait]
impl SessionRuntime for FakeRuntime {
    async fn run_turn(
        &self,
        _agent: &str,
        _thread: &str,
        user_text: &str,
    ) -> Result<TurnOutcome, RunError> {
        let tool_call = Message {
            id: Id("a1".into()),
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "read".into(),
                input: serde_json::json!({ "path": "x.txt" }),
            }],
        };
        let tool_result = Message {
            id: Id("t1r".into()),
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: vec![ContentBlock::text("file body")],
            }],
        };
        let reply = Message::text(
            Id("a2".into()),
            Role::Assistant,
            format!("echo: {user_text}"),
        );
        Ok(TurnOutcome {
            messages: vec![tool_call, tool_result, reply],
            stop: StopReason::EndTurn,
        })
    }

    fn model(&self) -> String {
        "test-model".into()
    }
}

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
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{method} {uri}");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn body_string(app: &Router, uri: &str) -> String {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn full_session_roundtrip() {
    let state = Arc::new(ManagedState::new(FakeRuntime));
    let app = router(state);

    // 1. Create a session.
    let session = json_call(
        &app,
        "POST",
        "/v1/sessions",
        serde_json::json!({ "agent": "coder", "environment_id": "env_local" }),
    )
    .await;
    assert_eq!(session["type"], "session");
    assert_eq!(session["status"], "idle");
    assert_eq!(session["agent"]["model"], "test-model");
    let id = session["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("sesn_"));

    // 2. Send a user.message — the turn runs, events are projected.
    let receipts = json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({
            "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }]
        }),
    )
    .await;
    let receipt = &receipts["data"][0];
    assert_eq!(receipt["type"], "user.message");
    assert!(receipt["id"].as_str().unwrap().starts_with("evt_"));
    assert!(receipt["processed_at"].is_null());

    // 3. List the projected events.
    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    let types: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        vec![
            "agent.tool_use",
            "agent.tool_result",
            "agent.message",
            "session.status_idle"
        ]
    );
    // agent.message carries the echoed text; the terminal stop_reason is a tagged object.
    let msg = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.message")
        .unwrap();
    assert_eq!(msg["content"][0]["text"], "echo: hi");
    let idle = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "session.status_idle")
        .unwrap();
    assert_eq!(idle["stop_reason"]["type"], "end_turn");

    // 4. The SSE stream replays the same events.
    let sse = body_string(&app, &format!("/v1/sessions/{id}/events/stream")).await;
    assert!(sse.contains("\"type\":\"agent.message\""));
    assert!(sse.contains("\"type\":\"session.status_idle\""));
    assert!(sse.contains("data:"));
}

#[tokio::test]
async fn unknown_session_is_404() {
    let state = Arc::new(ManagedState::new(FakeRuntime));
    let app = router(state);
    let req = Request::builder()
        .method("GET")
        .uri("/v1/sessions/nope/events")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
