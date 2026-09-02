//! Cross-protocol integration through the *real* kernel: the AI SDK v6 UI Message
//! Stream surface, and the headline case — a run started by the AI SDK adapter
//! that awaits on a client-executed tool is resumed by the Managed Agents adapter
//! on the *same thread*, and the result is visible back through the AI SDK.
//!
//! Conformance matrix — every protocol adapter is held to the same six categories,
//! each in its own wire vocabulary (Managed & A2A: HTTP status + JSON envelope;
//! AI SDK: UI Message Stream; AG-UI: run event stream). A2A is request/response
//! (`message:send` returns a `Task`), so categories 1–3 fold into that one call:
//!
//! | # | category                    | Managed | AI SDK          | AG-UI          | A2A            |
//! |---|-----------------------------|---------|-----------------|----------------|----------------|
//! | 1 | Run / streaming             | server  | echo_run        | echo_run       | a2a_run        |
//! | 2 | history read-back           | server  | history_reflects| cross-proto    | a2a_history    |
//! | 3 | client-tool await + resume   | server  | awaits_then_*    | awaits_then_*   | a2a_awaits_*    |
//! | 4 | driver error → wire format  | server  | driver_error    | driver_error   | router unit    |
//! | 5 | malformed body → wire format| Managed | malformed_body  | malformed_body | a2a_malformed  |
//! | 6 | interrupt                   | server  | (shared host)   | (shared host)  | (shared host)  |
//!
//! Errors never leak axum's default plain-text 400: each adapter's JSON extractor
//! converts a decode failure into its own error frame (categories 4 and 5).
//!
//! Cross-protocol causal graph:
//!
//! `Run accepted -> stream reaches terminal frame -> committed Thread facts`
//! `committed thread facts -> history projection through any adapter`
//! `client-tool call -> awaiting -> same-thread result -> resume -> committed reply`
//!
//! | rule | terminal stream consumed | awaiting tool | same-thread result | effect |
//! |------|--------------------------|---------------|--------------------|--------|
//! | X1   | yes                      | no            | -                  | history exposes Run |
//! | X2   | yes                      | yes           | no                 | run remains awaiting |
//! | X3   | yes                      | yes           | yes                | resumed reply is visible cross-protocol |
//!
//! The response collector consumes through `[DONE]`, which is emitted only after
//! the Run future and authoritative commit complete. Consequently X1/X3 need no
//! timing sleeps or retry loop; the stream terminal is their consistency boundary.

mod support;

use awaken_scenario_host::{build_custom_router, build_echo_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use support::wait_for_session_events;

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

fn assert_ai_sdk_result_precedes_answer(messages: &[Value], body: &str) {
    let tool_result_index = messages
        .iter()
        .position(|message| {
            message["role"] == "assistant"
                && message["parts"].as_array().is_some_and(|parts| {
                    parts.iter().any(|part| {
                        part["toolCallId"] == "c1"
                            && part["state"] == "output-available"
                            && part["output"] == json!(42)
                    })
                })
        })
        .unwrap_or_else(|| panic!("same-thread history lacks committed c1=42: {body}"));
    let answer_index = messages
        .iter()
        .position(|message| {
            message["role"] == "assistant"
                && message["parts"].as_array().is_some_and(|parts| {
                    parts.iter().any(|part| {
                        part["type"] == "text"
                            && part["text"]
                                .as_str()
                                .is_some_and(|text| text.contains("got: 42"))
                    })
                })
        })
        .unwrap_or_else(|| panic!("same-thread history lacks the resumed answer: {body}"));
    assert!(
        tool_result_index < answer_index,
        "the committed c1=42 result must causally precede its answer: {body}"
    );
}

/// Cause/effect design: C1 one AI SDK Run receives user text `hi` through the
/// echo agent. Effects: E1 its SSE contains `Echo: hi` as a text delta; E2 the
/// stream terminates with finish reason `stop`. Decision rule S1=C1=>E1+E2.
#[tokio::test(flavor = "multi_thread")]
async fn ai_sdk_echo_run_streams_text() {
    // Causes: the fixtures below establish `ai sdk echo run streams text` with the concrete inputs,
    // state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
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

/// Cause/effect design: C1 an AI SDK Run commits user input and the echo reply on
/// one Thread; C2 history is read afterward. Effect E1: the returned items expose
/// both committed roles and exact texts. Decision rule H1=C1+C2=>E1.
#[tokio::test(flavor = "multi_thread")]
async fn ai_sdk_history_reflects_committed_run() {
    // Causes: the fixtures below establish `ai sdk history reflects committed run` with the
    // concrete inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
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
    let messages = payload["items"].as_array().unwrap();

    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "user" && m["parts"][0]["text"] == "remember me"),
        "history should include the user input: {body}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "assistant" && m["parts"][0]["text"] == "Echo: remember me"),
        "history should include the assistant reply: {body}"
    );
}

