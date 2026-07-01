//! Cross-protocol integration through the *real* kernel: the AI SDK v6 UI Message
//! Stream surface, and the headline case — a run started by the AI SDK adapter
//! that parks on a client-executed tool is resumed by the Managed Agents adapter
//! on the *same thread*, and the result is visible back through the AI SDK.

use awaken_server_local::{build_custom_router, build_echo_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// POST/GET returning the raw response body as a string (SSE bodies are not JSON).
async fn call(app: &Router, method: &str, uri: &str, body: Value) -> (StatusCode, String) {
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
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// Parse the JSON events out of a UI Message Stream SSE body.
fn sse_events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str::<Value>(data).unwrap())
        .collect()
}

fn user_msg(id: &str, text: &str) -> Value {
    json!({ "id": id, "role": "user", "parts": [{ "type": "text", "text": text }] })
}

/// An AG-UI message (content is a plain string, not parts).
fn ag_user_msg(id: &str, text: &str) -> Value {
    json!({ "id": id, "role": "user", "content": text })
}

#[tokio::test]
async fn ai_sdk_echo_turn_streams_text() {
    let app = build_echo_router();
    let (status, body) = call(
        &app,
        "POST",
        "/v1/ai-sdk/agents/assistant/runs",
        json!({ "threadId": "t1", "messages": [user_msg("u1", "hi")] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = sse_events(&body);

    assert!(
        events
            .iter()
            .any(|e| e["type"] == "text-delta" && e["delta"] == "Echo: hi"),
        "stream should carry the echoed text: {body}"
    );
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "finish" && e["finishReason"] == "stop"),
        "stream should finish with reason stop: {body}"
    );
}

#[tokio::test]
async fn ai_sdk_history_reflects_committed_turn() {
    let app = build_echo_router();
    call(
        &app,
        "POST",
        "/v1/ai-sdk/threads/t-hist/runs",
        json!({ "threadId": "t-hist", "messages": [user_msg("u1", "remember me")] }),
    )
    .await;

    let (status, body) = call(
        &app,
        "GET",
        "/v1/ai-sdk/threads/t-hist/messages",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let payload: Value = serde_json::from_str(&body).unwrap();
    let messages = payload["messages"].as_array().unwrap();

    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "user" && m["parts"][0]["text"] == "remember me"),
        "history should include the user turn: {body}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "assistant" && m["parts"][0]["text"] == "Echo: remember me"),
        "history should include the assistant reply: {body}"
    );
}

