//! Server integration tests through the *real* kernel: the echo path, and a full
//! HITL round-trip where a mutating tool awaits for approval, is confirmed, runs
//! rooted in the session's sandbox, and the read-back proves isolation.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_scenario_host::{
    EchoModel, ReviseModel, build_custom_router, build_delegation_router, build_graded_router,
    build_router,
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
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        status,
        StatusCode::OK,
        "{method} {uri}: {}",
        String::from_utf8_lossy(&bytes)
    );
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
    // Cause/effect rule: accepting a user.message first persists that exact
    // inbound event, then brackets the resulting turn with running/output/idle.
    assert_eq!(
        event_types(&list),
        vec![
            "user.message",
            "session.status_running",
            "agent.message",
            "session.status_idle"
        ]
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
/// reply. `write` is asked (awaits); `read` is allowed (runs).
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
            .filter(|m| m.role == Role::Tool)
            .count();
        let user_text = request
            .messages
            .iter()
            .find(|m| m.role == Role::User)
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
            stop_reason: None,
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
async fn hitl_write_awaits_then_confirms_and_reads_rooted() {
    let app = build_router(Arc::new(WriteReadProbe), "scripted");
    let id = create_session(&app).await;

    // The write is asked -> the run awaits.
    let list = send_message(&app, &id, "HELLO-SANDBOX").await;
    // Decision rule: user.message + permission-gated tool call persists the
    // input, starts the turn, projects the request, then idles awaiting action.
    assert_eq!(
        event_types(&list),
        vec![
            "user.message",
            "session.status_running",
            "agent.tool_use",
            "session.status_idle"
        ]
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

async fn define_outcome(app: &Router, session: &str, rubric: &str) -> serde_json::Value {
    json_call(
        app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        serde_json::json!({ "events": [{ "type": "user.define_outcome", "description": "finish it", "rubric": { "type": "text", "content": rubric }, "max_iterations": 3 }] }),
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
    assert_eq!(
        ends.first().unwrap()["result"],
        "needs_revision",
        "outcome evaluation events: {ends:?}"
    );
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

/// Deterministic Outcome Worker/Judge pair: the Worker first crosses the
/// protected `write` boundary and, after the ordinary Run resume commits the
/// permission result, produces the rubric marker. The Judge then accepts it.
struct OutcomeHitlModel;

#[async_trait::async_trait]
impl LlmExecutor for OutcomeHitlModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(|message| {
                message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>()
            })
            .unwrap_or_default();
        let tool_results = request
            .messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .count();
        let output = if last_user.contains("Evaluate this Outcome input") {
            AssistantOutput::text(
                r#"{"result":"satisfied","explanation":"the resumed Worker produced FINAL"}"#,
            )
        } else if tool_results >= 2 {
            AssistantOutput::text("FINAL after permission")
        } else {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: format!("outcome-write-{}", tool_results + 1),
                tool_id: "write".into(),
                arguments: serde_json::json!({
                    "path": format!("outcome-{tool_results}.txt"),
                    "content": "permission crossed"
                }),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn outcome_hitl_awaits_without_failure_then_resumes_the_active_aggregate() {
    let app = build_router(Arc::new(OutcomeHitlModel), "outcome-hitl");
    let id = create_session(&app).await;

    // Cause/effect decision table for the changed Outcome/HITL boundary:
    // R1 C={active Outcome, protected Worker tool, no result} -> E={successful
    // receipt, requires_action, no evaluation/error}; R2 C={same aggregate,
    // matching allow, another protected call} -> E={ordinary Run commit, same
    // aggregate awaits again}; R3 C={same aggregate, matching deny, Worker
    // reaches natural end} -> E={denial committed, aggregate continuation,
    // satisfied evaluation}; R4 C={no active Outcome} -> E={ordinary resume
    // only}, covered by `hitl_write_awaits_then_confirms_and_reads_rooted`;
    // mismatched result rejection remains covered by Managed admission tests.
    let awaiting = define_outcome(&app, &id, "FINAL").await;
    let awaiting_events = awaiting["data"].as_array().unwrap();
    assert!(
        awaiting_events
            .iter()
            .any(|event| { event["type"] == "agent.tool_use" && event["id"] == "outcome-write-1" })
    );
    assert!(
        !awaiting_events
            .iter()
            .any(|event| event["type"] == "session.error")
    );
    assert!(
        !awaiting_events
            .iter()
            .any(|event| event["type"] == "span.outcome_evaluation_end")
    );
    let idle = awaiting_events.last().unwrap();
    assert_eq!(idle["stop_reason"]["type"], "requires_action");
    assert_eq!(idle["stop_reason"]["event_ids"][0], "outcome-write-1");

    let awaiting_again = confirm(&app, &id, "outcome-write-1").await;
    let awaiting_again_events = awaiting_again["data"].as_array().unwrap();
    assert!(
        awaiting_again_events
            .iter()
            .any(|event| { event["type"] == "agent.tool_use" && event["id"] == "outcome-write-2" })
    );
    assert!(
        !awaiting_again_events
            .iter()
            .any(|event| event["type"] == "span.outcome_evaluation_end")
    );
    assert_eq!(
        awaiting_again_events.last().unwrap()["stop_reason"]["type"],
        "requires_action"
    );

    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({
            "events": [{
                "type": "user.tool_confirmation",
                "tool_use_id": "outcome-write-2",
                "result": "deny",
                "deny_message": "continue without the second write"
            }]
        }),
    )
    .await;
    let completed = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    let completed_events = completed["data"].as_array().unwrap();
    assert!(
        !completed_events
            .iter()
            .any(|event| event["type"] == "session.error")
    );
    let evaluations = completed_events
        .iter()
        .filter(|event| event["type"] == "span.outcome_evaluation_end")
        .collect::<Vec<_>>();
    assert_eq!(evaluations.len(), 1);
    assert_eq!(evaluations[0]["result"], "satisfied");
    assert_eq!(
        completed_events
            .iter()
            .filter(|event| event["type"] == "agent.tool_use")
            .count(),
        2,
        "each committed tool call must be projected exactly once"
    );
    assert!(completed_events.iter().any(|event| {
        event["type"] == "agent.message" && event["content"][0]["text"] == "FINAL after permission"
    }));
}

/// A model serving both roles for the judge-graded outcome test: as a judge (it
/// sees the grading prompt) it returns a JSON verdict — met iff the deliverable
/// carries the `FINAL` marker; as the doer it drafts, then revises to `FINAL`
/// once it sees the loop's feedback.
struct GradedModel;

#[async_trait::async_trait]
impl LlmExecutor for GradedModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
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
        let reply = if last_user.contains("Evaluate this Outcome input") {
            // Judge: the deliverable is met iff it carries the FINAL marker.
            if last_user.contains("FINAL answer") {
                r#"{"result": "satisfied", "explanation": "carries the marker"}"#.to_string()
            } else {
                r#"{"result": "needs_revision", "explanation": "add the completion marker"}"#
                    .to_string()
            }
        } else if last_user.contains("Revise the deliverable") {
            "FINAL answer".to_string()
        } else {
            "a rough draft".to_string()
        };
        Ok(ChatResponse {
            output: AssistantOutput::text(reply),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn outcome_graded_by_a_judge_subagent() {
    // The rubric is prose, not a keyword — a keyword grader would never match it
    // and would exhaust the budget. The judge sub-agent grades it instead, so the
    // revision that adds the FINAL marker is accepted.
    let app = build_graded_router(Arc::new(GradedModel), "scripted", "judge");
    let id = create_session(&app).await;

    send_message(&app, &id, "write something").await;
    let list = define_outcome(&app, &id, "the deliverable is complete").await;

    let ends: Vec<&serde_json::Value> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "span.outcome_evaluation_end")
        .collect();
    assert!(ends.len() >= 2, "expected at least two judge-graded rounds");
    assert_eq!(
        ends.first().unwrap()["result"],
        "needs_revision",
        "outcome evaluation events: {ends:?}"
    );
    assert_eq!(ends.last().unwrap()["result"], "satisfied");
}

#[tokio::test]
async fn custom_tool_use_through_real_kernel() {
    let app = build_custom_router();
    let id = create_session(&app).await;

    // The model calls the client-executed tool `submit_answer` -> awaits as custom.
    let list = send_message(&app, &id, "solve it").await;
    // Decision rule: user.message + client-executed tool persists the input,
    // starts the turn, projects custom_tool_use, then idles awaiting its result.
    assert_eq!(
        event_types(&list),
        vec![
            "user.message",
            "session.status_running",
            "agent.custom_tool_use",
            "session.status_idle"
        ]
    );
    let custom = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.custom_tool_use")
        .unwrap();
    assert_eq!(custom["name"], "submit_answer");
    let tool_use_id = custom["id"].as_str().unwrap().to_string();
    let idle = list["data"].as_array().unwrap().last().unwrap();
    assert_eq!(idle["stop_reason"]["type"], "requires_action");

    // The client returns the result -> the model incorporates it and replies.
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.custom_tool_result", "custom_tool_use_id": tool_use_id, "content": [{ "type": "text", "text": "42" }] }] }),
    )
    .await;
    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    let msgs: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "agent.message")
        .map(|e| e["content"][0]["text"].as_str().unwrap())
        .collect();
    assert!(
        msgs.iter().any(|m| m.contains("got: 42")),
        "the client's result reached the model: {msgs:?}"
    );
    assert_eq!(
        list["data"].as_array().unwrap().last().unwrap()["stop_reason"]["type"],
        "end_turn"
    );
}

/// A model that replies with every system message it can see, so a test can prove
/// a `system.message` reached the turn's context.
struct SystemEchoModel;

#[async_trait::async_trait]
impl LlmExecutor for SystemEchoModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let system: String = request
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(" | ");
        Ok(ChatResponse {
            output: AssistantOutput::text(format!("system says: {system}")),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn system_message_reaches_next_turn() {
    let app = build_router(Arc::new(SystemEchoModel), "sys");
    let id = create_session(&app).await;

    // A `system.message` is accept-only (no agent/session projection) but its
    // canonical inbound event is persisted and the directive is buffered.
    let receipts = json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "system.message", "content": [{ "type": "text", "text": "SECRET-DIRECTIVE" }] }] }),
    )
    .await;
    assert_eq!(receipts["data"][0]["type"], "system.message");
    let before = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    // Cause/effect rule: accepted system.message -> one processed inbound event,
    // with no running/message/idle projection until a later user turn.
    assert_eq!(event_types(&before), vec!["system.message"]);
    assert!(before["data"][0]["processed_at"].is_string());

    // The next user turn sees the buffered directive.
    let list = send_message(&app, &id, "hello").await;
    let messages: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "agent.message")
        .map(|e| e["content"][0]["text"].as_str().unwrap())
        .collect();
    assert!(
        messages.iter().any(|m| m.contains("SECRET-DIRECTIVE")),
        "directive reached the turn: {messages:?}"
    );
}

