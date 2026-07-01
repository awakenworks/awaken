//! Server integration tests through the *real* kernel: the echo path, and a full
//! HITL round-trip where a mutating tool parks for approval, is confirmed, runs
//! rooted in the session's sandbox, and the read-back proves isolation.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, ChatRole, LlmExecutor, ToolCall,
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

async fn create_session(app: &Router) -> String {
    let s = json_call(
        app,
        "POST",
        "/v1/sessions",
        serde_json::json!({ "agent": "assistant" }),
    )
    .await;
    s["id"].as_str().unwrap().to_string()
}

async fn send_message(app: &Router, session: &str, text: &str) -> serde_json::Value {
    json_call(
        app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": text }] }] }),
    )
    .await;
    json_call(
        app,
        "GET",
        &format!("/v1/sessions/{session}/events"),
        serde_json::Value::Null,
    )
    .await
}

async fn confirm(app: &Router, session: &str, tool_use_id: &str) -> serde_json::Value {
    json_call(
        app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        serde_json::json!({ "events": [{ "type": "user.tool_confirmation", "tool_use_id": tool_use_id, "result": "allow" }] }),
    )
    .await;
    json_call(
        app,
        "GET",
        &format!("/v1/sessions/{session}/events"),
        serde_json::Value::Null,
    )
    .await
}

#[tokio::test]
async fn echo_turn_end_to_end() {
    let app = build_router(Arc::new(EchoModel), "echo-model");
    let id = create_session(&app).await;
    let list = send_message(&app, &id, "hi there").await;
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
}

/// Stateless probe: write the user's text to a relative `probe.txt`, read it back,
/// reply. `write` is asked (parks); `read` is allowed (runs).
struct WriteReadProbe;

#[async_trait::async_trait]
impl LlmExecutor for WriteReadProbe {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let tool_results = request
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::Tool)
            .count();
        let user_text = request
            .messages
            .iter()
            .find(|m| m.role == ChatRole::User)
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>()
            })
            .unwrap_or_default();
        let output = match tool_results {
            0 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "w".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({ "path": "probe.txt", "content": user_text }),
            }]),
            1 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "r".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({ "path": "probe.txt" }),
            }]),
            _ => AssistantOutput::text("done"),
        };
        Ok(ChatResponse {
            output,
            usage: None,
        })
    }
}

fn read_result_text(list: &serde_json::Value) -> String {
    let results: Vec<&serde_json::Value> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "agent.tool_result")
        .collect();
    results.last().unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn hitl_write_parks_then_confirms_and_reads_rooted() {
    let app = build_router(Arc::new(WriteReadProbe), "scripted");
    let id = create_session(&app).await;

    // The write is asked -> the run parks.
    let list = send_message(&app, &id, "HELLO-SANDBOX").await;
    assert_eq!(
        event_types(&list),
        vec!["agent.tool_use", "session.status_idle"]
    );
    let idle = list["data"].as_array().unwrap().last().unwrap();
    assert_eq!(idle["stop_reason"]["type"], "requires_action");
    assert_eq!(idle["stop_reason"]["event_ids"][0], "w");

    // Confirm -> write runs (rooted), read runs (allowed), reply.
    let list = confirm(&app, &id, "w").await;
    assert!(event_types(&list).contains(&"agent.message".to_string()));
    let last = list["data"].as_array().unwrap().last().unwrap();
    assert_eq!(last["stop_reason"]["type"], "end_turn");
    assert!(read_result_text(&list).contains("HELLO-SANDBOX"));
}

/// A model that replies with a draft, and revises to include "FINAL" once it sees
/// the goal-loop's feedback. Stateless: it keys off the last user message.
struct ReviseModel;

#[async_trait::async_trait]
impl LlmExecutor for ReviseModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::User)
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>()
            })
            .unwrap_or_default();
        let reply = if last_user.contains("did not meet the goal") {
            "FINAL answer"
        } else {
            "a rough draft"
        };
        Ok(ChatResponse {
            output: AssistantOutput::text(reply),
            usage: None,
        })
    }
}

async fn define_outcome(app: &Router, session: &str, rubric: &str) -> serde_json::Value {
    json_call(
        app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        serde_json::json!({ "events": [{ "type": "user.define_outcome", "description": "finish it", "rubric": rubric, "max_iterations": 3 }] }),
    )
    .await;
    json_call(
        app,
        "GET",
        &format!("/v1/sessions/{session}/events"),
        serde_json::Value::Null,
    )
    .await
}

#[tokio::test]
async fn outcome_iterates_until_satisfied() {
    let app = build_router(Arc::new(ReviseModel), "scripted");
    let id = create_session(&app).await;

    // A draft, then define an outcome the draft misses -> revise -> satisfied.
    send_message(&app, &id, "write something").await;
    let list = define_outcome(&app, &id, "FINAL").await;

    let ends: Vec<&serde_json::Value> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "span.outcome_evaluation_end")
        .collect();
    assert!(ends.len() >= 2, "expected at least two evaluation rounds");
    assert_eq!(ends.first().unwrap()["result"], "needs_revision");
    assert_eq!(ends.last().unwrap()["result"], "satisfied");
    // The revision that satisfied the goal was projected.
    let messages: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "agent.message")
        .map(|e| e["content"][0]["text"].as_str().unwrap())
        .collect();
    assert!(
        messages.iter().any(|m| m.contains("FINAL")),
        "messages: {messages:?}"
    );
}

/// A model that never stops: it always calls an allowed tool, so the loop runs
/// until the step ceiling -> `EndCause::MaxSteps` -> `retries_exhausted`.
struct LoopModel;

#[async_trait::async_trait]
impl LlmExecutor for LoopModel {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "g".into(),
                tool_id: "glob".into(),
                arguments: serde_json::json!({ "pattern": "*" }),
            }]),
            usage: None,
        })
    }
}

#[tokio::test]
async fn max_steps_maps_to_retries_exhausted() {
    let app = build_router(Arc::new(LoopModel), "loop");
    let id = create_session(&app).await;
    let list = send_message(&app, &id, "loop forever").await;
    let idle = list["data"].as_array().unwrap().last().unwrap();
    assert_eq!(idle["stop_reason"]["type"], "retries_exhausted");
}

#[tokio::test]
async fn sessions_are_isolated() {
    let app = build_router(Arc::new(WriteReadProbe), "scripted");
    let one = create_session(&app).await;
    let two = create_session(&app).await;

    send_message(&app, &one, "SECRET-ONE").await;
    let list_one = confirm(&app, &one, "w").await;
    send_message(&app, &two, "SECRET-TWO").await;
    let list_two = confirm(&app, &two, "w").await;

    let read_one = read_result_text(&list_one);
    let read_two = read_result_text(&list_two);
    assert!(
        read_one.contains("SECRET-ONE") && !read_one.contains("SECRET-TWO"),
        "one: {read_one}"
    );
    assert!(
        read_two.contains("SECRET-TWO") && !read_two.contains("SECRET-ONE"),
        "two: {read_two}"
    );
}
