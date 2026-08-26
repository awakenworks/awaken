//! Server integration tests through the *real* kernel: the echo path, and a full
//! HITL round-trip where a mutating tool awaits for approval, is confirmed, runs
//! rooted in the session's sandbox, and the read-back proves isolation.

mod support;

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

use support::wait_for_session_events;

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

fn last_event_of_type<'a>(
    events: &'a [serde_json::Value],
    event_type: &str,
) -> &'a serde_json::Value {
    events
        .iter()
        .rev()
        .find(|event| event["type"] == event_type)
        .unwrap_or_else(|| panic!("missing {event_type} event"))
}

async fn create_session(app: &Router) -> String {
    let s = json_call(
        app,
        "POST",
        "/v1/sessions",
        serde_json::json!({
            "agent": "assistant",
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
        }),
    )
    .await;
    s["id"].as_str().unwrap().to_string()
}

async fn send_message(app: &Router, session: &str, text: &str) -> serde_json::Value {
    let receipt = json_call(
        app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": text }] }] }),
    )
    .await;
    wait_for_session_events(
        app,
        session,
        Some(&receipt),
        "the accepted user.message to reach an aggregate idle boundary",
        |events| {
            events
                .iter()
                .any(|event| event["type"] == "session.status_idle")
        },
    )
    .await
}