/// POST an events batch and return the HTTP status (no success assertion).
async fn post_status(app: &Router, uri: &str, body: serde_json::Value) -> StatusCode {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn custom_result_fails_closed_on_mismatch() {
    let app = build_custom_router();
    let id = create_session(&app).await;
    send_message(&app, &id, "solve it").await; // awaits on submit_answer (id "c1")

    // A result naming the wrong tool_use_id is rejected...
    let status = post_status(
        &app,
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.custom_tool_result", "custom_tool_use_id": "WRONG", "content": [{ "type": "text", "text": "42" }] }] }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "mismatched id must fail closed"
    );

    // ...and a `user.tool_confirmation` is rejected too (this await is
    // client-executed, not a built-in awaiting approval).
    let status = post_status(
        &app,
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.tool_confirmation", "tool_use_id": "c1", "result": "allow" }] }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "wrong binding must fail closed"
    );

    // The await survives both rejections; the correct result still resumes it.
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.custom_tool_result", "custom_tool_use_id": "c1", "content": [{ "type": "text", "text": "42" }] }] }),
    )
    .await;
    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    let msgs: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "agent.message")
        .map(|e| e["content"][0]["text"].as_str().unwrap())
        .collect();
    assert!(
        msgs.iter().any(|m| m.contains("got: 42")),
        "correct result resumes: {msgs:?}"
    );
}