/// The headline cross-protocol case. A managed session fixes the shared thread id;
/// the AI SDK adapter drives a Run on that Thread which awaits on a client-executed
/// tool; the Managed adapter delivers the tool result on the *same thread* and the
/// run resumes; the final answer is visible back through the AI SDK.
#[tokio::test(flavor = "multi_thread")]
async fn ai_sdk_awaits_then_managed_resumes_same_thread() {
    // Causes: C1 AI SDK commits client tool call c1 on one Managed-created
    // Thread; C2 Managed accepts the exact c1=42 result receipt on that Thread.
    // Effects: the lifecycle supervisor commits the result, resumes the Run, and
    // AI SDK history shows output-available c1=42 before the `got: 42` answer.
    // Constraints/invariants: the POST receipt is acceptance, not completion;
    // one Thread transcript is the authority shared by both protocol adapters.
    // Decision rule: X2=C1 without C2 -> awaiting; X3=C1+C2 -> committed result
    // precedes committed answer in the same Thread history.
    let app = build_custom_router();

    // 1. Managed creates the session; its id is the shared thread id.
    let (_, created) = call(
        &app,
        "POST",
        "/v1/sessions",
        json!({
            "agent": "assistant",
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
        }),
    )
    .await;
    let session: Value = serde_json::from_str(&created).unwrap();
    let thread = session["id"].as_str().unwrap().to_string();

    // 2. AI SDK drives a Run on that Thread; the model calls the client tool
    //    `submit_answer` and the run awaits.
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
        "awaiting run should finish the step with tool-calls: {body}"
    );

    // 3. Managed delivers the client tool result on the SAME thread, resuming the
    //    run the AI SDK started.
    let (status, receipt_body) = call(
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
    let receipt = serde_json::from_str::<Value>(&receipt_body).unwrap();
    wait_for_session_events(
        &app,
        &thread,
        Some(&receipt),
        "the Managed c1=42 result to commit its same-Thread answer",
        |events| {
            events.iter().any(|event| {
                event["type"] == "user.custom_tool_result" && event["custom_tool_use_id"] == "c1"
            }) && events.iter().any(|event| {
                event["type"] == "agent.message"
                    && event["content"][0]["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("got: 42"))
            }) && events.iter().any(|event| {
                event["type"] == "session.status_idle" && event["stop_reason"]["type"] == "end_turn"
            })
        },
    )
    .await;

    // 4. The resumed answer is visible back through the AI SDK history.
    let (_, body) = call(
        &app,
        "GET",
        &format!("/v1/ai-sdk/threads/{thread}/messages"),
        Value::Null,
    )
    .await;
    let payload: Value = serde_json::from_str(&body).unwrap();
    let messages = payload["items"].as_array().unwrap();
    assert_ai_sdk_result_precedes_answer(messages, &body);
}

#[tokio::test(flavor = "multi_thread")]
async fn ag_ui_echo_run_streams_run_events() {
    // Causes: the fixtures below establish `ag ui echo run streams run events` with the concrete
    // inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Coverage rationale: `ag ui echo run streams run events` is one independent branch selecting
    // `all output, state, side-effect, error, and terminal assertions below hold together`; a
    // multi-row decision table is not applicable, and sibling tests own alternate causes.
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

/// The three-protocol case: AG-UI drives a Run that awaits on a client tool, the
/// Managed adapter delivers the result on the same thread, and the resumed answer
/// is visible back through the AI SDK — all three over one shared host/thread.
#[tokio::test(flavor = "multi_thread")]
async fn ag_ui_awaits_then_managed_resumes_visible_via_ai_sdk() {
    // Causes: C1 AG-UI commits client tool call c1 on a Managed-created Thread;
    // C2 Managed accepts the exact c1=42 result receipt on that same Thread.
    // Effects: committed Managed projection reaches end_turn, then AI SDK history
    // shows output-available c1=42 before the resumed `got: 42` answer.
    // Constraints/invariants: AG-UI, Managed, and AI SDK are wire projections of
    // one committed Thread; no adapter owns a parallel resume or history track.
    // Decision rule: X2=C1 without C2 -> awaiting; X3=C1+C2 -> result-before-answer
    // causality remains visible through the third protocol. X4=C1+C2 on the
    // default test stack -> the canonical Host context realization completes;
    // no adapter-specific stack override or resume path is permitted.
    let app = build_custom_router();

    let (_, created) = call(
        &app,
        "POST",
        "/v1/sessions",
        json!({
            "agent": "assistant",
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
        }),
    )
    .await;
    let session: Value = serde_json::from_str(&created).unwrap();
    let thread = session["id"].as_str().unwrap().to_string();

    // AG-UI drives the Run; the model calls the client tool `submit_answer`.
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
        "awaiting run should finish: {body}"
    );

    // Managed delivers the client tool result on the SAME thread.
    let (status, receipt_body) = call(
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
    let receipt = serde_json::from_str::<Value>(&receipt_body).unwrap();
    wait_for_session_events(
        &app,
        &thread,
        Some(&receipt),
        "the Managed c1=42 result to commit its AG-UI-originated same-Thread answer",
        |events| {
            events.iter().any(|event| {
                event["type"] == "user.custom_tool_result" && event["custom_tool_use_id"] == "c1"
            }) && events.iter().any(|event| {
                event["type"] == "agent.message"
                    && event["content"][0]["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("got: 42"))
            }) && events.iter().any(|event| {
                event["type"] == "session.status_idle" && event["stop_reason"]["type"] == "end_turn"
            })
        },
    )
    .await;

    // The resumed answer is visible back through the AI SDK history.
    let (_, body) = call(
        &app,
        "GET",
        &format!("/v1/ai-sdk/threads/{thread}/messages"),
        Value::Null,
    )
    .await;
    let payload: Value = serde_json::from_str(&body).unwrap();
    let messages = payload["items"].as_array().unwrap();
    assert_ai_sdk_result_precedes_answer(messages, &body);
}

// ── Category 4/5: errors convert to each protocol's wire format ──────────────

#[tokio::test(flavor = "multi_thread")]
async fn ai_sdk_malformed_body_returns_stream_error() {
    // A body that fails to decode must surface as a UI Message Stream error frame
    // (status 200, error in-stream), not axum's plain-text 400.
    let app = build_echo_router();
    let (status, body) = call(
        &app,
        "POST",
        "/v1/ai-sdk/agents/assistant/runs",
        json!({ "messages": 5 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "AI SDK streams the error: {body}");
    let events = sse_events(&body);
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "error" && e["errorText"].is_string()),
        "malformed body → stream error frame: {body}"
    );
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "finish" && e["finishReason"] == "error"),
        "error stream finishes with reason error: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ai_sdk_driver_error_returns_stream_error() {
    // SSE failure FMECA/decision rule: C1 the driver cannot produce a committed
    // receipt; E1 emit an error frame, E2 emit finish(error), E3 never emit the
    // normal finish(stop), and E4 close with exactly one transport sentinel.
    // Empty messages on a fresh thread fault-injects C1 at the admission/driver
    // boundary without inventing a second protocol-only completion path.
    let app = build_echo_router();
    let (status, body) = call(
        &app,
        "POST",
        "/v1/ai-sdk/threads/t-noawaiting/runs",
        json!({ "threadId": "t-noawaiting", "messages": [] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = sse_events(&body);
    assert!(
        events.iter().any(|e| e["type"] == "error"),
        "driver error → stream error frame: {body}"
    );
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "finish" && e["finishReason"] == "error"),
        "driver error → finish(error): {body}"
    );
    assert!(
        !events
            .iter()
            .any(|e| e["type"] == "finish" && e["finishReason"] == "stop"),
        "driver error must never report normal stop: {body}"
    );
    assert_eq!(body.matches("data: [DONE]").count(), 1, "one SSE sentinel");
}

#[tokio::test(flavor = "multi_thread")]
async fn ag_ui_malformed_body_returns_run_error() {
    // A body that fails to decode must surface as a bare RUN_ERROR event.
    let app = build_echo_router();
    let (status, body) = call(&app, "POST", "/v1/ag-ui", json!({ "messages": 5 })).await;
    assert_eq!(status, StatusCode::OK, "AG-UI streams the error: {body}");
    assert!(
        sse_events(&body)
            .iter()
            .any(|e| e["type"] == "RUN_ERROR" && e["message"].is_string()),
        "malformed body → RUN_ERROR event: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ag_ui_driver_error_returns_run_error() {
    // A resume with no awaiting run is a driver error → RUN_ERROR (bracketed by the
    // RUN_STARTED the run began with).
    let app = build_echo_router();
    let (status, body) = call(
        &app,
        "POST",
        "/v1/ag-ui",
        json!({ "threadId": "ag-noawaiting", "runId": "r1", "messages": [] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        sse_events(&body).iter().any(|e| e["type"] == "RUN_ERROR"),
        "driver error → RUN_ERROR event: {body}"
    );
}

// ── A2A adapter conformance (categories 1–3, 5) ─────────────────────────────

/// Causal graph for the A2A cross-protocol slice:
///
/// - C1: message has the canonical `kind = message` discriminator.
/// - C2: role is the canonical lowercase `user` token.
/// - C3: every text part has the canonical `kind = text` discriminator.
/// - C4: `contextId` names an existing conversation.
/// - C5: that conversation is awaiting client input.
/// - E1: `!(C1 && C2 && C3)` -> HTTP 400 A2A error envelope.
/// - E2: `C1 && C2 && C3 && !C4` -> completed task and new history.
/// - E3: `C1 && C2 && C3 && C4 && !C5` -> completed task with accumulated history.
/// - E4: `C1 && C2 && C3 && C4 && C5` -> awaited run resumes to completion.
///
/// Decision table:
///
/// | rule | canonical wire | existing context | awaiting input | expected effect |
/// |------|----------------|------------------|----------------|-----------------|
/// | R1   | no             | -                | -              | E1 / 400        |
/// | R2   | yes            | no               | no             | E2 / completed  |
/// | R3   | yes            | yes              | no             | E3 / accumulated|
/// | R4   | yes            | yes              | yes            | E4 / resumed    |
///
/// `a2a_malformed_body_returns_error_envelope` covers R1; the three positive
/// tests below cover R2-R4 respectively.
///
/// An A2A `message:send` body: a user message on `context` carrying `text`.
fn a2a_send(context: &str, msg_id: &str, text: &str) -> Value {
    json!({ "message": {
        "kind": "message",
        "messageId": msg_id,
        "contextId": context,
        "role": "user",
        "parts": [{ "kind": "text", "text": text }]
    }})
}

#[tokio::test(flavor = "multi_thread")]
async fn a2a_run_returns_completed_task() {
    // Causes: the fixtures below establish `a2a run` with the concrete inputs, state, dependencies,
    // and failure triggers used by this case.
    // Effects: the observable result `returns completed task` and every asserted state transition
    // or side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let app = build_echo_router();
    let (status, body) = call(
        &app,
        "POST",
        "/v1/a2a/message:send",
        a2a_send("a2a-t1", "m1", "hi"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let task = serde_json::from_str::<Value>(&body).unwrap()["task"].clone();
    assert_eq!(task["contextId"], "a2a-t1");
    assert_eq!(task["status"]["state"], "completed");
    assert_eq!(task["status"]["message"]["parts"][0]["text"], "Echo: hi");
    // The task history carries both the user input and the agent reply.
    let history = task["history"].as_array().unwrap();
    assert!(
        history
            .iter()
            .any(|m| m["role"] == "user" && m["parts"][0]["text"] == "hi")
    );
    assert!(
        history
            .iter()
            .any(|m| m["role"] == "agent" && m["parts"][0]["text"] == "Echo: hi")
    );
}

/// Cause/effect design: C1 two A2A sends use the same context id in sequence.
/// Effect E1: the second Task history retains the first input/reply and current
/// input instead of resetting the context. Decision rule A1=C1=>E1; this test's
/// membership assertions cover accumulation, not a separate ordering contract.
#[tokio::test(flavor = "multi_thread")]
async fn a2a_history_accumulates_across_runs() {
    // Causes: the fixtures below establish `a2a history` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `accumulates across runs` and every asserted state transition
    // or side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let app = build_echo_router();
    call(
        &app,
        "POST",
        "/v1/a2a/message:send",
        a2a_send("a2a-h", "m1", "one"),
    )
    .await;
    let (_, body) = call(
        &app,
        "POST",
        "/v1/a2a/message:send",
        a2a_send("a2a-h", "m2", "two"),
    )
    .await;
    let task = serde_json::from_str::<Value>(&body).unwrap()["task"].clone();
    let texts: Vec<String> = task["history"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["parts"][0]["text"].as_str().map(str::to_string))
        .collect();
    // The returned history retains the prior exchange plus the current input.
    assert!(texts.contains(&"one".to_string()));
    assert!(texts.contains(&"Echo: one".to_string()));
    assert!(texts.contains(&"two".to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a2a_awaits_input_required_then_resumes_completed() {
    let app = build_custom_router();
    // A managed session fixes the shared context id.
    let (_, created) = call(
        &app,
        "POST",
        "/v1/sessions",
        json!({
            "agent": "assistant",
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
        }),
    )
    .await;
    let thread = serde_json::from_str::<Value>(&created).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // A2A drives a Run; the model calls the client tool and the task awaits.
    let (status, body) = call(
        &app,
        "POST",
        "/v1/a2a/message:send",
        a2a_send(&thread, "m1", "answer please"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let task = serde_json::from_str::<Value>(&body).unwrap()["task"].clone();
    assert_eq!(
        task["status"]["state"], "input-required",
        "a client-tool await is input-required: {body}"
    );

    // A2A delivers the awaited input on the SAME context; the run resumes to done.
    let (_, body) = call(
        &app,
        "POST",
        "/v1/a2a/message:send",
        a2a_send(&thread, "m2", "42"),
    )
    .await;
    let task = serde_json::from_str::<Value>(&body).unwrap()["task"].clone();
    assert_eq!(task["status"]["state"], "completed");
    assert!(
        task["history"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["parts"][0]["text"]
                .as_str()
                .map(|t| t.contains("got: 42"))
                .unwrap_or(false)),
        "the resumed answer appears in the task history: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a2a_malformed_body_returns_error_envelope() {
    // A2A is request/response, so a decode failure is an HTTP 400 + JSON error
    // envelope (like Managed), not an in-stream event.
    let app = build_echo_router();
    let (status, body) = call(
        &app,
        "POST",
        "/v1/a2a/message:send",
        json!({ "message": 5 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let err = serde_json::from_str::<Value>(&body).unwrap();
    assert!(
        err["error"]["code"].is_number(),
        "error envelope has a code: {body}"
    );
    assert!(
        err["error"]["message"].is_string(),
        "error envelope has a message: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a2a_agent_card_advertises_the_protocol() {
    let app = build_echo_router();
    let (status, body) = call(&app, "GET", "/v1/a2a/agent-card", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let card = serde_json::from_str::<Value>(&body).unwrap();
    assert_eq!(card["protocolVersion"], "0.3.0");
    assert_eq!(card["capabilities"]["streaming"], true);
}