/// The headline cross-protocol case. A managed session fixes the shared thread id;
/// the AI SDK adapter drives a turn on that thread which parks on a client-executed
/// tool; the Managed adapter delivers the tool result on the *same thread* and the
/// run resumes; the final answer is visible back through the AI SDK.
#[tokio::test]
async fn ai_sdk_parks_then_managed_resumes_same_thread() {
    let app = build_custom_router();

    // 1. Managed creates the session; its id is the shared thread id.
    let (_, created) = call(
        &app,
        "POST",
        "/v1/sessions",
        json!({ "agent": "assistant" }),
    )
    .await;
    let session: Value = serde_json::from_str(&created).unwrap();
    let thread = session["id"].as_str().unwrap().to_string();

    // 2. AI SDK drives a turn on that thread; the model calls the client tool
    //    `submit_answer` and the run parks.
    let (status, body) = call(
        &app,
        "POST",
        &format!("/v1/ai-sdk/threads/{thread}/runs"),
        json!({ "threadId": thread, "messages": [user_msg("u1", "answer please")] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = sse_events(&body);
    assert!(
        events.iter().any(|e| e["type"] == "tool-input-available"
            && e["toolCallId"] == "c1"
            && e["providerExecuted"] == false),
        "AI SDK stream should surface the client tool call to run: {body}"
    );
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "finish" && e["finishReason"] == "tool-calls"),
        "parked run should finish the step with tool-calls: {body}"
    );

    // 3. Managed delivers the client tool result on the SAME thread, resuming the
    //    run the AI SDK started.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{thread}/events"),
        json!({ "events": [{
            "type": "user.custom_tool_result",
            "custom_tool_use_id": "c1",
            "content": [{ "type": "text", "text": "42" }]
        }] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 4. The resumed answer is visible back through the AI SDK history.
    let (_, body) = call(
        &app,
        "GET",
        &format!("/v1/ai-sdk/threads/{thread}/messages"),
        Value::Null,
    )
    .await;
    let payload: Value = serde_json::from_str(&body).unwrap();
    let messages = payload["messages"].as_array().unwrap();
    assert!(
        messages.iter().any(|m| m["role"] == "assistant"
            && m["parts"].as_array().unwrap().iter().any(|p| p["text"]
                .as_str()
                .map(|t| t.contains("got: 42"))
                .unwrap_or(false))),
        "the answer delivered via Managed should appear in AI SDK history: {body}"
    );
}

#[tokio::test]
async fn ag_ui_echo_turn_streams_run_events() {
    let app = build_echo_router();
    let (status, body) = call(
        &app,
        "POST",
        "/v1/ag-ui/agents/assistant",
        json!({ "threadId": "ag1", "runId": "r1", "messages": [ag_user_msg("u1", "hi")] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = sse_events(&body);

    assert!(
        events
            .iter()
            .any(|e| e["type"] == "RUN_STARTED" && e["threadId"] == "ag1" && e["runId"] == "r1"),
        "stream should open with RUN_STARTED: {body}"
    );
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "TEXT_MESSAGE_CONTENT" && e["delta"] == "Echo: hi"),
        "stream should carry the echoed text: {body}"
    );
    assert!(
        events.iter().any(|e| e["type"] == "RUN_FINISHED"),
        "stream should close with RUN_FINISHED: {body}"
    );
}

/// The three-protocol case: AG-UI drives a turn that parks on a client tool, the
/// Managed adapter delivers the result on the same thread, and the resumed answer
/// is visible back through the AI SDK — all three over one shared host/thread.
#[tokio::test]
async fn ag_ui_parks_then_managed_resumes_visible_via_ai_sdk() {
    let app = build_custom_router();

    let (_, created) = call(
        &app,
        "POST",
        "/v1/sessions",
        json!({ "agent": "assistant" }),
    )
    .await;
    let session: Value = serde_json::from_str(&created).unwrap();
    let thread = session["id"].as_str().unwrap().to_string();

    // AG-UI drives the turn; the model calls the client tool `submit_answer`.
    let (status, body) = call(
        &app,
        "POST",
        "/v1/ag-ui",
        json!({ "threadId": thread, "runId": "r1", "messages": [ag_user_msg("u1", "answer please")] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = sse_events(&body);
    assert!(
        events.iter().any(|e| e["type"] == "TOOL_CALL_START"
            && e["toolCallId"] == "c1"
            && e["toolCallName"] == "submit_answer"),
        "AG-UI stream should surface the client tool call: {body}"
    );
    assert!(
        events.iter().any(|e| e["type"] == "RUN_FINISHED"),
        "parked run should finish: {body}"
    );

    // Managed delivers the client tool result on the SAME thread.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{thread}/events"),
        json!({ "events": [{
            "type": "user.custom_tool_result",
            "custom_tool_use_id": "c1",
            "content": [{ "type": "text", "text": "42" }]
        }] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The resumed answer is visible back through the AI SDK history.
    let (_, body) = call(
        &app,
        "GET",
        &format!("/v1/ai-sdk/threads/{thread}/messages"),
        Value::Null,
    )
    .await;
    let payload: Value = serde_json::from_str(&body).unwrap();
    let messages = payload["messages"].as_array().unwrap();
    assert!(
        messages.iter().any(|m| m["role"] == "assistant"
            && m["parts"].as_array().unwrap().iter().any(|p| p["text"]
                .as_str()
                .map(|t| t.contains("got: 42"))
                .unwrap_or(false))),
        "answer delivered via Managed should appear in AI SDK history: {body}"
    );
}