#[tokio::test]
async fn custom_result_cannot_fabricate_a_builtin_tools_output() {
    // A run awaiting on the *built-in* `write` (HITL) must not be resumable with a
    // `user.custom_tool_result`: that would bypass execution and the approval gate.
    let app = build_router(Arc::new(WriteReadProbe), "scripted");
    let id = create_session(&app).await;
    send_message(&app, &id, "HELLO").await; // awaits on write (id "w")

    let status = post_status(
        &app,
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.custom_tool_result", "custom_tool_use_id": "w", "content": [{ "type": "text", "text": "forged" }] }] }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "built-in await must reject a custom result"
    );

    // The proper confirmation path still runs the real tool.
    let list = confirm(&app, &id, "w").await;
    assert!(read_result_text(&list).contains("HELLO"));
    assert!(!read_result_text(&list).contains("forged"));
}

/// Every agent text a turn produced: assistant messages and tool results.
fn all_agent_text(list: &serde_json::Value) -> String {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "agent.message" || e["type"] == "agent.tool_result")
        .filter_map(|e| e["content"][0]["text"].as_str())
        .collect::<Vec<_>>()
        .join(" | ")
}

#[tokio::test]
async fn delegation_runs_a_subagent_and_returns_its_result() {
    let app = build_delegation_router();
    let id = create_session(&app).await;
    let list = send_message(&app, &id, "research the answer").await;

    // `agent_run` awaits and the host fulfills it: a sub-run executes and its
    // result flows back transparently within the turn.
    let msgs: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "agent.message")
        .map(|e| e["content"][0]["text"].as_str().unwrap())
        .collect();
    assert!(
        msgs.iter()
            .any(|m| m.contains("delegate said: researched: 42")),
        "sub-agent output reached the main agent: {msgs:?}"
    );
    assert_eq!(
        list["data"].as_array().unwrap().last().unwrap()["stop_reason"]["type"],
        "end_turn"
    );
}