async fn confirm(app: &Router, session: &str, tool_use_id: &str) -> serde_json::Value {
    let before = json_call(
        app,
        "GET",
        &format!("/v1/sessions/{session}/events?limit=500"),
        serde_json::Value::Null,
    )
    .await;
    let prior_pending_tool_ids = before["data"]
        .as_array()
        .expect("Managed Event page")
        .iter()
        .filter(|event| {
            event["type"] == "session.status_idle"
                && event["stop_reason"]["type"] == "requires_action"
        })
        .flat_map(|event| {
            event["stop_reason"]["event_ids"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .filter_map(serde_json::Value::as_str)
        .map(str::to_owned)
        .collect::<std::collections::HashSet<_>>();
    let receipt = json_call(
        app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        serde_json::json!({ "events": [{ "type": "user.tool_confirmation", "tool_use_id": tool_use_id, "result": "allow" }] }),
    )
    .await;
    wait_for_session_events(
        app,
        session,
        Some(&receipt),
        "the accepted tool confirmation to reach an aggregate idle boundary",
        |events| {
            events.iter().any(|event| {
                if event["type"] != "session.status_idle" {
                    return false;
                }
                if event["stop_reason"]["type"] != "requires_action" {
                    return true;
                }
                event["stop_reason"]["event_ids"]
                    .as_array()
                    .is_some_and(|ids| {
                        ids.iter()
                            .filter_map(serde_json::Value::as_str)
                            .any(|id| !prior_pending_tool_ids.contains(id))
                    })
            })
        },
    )
    .await
}

#[tokio::test]
async fn echo_run_end_to_end() {
    // Causes: the fixtures below establish `echo run end to end` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let app = build_router(Arc::new(EchoModel), "echo-model");
    let id = create_session(&app).await;
    let list = send_message(&app, &id, "hi there").await;
    // Cause/effect rule: accepting a user.message first persists that exact
    // inbound Event, then brackets the resulting Run with both aggregate and
    // primary-Thread lifecycle before usage and aggregate idle.
    assert_eq!(
        event_types(&list),
        vec![
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "span.model_request_start",
            "span.model_request_end",
            "agent.message",
            "session.thread_status_idle",
            "session.usage",
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
    // Causes: the fixtures below establish `hitl write awaits then confirms and reads rooted` with
    // the concrete inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    let app = build_router(Arc::new(WriteReadProbe), "scripted");
    let id = create_session(&app).await;

    // The write is asked -> the Run awaits.
    let list = send_message(&app, &id, "HELLO-SANDBOX").await;
    // Decision rule: user.message + permission-gated tool call persists the
    // input, starts the Run, projects the request, then idles awaiting action.
    assert_eq!(
        event_types(&list),
        vec![
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "span.model_request_start",
            "span.model_request_end",
            "agent.tool_use",
            "session.thread_status_idle",
            "session.usage",
            "session.status_idle"
        ]
    );
    let tool_use_id = last_event_of_type(list["data"].as_array().unwrap(), "agent.tool_use")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let idle = last_event_of_type(list["data"].as_array().unwrap(), "session.status_idle");
    assert_eq!(idle["stop_reason"]["type"], "requires_action");
    assert_eq!(idle["stop_reason"]["event_ids"][0], tool_use_id);

    // Confirm -> write runs (rooted), read runs (allowed), reply.
    let list = confirm(&app, &id, &tool_use_id).await;
    assert!(event_types(&list).contains(&"agent.message".to_string()));
    let last = last_event_of_type(list["data"].as_array().unwrap(), "session.status_idle");
    assert_eq!(last["stop_reason"]["type"], "end_turn");
    assert!(read_result_text(&list).contains("HELLO-SANDBOX"));
}

async fn define_outcome(app: &Router, session: &str, rubric: &str) -> serde_json::Value {
    let receipt = json_call(
        app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        serde_json::json!({ "events": [{ "type": "user.define_outcome", "description": "finish it", "rubric": { "type": "text", "content": rubric }, "max_iterations": 3 }] }),
    )
    .await;
    wait_for_session_events(
        app,
        session,
        Some(&receipt),
        "the accepted Outcome to become satisfied or await required action",
        |events| {
            events.iter().any(|event| {
                (event["type"] == "span.outcome_evaluation_end" && event["result"] == "satisfied")
                    || (event["type"] == "session.status_idle"
                        && event["stop_reason"]["type"] == "requires_action")
            })
        },
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
    // Causes: the fixtures below establish `outcome hitl awaits without failure then` with the
    // concrete inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `resumes the active aggregate` and every asserted state
    // transition or side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
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
    let first_tool_id = last_event_of_type(awaiting_events, "agent.tool_use")["id"]
        .as_str()
        .expect("first Outcome tool Event id")
        .to_string();
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
    let idle = last_event_of_type(awaiting_events, "session.status_idle");
    assert_eq!(idle["stop_reason"]["type"], "requires_action");
    assert_eq!(idle["stop_reason"]["event_ids"][0], first_tool_id);

    let awaiting_again = confirm(&app, &id, &first_tool_id).await;
    let awaiting_again_events = awaiting_again["data"].as_array().unwrap();
    let second_tool_id = last_event_of_type(awaiting_again_events, "agent.tool_use")["id"]
        .as_str()
        .expect("second Outcome tool Event id")
        .to_string();
    assert_ne!(
        second_tool_id, first_tool_id,
        "R2 uses a fresh public Event id"
    );
    assert!(
        !awaiting_again_events
            .iter()
            .any(|event| event["type"] == "span.outcome_evaluation_end")
    );
    assert_eq!(
        last_event_of_type(awaiting_again_events, "session.status_idle")["stop_reason"]["type"],
        "requires_action"
    );

    let deny_receipt = json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({
            "events": [{
                "type": "user.tool_confirmation",
                "tool_use_id": second_tool_id,
                "result": "deny",
                "deny_message": "continue without the second write"
            }]
        }),
    )
    .await;
    let completed = wait_for_session_events(
        &app,
        &id,
        Some(&deny_receipt),
        "the denied second tool call to commit the final Outcome answer and verdict",
        |events| {
            events.iter().any(|event| {
                event["type"] == "span.outcome_evaluation_end" && event["result"] == "satisfied"
            }) && events.iter().any(|event| {
                event["type"] == "agent.message"
                    && event["content"][0]["text"] == "FINAL after permission"
            })
        },
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
    // Causes: the fixtures below establish `custom tool use through real kernel` with the concrete
    // inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    let app = build_custom_router();
    let id = create_session(&app).await;

    // The model calls the client-executed tool `submit_answer` -> awaits as custom.
    let list = send_message(&app, &id, "solve it").await;
    // Decision rule: user.message + client-executed tool persists the input,
    // starts the Run, projects custom_tool_use, then idles awaiting its result.
    assert_eq!(
        event_types(&list),
        vec![
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "span.model_request_start",
            "span.model_request_end",
            "agent.custom_tool_use",
            "session.thread_status_idle",
            "session.usage",
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
    let idle = last_event_of_type(list["data"].as_array().unwrap(), "session.status_idle");
    assert_eq!(idle["stop_reason"]["type"], "requires_action");

    // The client returns the result -> the model incorporates it and replies.
    let result_receipt = json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.custom_tool_result", "custom_tool_use_id": tool_use_id, "content": [{ "type": "text", "text": "42" }] }] }),
    )
    .await;
    let list = wait_for_session_events(
        &app,
        &id,
        Some(&result_receipt),
        "the accepted custom tool result to reach the model answer and end_turn",
        |events| {
            events.iter().any(|event| {
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
        last_event_of_type(list["data"].as_array().unwrap(), "session.status_idle")["stop_reason"]
            ["type"],
        "end_turn"
    );
}

/// A model that replies with every system message it can see, so a test can prove
/// a `system.message` reached the Run's context.
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
async fn system_message_reaches_accompanying_and_later_runs() {
    // Causes: the fixtures below establish `system message` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the accompanying Run and every later Run observe the one committed System context.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let app = build_router(Arc::new(SystemEchoModel), "sys");
    let id = create_session(&app).await;

    // Cause/effect graph: C1=one text System event; C2=it is final and follows
    // one User event; C3=a later User event starts another Run. Effects:
    // E1=the first batch is accepted in public order; E2=the accompanying Run
    // sees the System context; E3=the later Run retains it. Decision rule
    // S1=C1+C2+C3 -> E1+E2+E3. Invalid predecessor/cardinality rules live in the
    // Managed adapter decision table and must fail before any Run is admitted.
    let receipts = json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [
            { "type": "user.message", "content": [{ "type": "text", "text": "first" }] },
            { "type": "system.message", "content": [{ "type": "text", "text": "SECRET-DIRECTIVE" }] }
        ] }),
    )
    .await;
    assert_eq!(
        receipts["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["user.message", "system.message"],
        "S1/E1"
    );
    let first = wait_for_session_events(
        &app,
        &id,
        Some(&receipts),
        "the accepted system.message to affect the accompanying Run",
        |events| {
            events.iter().any(|event| {
                event["type"] == "agent.message"
                    && event["content"][0]["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("SECRET-DIRECTIVE"))
            }) && events
                .iter()
                .any(|event| event["type"] == "session.status_idle")
        },
    )
    .await;
    let first_messages = first["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "agent.message")
        .map(|event| event["content"][0]["text"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(
        first_messages
            .iter()
            .any(|message| message.contains("SECRET-DIRECTIVE")),
        "S1/E2 accompanying Run input: {first_messages:?}"
    );

    let list = send_message(&app, &id, "later").await;
    let messages: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "agent.message")
        .map(|e| e["content"][0]["text"].as_str().unwrap())
        .collect();
    assert!(
        messages.iter().any(|m| m.contains("SECRET-DIRECTIVE")),
        "S1/E3 later Run input: {messages:?}"
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
    // Causes: the fixtures below establish `custom result` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `fails closed on mismatch` and every asserted state transition
    // or side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Coverage rationale: `custom result` is one independent branch selecting `fails closed on
    // mismatch`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    let app = build_custom_router();
    let id = create_session(&app).await;
    let awaiting = send_message(&app, &id, "solve it").await;
    let tool_use_id = last_event_of_type(
        awaiting["data"].as_array().unwrap(),
        "agent.custom_tool_use",
    )["id"]
        .as_str()
        .expect("public custom tool Event id")
        .to_string();

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
        serde_json::json!({ "events": [{ "type": "user.tool_confirmation", "tool_use_id": tool_use_id, "result": "allow" }] }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "wrong binding must fail closed"
    );

    // The await survives both rejections; the correct result still resumes it.
    let result_receipt = json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.custom_tool_result", "custom_tool_use_id": tool_use_id, "content": [{ "type": "text", "text": "42" }] }] }),
    )
    .await;
    let list = wait_for_session_events(
        &app,
        &id,
        Some(&result_receipt),
        "the still-pending custom tool call to resume from its correctly bound result",
        |events| {
            events.iter().any(|event| {
                event["type"] == "agent.message"
                    && event["content"][0]["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("got: 42"))
            })
        },
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
    // Causes: the fixtures below establish `custom result` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `cannot fabricate a builtin tools output` and every asserted
    // state transition or side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Coverage rationale: `custom result` is one independent branch selecting `cannot fabricate a
    // builtin tools output`; a multi-row decision table is not applicable, and sibling tests own
    // alternate causes.
    // A Run awaiting on the *built-in* `write` (HITL) must not be resumable with a
    // `user.custom_tool_result`: that would bypass execution and the approval gate.
    let app = build_router(Arc::new(WriteReadProbe), "scripted");
    let id = create_session(&app).await;
    let awaiting = send_message(&app, &id, "HELLO").await;
    let tool_use_id =
        last_event_of_type(awaiting["data"].as_array().unwrap(), "agent.tool_use")["id"]
            .as_str()
            .expect("public built-in tool Event id")
            .to_string();

    let status = post_status(
        &app,
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.custom_tool_result", "custom_tool_use_id": tool_use_id, "content": [{ "type": "text", "text": "forged" }] }] }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "built-in await must reject a custom result"
    );

    // The proper confirmation path still runs the real tool.
    let list = confirm(&app, &id, &tool_use_id).await;
    assert!(read_result_text(&list).contains("HELLO"));
    assert!(!read_result_text(&list).contains("forged"));
}

/// Every agent text a Run produced: assistant messages and tool results.
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
    // Causes: the fixtures below establish `delegation runs a subagent and` with the concrete
    // inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `returns its result` and every asserted state transition or
    // side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let app = build_delegation_router();
    let id = create_session(&app).await;
    send_message(&app, &id, "research the answer").await;
    let list = wait_for_session_events(
        &app,
        &id,
        None,
        "the committed child report and its one later root report Run",
        |events| {
            let child_report = events.iter().any(|event| {
                event["type"] == "agent.thread_message_received"
                    && event["content"][0]["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("researched: 42"))
            });
            let root_report = events.iter().any(|event| {
                event["type"] == "agent.message"
                    && event["content"][0]["text"] == "coordination completed from child report"
            });
            child_report
                && root_report
                && events
                    .iter()
                    .rev()
                    .any(|event| event["type"] == "session.status_idle")
        },
    )
    .await;

    // Cause/effect rule: list_agents resolves the frozen roster, send_to_agent
    // returns only an admission receipt, the child executes on its own Thread,
    // and its terminal report admits exactly one later root report Run.
    let msgs: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "agent.message")
        .map(|e| e["content"][0]["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        msgs.iter()
            .filter(|message| message.contains("coordination accepted:"))
            .count(),
        1,
        "one admission receipt: {msgs:?}"
    );
    assert_eq!(
        msgs.iter()
            .filter(|message| **message == "coordination completed from child report")
            .count(),
        1,
        "one report Run: {msgs:?}"
    );
    assert!(
        !msgs
            .iter()
            .any(|message| message.contains("researched: 42")),
        "child payload is not a synchronous result: {msgs:?}"
    );
    let received = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "agent.thread_message_received")
        .expect("child reply cross-post");
    assert!(
        received["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("researched: 42")),
        "child Thread carries the delegate output: {received}"
    );
    assert_eq!(
        last_event_of_type(list["data"].as_array().unwrap(), "session.status_idle")["stop_reason"]
            ["type"],
        "end_turn"
    );
}

#[tokio::test]
async fn delegation_fails_closed_on_unpublished_target() {
    // Causes: the fixtures below establish `delegation` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `fails closed on unpublished target` and every asserted state
    // transition or side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect rule: `ghost` is absent from the frozen Session roster, so
    // send_to_agent fails before a child Thread or Run can be admitted.
    let app = build_delegation_router();
    let id = create_session(&app).await;
    let list = send_message(&app, &id, "use the ghost agent").await;

    let text = all_agent_text(&list);
    assert!(
        text.contains("frozen roster"),
        "frozen-roster rejection is surfaced: {text}"
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

    // Cause/effect graph and decision table:
    // C1=a tool has one executable in the assembled host; C2=a configurable Web
    // route has no provider. E1=offer it to the model; E2=omit it.
    // R1(C1,!C2)->E1 covers filesystem/shell. R2(!C1,C2)->E2 covers WebFetch and
    // WebSearch. K1 the configured-provider registry is the sole Web route owner;
    // there is no parallel static WebFetch fallback. FMECA: offering an
    // inexecutable tool causes a late tool failure (R2 detects it); omitting a real
    // executable hides a capability (R1 detects it). Permission gating is
    // independently enforced by the effective-authorization decision table.
    for tool in ["read", "write", "edit", "glob", "grep", "bash"] {
        assert!(offered.contains(tool), "expected `{tool}` in {offered:?}");
    }
    for tool in ["web_fetch", "web_search"] {
        assert!(
            !offered.contains(tool),
            "`{tool}` must not be advertised without a routed plugin: {offered:?}"
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
    // Causes: a model emits one executable `glob` call on every step until the
    // canonical loop ceiling ends the accepted user.message Run.
    // Effects: the committed aggregate terminal projection preserves
    // `retries_exhausted` instead of reporting an ordinary end_turn.
    // Constraints/invariants: the receipt-aware helper observes the lifecycle
    // supervisor's committed projection and never drives the Run itself.
    // Decision rule: M1=unbounded tool calls + reached step ceiling -> one
    // terminal `retries_exhausted`; ordinary natural-end coverage is `echo_run_end_to_end`.
    let app = build_router(Arc::new(LoopModel), "loop");
    let id = create_session(&app).await;
    let list = send_message(&app, &id, "loop forever").await;
    assert!(
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["stop_reason"]["type"] == "retries_exhausted"),
        "terminal projection must preserve retries_exhausted: {}",
        list["data"]
    );
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
    // Causes: C1 an accepted Outcome is blocked inside its active grading Run;
    // C2 a concurrent user.interrupt receipt addresses that same Session; C3 the
    // blocked model is then released so terminal reconciliation can settle.
    // Effects: exactly one committed Outcome evaluation ends as `interrupted`,
    // and neither satisfaction nor iteration exhaustion replaces it.
    // Constraints/invariants: the interrupt receipt is the causal anchor; only
    // the lifecycle supervisor may reconcile the Run and Outcome terminal facts.
    // Decision rule: I1=C1+C2+C3 -> one terminal interrupted projection; missing
    // C2 leaves the ordinary Outcome loop and is covered by the Outcome tests.
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
    let interrupt_receipt = json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.interrupt" }] }),
    )
    .await;
    gate.notify_one();
    task.await.unwrap();

    // The projected outcome ends `interrupted`, not satisfied/max_iterations.
    let list = wait_for_session_events(
        &app,
        &id,
        Some(&interrupt_receipt),
        "the accepted interrupt to commit the Outcome's interrupted verdict",
        |events| {
            events.iter().any(|event| {
                event["type"] == "span.outcome_evaluation_end" && event["result"] == "interrupted"
            })
        },
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
