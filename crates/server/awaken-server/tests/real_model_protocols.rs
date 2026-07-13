//! Real-model conformance across every protocol adapter: multi-turn conversation
//! (each protocol threads history to the model) and multi-agent (a graded outcome
//! runs a judge sub-agent through the kernel). Gated with `#[ignore]`; run:
//!
//! ```sh
//! KIMI_API_KEY=sk-... cargo test -p awaken-server \
//!   --test real_model_protocols -- --ignored --nocapture
//! ```
//!
//! Env: `KIMI_API_KEY` (required), `KIMI_BASE_URL` (default Kimi coding endpoint),
//! `KIMI_MODEL` (default `kimi-k2-0711-preview`).

use std::sync::Arc;

use awaken_provider_genai::GenaiExecutor;
use awaken_scenario_host::{build_graded_router, build_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(app: &Router, method: &str, uri: &str, body: Value) -> (StatusCode, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn live_env() -> (String, String, String) {
    let key = std::env::var("KIMI_API_KEY")
        .expect("set KIMI_API_KEY (the Kimi/Anthropic-compatible key) to run this test");
    let base = std::env::var("KIMI_BASE_URL")
        .unwrap_or_else(|_| "https://api.kimi.com/coding/v1/".to_string());
    let model = std::env::var("KIMI_MODEL").unwrap_or_else(|_| "kimi-k2-0711-preview".to_string());
    (base, key, model)
}

fn live_executor() -> (Arc<GenaiExecutor>, String) {
    let (base, key, model) = live_env();
    (
        Arc::new(GenaiExecutor::anthropic_compatible(base, key)),
        model,
    )
}

fn live_router() -> Router {
    let (executor, model) = live_executor();
    build_router(executor, model)
}

/// Concatenate the `field` of every SSE `data:` event whose `type` matches.
fn sse_concat(body: &str, event_type: &str, field: &str) -> String {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|json| serde_json::from_str::<Value>(json).ok())
        .filter(|event| event["type"] == event_type)
        .filter_map(|event| event[field].as_str().map(str::to_string))
        .collect()
}

const SET: &str = "Remember my codeword: BALTHAZAR. Reply with just OK.";
const ASK: &str = "What is my codeword? Reply with only the single word.";

fn recalled(reply: &str) -> bool {
    reply.to_uppercase().contains("BALTHAZAR")
}

// ---------------------------------------------------------------------------
// Managed protocol (/v1/sessions)
// ---------------------------------------------------------------------------

/// The concatenated text of the last agent message in the session's committed
/// event log (read after the turn drives the run).
async fn managed_last_reply(app: &Router, session: &str) -> String {
    let (status, body) = call(
        app,
        "GET",
        &format!("/v1/sessions/{session}/events"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "managed events read failed: {body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    value["data"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .rev()
        .find(|e| e["type"] == "agent.message")
        .and_then(|e| e["content"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|b| b["text"].as_str())
        .collect()
}

async fn managed_turn(app: &Router, session: &str, text: &str) -> String {
    let (status, body) = call(
        app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        json!({ "events": [{ "type": "user.message",
            "content": [{ "type": "text", "text": text }] }] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "managed turn failed: {body}");
    managed_last_reply(app, session).await
}

#[tokio::test]
#[ignore = "hits a live model endpoint; run with KIMI_API_KEY set and --ignored"]
async fn managed_multi_turn_remembers_context() {
    let app = live_router();
    let (_, body) = call(
        &app,
        "POST",
        "/v1/sessions",
        json!({ "agent": "assistant" }),
    )
    .await;
    let session = serde_json::from_str::<Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    managed_turn(&app, &session, SET).await;
    let reply = managed_turn(&app, &session, ASK).await;
    eprintln!("[managed] reply: {reply:?}");
    assert!(
        recalled(&reply),
        "managed lost multi-turn context: {reply:?}"
    );
}

// ---------------------------------------------------------------------------
// AI SDK protocol (/v1/ai-sdk)
// ---------------------------------------------------------------------------

async fn ai_sdk_turn(app: &Router, thread: &str, id: &str, text: &str) -> String {
    let (status, body) = call(
        app,
        "POST",
        &format!("/v1/ai-sdk/threads/{thread}/runs"),
        json!({ "threadId": thread,
            "messages": [{ "id": id, "role": "user", "parts": [{ "type": "text", "text": text }] }] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "ai-sdk run failed: {body}");
    sse_concat(&body, "text-delta", "delta")
}

#[tokio::test]
#[ignore = "hits a live model endpoint; run with KIMI_API_KEY set and --ignored"]
async fn ai_sdk_multi_turn_remembers_context() {
    let app = live_router();
    ai_sdk_turn(&app, "sdk-mt", "u1", SET).await;
    let reply = ai_sdk_turn(&app, "sdk-mt", "u2", ASK).await;
    eprintln!("[ai-sdk] reply: {reply:?}");
    assert!(
        recalled(&reply),
        "ai-sdk lost multi-turn context: {reply:?}"
    );
}

// ---------------------------------------------------------------------------
// AG-UI protocol (/v1/ag-ui)
// ---------------------------------------------------------------------------

async fn ag_ui_turn(app: &Router, thread: &str, run: &str, text: &str) -> String {
    let (status, body) = call(
        app,
        "POST",
        "/v1/ag-ui/agents/assistant",
        json!({ "threadId": thread, "runId": run,
            "messages": [{ "id": run, "role": "user", "content": text }] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "ag-ui run failed: {body}");
    sse_concat(&body, "TEXT_MESSAGE_CONTENT", "delta")
}

#[tokio::test]
#[ignore = "hits a live model endpoint; run with KIMI_API_KEY set and --ignored"]
async fn ag_ui_multi_turn_remembers_context() {
    let app = live_router();
    ag_ui_turn(&app, "ag-mt", "r1", SET).await;
    let reply = ag_ui_turn(&app, "ag-mt", "r2", ASK).await;
    eprintln!("[ag-ui] reply: {reply:?}");
    assert!(recalled(&reply), "ag-ui lost multi-turn context: {reply:?}");
}

// ---------------------------------------------------------------------------
// A2A protocol (/v1/a2a)
// ---------------------------------------------------------------------------

async fn a2a_turn(app: &Router, context: &str, id: &str, text: &str) -> String {
    let (status, body) = call(
        app,
        "POST",
        "/v1/a2a/message:send",
        json!({ "message": {
            "messageId": id, "contextId": context, "role": "ROLE_USER",
            "parts": [{ "text": text }] } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a2a send failed: {body}");
    let task = serde_json::from_str::<Value>(&body).unwrap()["task"].clone();
    task["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
#[ignore = "hits a live model endpoint; run with KIMI_API_KEY set and --ignored"]
async fn a2a_multi_turn_remembers_context() {
    let app = live_router();
    a2a_turn(&app, "a2a-mt", "m1", SET).await;
    let reply = a2a_turn(&app, "a2a-mt", "m2", ASK).await;
    eprintln!("[a2a] reply: {reply:?}");
    assert!(recalled(&reply), "a2a lost multi-turn context: {reply:?}");
}

// ---------------------------------------------------------------------------
// Multi-agent: a graded outcome runs a judge sub-agent through the kernel.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "hits a live model endpoint; run with KIMI_API_KEY set and --ignored"]
async fn multi_agent_graded_outcome_runs_a_judge_subagent() {
    let (executor, model) = live_executor();
    // Outcomes are graded by a judge sub-agent (a second run) driven by the same
    // live model — a genuine multi-agent flow: a worker run plus a judge sub-run.
    let app = build_graded_router(executor, model, "assistant");

    let (_, body) = call(
        &app,
        "POST",
        "/v1/sessions",
        json!({ "agent": "assistant" }),
    )
    .await;
    let session = serde_json::from_str::<Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // A draft turn, then an outcome the judge sub-agent grades (and revises).
    managed_turn(
        &app,
        &session,
        "Write a one-sentence greeting that says hello.",
    )
    .await;
    let (status, body) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        json!({ "events": [{ "type": "user.define_outcome",
            "description": "Greet the user.",
            "rubric": "the reply contains the word hello",
            "max_iterations": 2 }] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "define_outcome failed: {body}");

    // The worker run produced a deliverable and a judge sub-agent graded it: read
    // the committed event log and look for the outcome-evaluation span (the judge
    // sub-run) plus an agent message.
    let (_, events_body) = call(
        &app,
        "GET",
        &format!("/v1/sessions/{session}/events"),
        Value::Null,
    )
    .await;
    let events = serde_json::from_str::<Value>(&events_body).unwrap()["data"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let types: Vec<&str> = events
        .iter()
        .map(|e| e["type"].as_str().unwrap_or("?"))
        .collect();
    eprintln!("[multi-agent] event types: {types:?}");
    assert!(
        types.contains(&"agent.message"),
        "the worker run should produce an agent message: {events_body}"
    );
    assert!(
        types.iter().any(|t| t.contains("outcome")),
        "a judge sub-agent should have evaluated the outcome: {types:?}"
    );
}