#[tokio::test]
async fn delegation_fails_closed_on_unpublished_target() {
    // `ghost` is not in the Agent's published targets; the child Run must never start.
    let app = build_delegation_router();
    let id = create_session(&app).await;
    let list = send_message(&app, &id, "use the ghost agent").await;

    let text = all_agent_text(&list);
    assert!(
        text.contains("published targets"),
        "published-target rejection is surfaced: {text}"
    );
    assert!(
        !text.contains("researched: 42"),
        "no sub-run output leaked: {text}"
    );
}

/// A model that reports the tool ids it was offered, so a test can assert the
/// advertised catalog matches what the runtime can actually execute.
struct ToolCatalogProbe;

#[async_trait::async_trait]
impl LlmExecutor for ToolCatalogProbe {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let mut ids: Vec<&str> = request.tools.iter().map(|t| t.id.as_str()).collect();
        ids.sort_unstable();
        Ok(ChatResponse {
            output: AssistantOutput::text(ids.join(",")),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn advertises_only_runnable_tools() {
    let app = build_router(Arc::new(ToolCatalogProbe), "probe");
    let id = create_session(&app).await;
    let list = send_message(&app, &id, "which tools do you have?").await;
    let offered = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.message")
        .unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();

    // The tools the assembly registers an executable for are offered...
    for tool in ["read", "write", "edit", "glob", "grep", "bash"] {
        assert!(offered.contains(tool), "expected `{tool}` in {offered:?}");
    }
    // ...and the ones it does not (network tools with no egress policy) are not.
    for tool in ["web_fetch", "web_search"] {
        assert!(
            !offered.contains(tool),
            "`{tool}` must not be advertised: {offered:?}"
        );
    }
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
            stop_reason: None,
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

/// A model that blocks on its second inference (the first grader run) so a
/// concurrent `user.interrupt` HTTP request can land mid-outcome.
struct GatedReviseModel {
    gate: std::sync::Arc<tokio::sync::Notify>,
    reached: std::sync::Arc<tokio::sync::Notify>,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for GatedReviseModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        use std::sync::atomic::Ordering;
        if self.calls.fetch_add(1, Ordering::SeqCst) == 2 {
            self.reached.notify_one();
            self.gate.notified().await;
        }
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(|message| {
                message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>()
            })
            .unwrap_or_default();
        let reply = if last_user.contains("Evaluate this Outcome input") {
            r#"{"result":"needs_revision","explanation":"include FINAL"}"#
        } else {
            "a rough draft"
        };
        Ok(ChatResponse {
            output: AssistantOutput::text(reply),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_user_interrupt_reports_interrupted() {
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    let reached = std::sync::Arc::new(tokio::sync::Notify::new());
    let app = build_router(
        Arc::new(GatedReviseModel {
            gate: gate.clone(),
            reached: reached.clone(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        "scripted",
    );
    let id = create_session(&app).await;

    // The model blocks the first Agent-backed grading run. Drive it on a task so
    // the test can interrupt concurrently.
    let app2 = app.clone();
    let id2 = id.clone();
    let task = tokio::spawn(async move {
        json_call(
            &app2,
            "POST",
            &format!("/v1/sessions/{id2}/events"),
            serde_json::json!({ "events": [{ "type": "user.define_outcome", "description": "finish it", "rubric": { "type": "text", "content": "FINAL" }, "max_iterations": 5 }] }),
        )
        .await
    });

    // Once the loop is blocked mid-run, send `user.interrupt` on a concurrent
    // request, then release the gate.
    reached.notified().await;
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.interrupt" }] }),
    )
    .await;
    gate.notify_one();
    task.await.unwrap();

    // The projected outcome ends `interrupted`, not satisfied/max_iterations.
    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    let ends: Vec<&serde_json::Value> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "span.outcome_evaluation_end")
        .collect();
    assert_eq!(
        ends.iter()
            .filter(|event| event["result"] == "interrupted")
            .count(),
        1,
        "the terminal interruption is projected exactly once: {ends:?}"
    );
    assert_eq!(
        ends.last().unwrap()["result"],
        "interrupted",
        "user.interrupt must end the outcome as interrupted"
    );
}
