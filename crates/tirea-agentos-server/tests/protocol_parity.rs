use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tirea_agentos::contracts::{AgentEvent, RunRequest};
use tirea_agentos::orchestrator::AgentDefinition;
use tirea_agentos::orchestrator::{AgentOs, AgentOsBuilder, RunStream};
use tirea_agentos_server::protocol::ag_ui::apply_agui_extensions;
use tirea_agentos_server::service::AppState;
use tirea_contract::{Message as CoreMessage, RunOrigin, ThreadReader, ThreadWriter};
use tirea_protocol_ag_ui::{Message, RunAgentInput};
use tirea_protocol_ai_sdk_v6::AiSdkV6RunRequest;
use tirea_store_adapters::MemoryStore;
use tower::ServiceExt;

mod common;

use common::{compose_http_app, post_sse, TerminatePlugin};

fn make_os_from_store(store: Arc<MemoryStore>) -> AgentOs {
    let def = AgentDefinition {
        id: "test".to_string(),
        behavior_ids: vec!["terminate_behavior_requested_parity".into()],
        ..Default::default()
    };

    AgentOsBuilder::new()
        .with_registered_behavior(
            "terminate_behavior_requested_parity",
            Arc::new(TerminatePlugin::new("terminate_behavior_requested_parity")),
        )
        .with_agent("test", def)
        .with_agent_state_store(store)
        .build()
        .unwrap()
}

fn make_os() -> AgentOs {
    make_os_from_store(Arc::new(MemoryStore::new()))
}

fn make_http_app() -> (axum::Router, Arc<MemoryStore>, Arc<AgentOs>) {
    let store = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os_from_store(store.clone()));
    let read_store: Arc<dyn ThreadReader> = store.clone();
    (
        compose_http_app(AppState {
            os: os.clone(),
            read_store,
        }),
        store,
        os,
    )
}

async fn start_active_run(
    os: &Arc<AgentOs>,
    agent_id: &str,
    thread_id: &str,
    run_id: &str,
) -> RunStream {
    let resolved = os.resolve(agent_id).expect("resolve agent");
    let request = RunRequest {
        agent_id: agent_id.to_string(),
        thread_id: Some(thread_id.to_string()),
        run_id: Some(run_id.to_string()),
        parent_run_id: None,
        parent_thread_id: None,
        resource_id: None,
        origin: RunOrigin::default(),
        state: None,
        messages: vec![],
        initial_decisions: vec![],
    };
    os.start_active_run_with_persistence(agent_id, request, resolved, false, false)
        .await
        .expect("start active run")
}

fn collect_kinds(events: &[AgentEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            AgentEvent::RunStart { .. } => "RunStart",
            AgentEvent::RunFinish { .. } => "RunFinish",
            AgentEvent::TextDelta { .. } => "TextDelta",
            AgentEvent::ToolCallStart { .. } => "ToolCallStart",
            AgentEvent::ToolCallDone { .. } => "ToolCallDone",
            AgentEvent::ToolCallReady { .. } => "ToolCallReady",
            AgentEvent::ToolCallDelta { .. } => "ToolCallDelta",
            AgentEvent::ToolCallResumed { .. } => "ToolCallResumed",
            AgentEvent::Error { .. } => "Error",
            AgentEvent::StateDelta { .. } => "StateDelta",
            AgentEvent::StateSnapshot { .. } => "StateSnapshot",
            AgentEvent::MessagesSnapshot { .. } => "MessagesSnapshot",
            AgentEvent::ActivityDelta { .. } => "ActivityDelta",
            AgentEvent::ActivitySnapshot { .. } => "ActivitySnapshot",
            AgentEvent::ReasoningDelta { .. } => "ReasoningDelta",
            AgentEvent::ReasoningEncryptedValue { .. } => "ReasoningEncryptedValue",
            AgentEvent::StepStart { .. } => "StepStart",
            AgentEvent::StepEnd => "StepEnd",
            AgentEvent::InferenceComplete { .. } => "InferenceComplete",
        })
        .collect()
}

fn normalize(run: RunRequest) -> RunRequest {
    RunRequest {
        parent_run_id: None,
        parent_thread_id: None,
        resource_id: None,
        state: None,
        ..run
    }
}

#[test]
fn agui_and_ai_sdk_inputs_map_to_equivalent_run_requests() {
    let agent_id = "test".to_string();
    let agui = RunAgentInput::new("thread_parity", "run_parity")
        .with_message(Message::user("hello parity"));
    let aisdk = AiSdkV6RunRequest::from_thread_input("thread_parity", "hello parity");

    let agui_run = normalize(agui.into_runtime_run_request(agent_id.clone()));
    let aisdk_run = normalize(aisdk.into_runtime_run_request(agent_id));

    assert_eq!(agui_run.agent_id, aisdk_run.agent_id);
    assert_eq!(agui_run.thread_id, aisdk_run.thread_id);
    assert_eq!(agui_run.messages.len(), 1);
    assert_eq!(aisdk_run.messages.len(), 1);
    assert_eq!(agui_run.messages[0].role, aisdk_run.messages[0].role);
    assert_eq!(agui_run.messages[0].content, aisdk_run.messages[0].content);
}

#[tokio::test]
async fn agui_and_ai_sdk_have_equivalent_runtime_event_shape() {
    let os = make_os();

    let agui_req = RunAgentInput::new("thread_parity_stream", "run_parity_stream")
        .with_message(Message::user("hello parity"));
    let mut agui_resolved = os.resolve("test").unwrap();
    apply_agui_extensions(&mut agui_resolved, &agui_req).expect("apply AG-UI extensions");
    let agui_run_req = agui_req.into_runtime_run_request("test".to_string());
    let agui_prepared = os.prepare_run(agui_run_req, agui_resolved).await.unwrap();
    let agui_run = AgentOs::execute_prepared(agui_prepared).unwrap();
    let agui_events: Vec<AgentEvent> = agui_run.events.collect().await;

    let aisdk_run_req =
        AiSdkV6RunRequest::from_thread_input("thread_parity_stream", "hello parity")
            .into_runtime_run_request("test".to_string());
    let aisdk_run = os.run_stream(aisdk_run_req).await.unwrap();
    let aisdk_events: Vec<AgentEvent> = aisdk_run.events.collect().await;

    assert_eq!(
        collect_kinds(&agui_events),
        collect_kinds(&aisdk_events),
        "AG-UI and AI-SDK should produce equivalent runtime event shape for the same semantic input"
    );
}

// =========================================================================
// HTTP-level protocol contract tests
// =========================================================================

// ---- Decision-only forwarding -------------------------------------------

#[tokio::test]
async fn agui_decision_only_forwards_to_active_run() {
    let (app, _store, os) = make_http_app();

    let thread_id = "agui-decision-thread";
    let run_id = "agui-decision-run";
    let _active_run = start_active_run(&os, "test", thread_id, run_id).await;

    // AG-UI decision-only: tool message with no user message.
    let (status, body) = post_sse(
        app,
        "/v1/ag-ui/agents/test/runs",
        json!({
            "threadId": thread_id,
            "runId": run_id,
            "messages": [
                { "role": "tool", "content": "true", "toolCallId": "tool-1" }
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let payload: Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(payload["status"].as_str(), Some("decision_forwarded"));
    // AG-UI response includes runId.
    assert_eq!(payload["runId"].as_str(), Some(run_id));
    assert_eq!(payload["threadId"].as_str(), Some(thread_id));
}

#[tokio::test]
async fn ai_sdk_decision_only_forwards_when_run_id_present() {
    let (app, _store, os) = make_http_app();

    let thread_id = "aisdk-decision-thread";
    let run_id = "aisdk-decision-run";
    let _active_run = start_active_run(&os, "test", thread_id, run_id).await;

    // AI-SDK decision-only: assistant message with tool-approval-response, no user input.
    let (status, body) = post_sse(
        app,
        "/v1/ai-sdk/agents/test/runs",
        json!({
            "id": thread_id,
            "runId": run_id,
            "messages": [{
                "role": "assistant",
                "parts": [{
                    "type": "tool-approval-response",
                    "approvalId": "perm-1",
                    "approved": true
                }]
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let payload: Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(payload["status"].as_str(), Some("decision_forwarded"));
    assert_eq!(payload["threadId"].as_str(), Some(thread_id));
    // AI-SDK response does NOT include runId (diverges from AG-UI).
    assert!(
        payload.get("runId").is_none(),
        "AI-SDK decision_forwarded response should not include runId, got: {payload}"
    );
}

#[tokio::test]
async fn ai_sdk_decision_only_without_run_id_forwards_by_thread() {
    let (app, _store, os) = make_http_app();

    let thread_id = "aisdk-norunid-thread";
    let run_id = "aisdk-norunid-run";
    let _active_run = start_active_run(&os, "test", thread_id, run_id).await;

    let (status, body) = post_sse(
        app,
        "/v1/ai-sdk/agents/test/runs",
        json!({
            "id": thread_id,
            "messages": [{
                "role": "assistant",
                "parts": [{
                    "type": "tool-approval-response",
                    "approvalId": "perm-2",
                    "approved": true
                }]
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "unexpected response: {body}");
    let payload: Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(payload["status"].as_str(), Some("decision_forwarded"));
    assert_eq!(payload["threadId"].as_str(), Some(thread_id));
}

// ---- SSE response shape: headers, trailer -------------------------------

#[tokio::test]
async fn ai_sdk_sse_includes_custom_headers() {
    let (app, _store, _os) = make_http_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({
                        "id": "header-test-thread",
                        "messages": [{"role": "user", "content": "hello"}]
                    })
                    .to_string(),
                ))
                .expect("request build should succeed"),
        )
        .await
        .expect("app should handle request");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-vercel-ai-ui-message-stream")
            .and_then(|v| v.to_str().ok()),
        Some("v1"),
        "AI-SDK response must include x-vercel-ai-ui-message-stream: v1"
    );
    assert!(
        response.headers().get("x-tirea-ai-sdk-version").is_some(),
        "AI-SDK response must include x-tirea-ai-sdk-version header"
    );
}

#[tokio::test]
async fn ai_sdk_sse_ends_with_done_trailer() {
    let (app, _store, _os) = make_http_app();

    let (status, body) = post_sse(
        app,
        "/v1/ai-sdk/agents/test/runs",
        json!({
            "id": "done-trailer-thread",
            "messages": [{"role": "user", "content": "hello"}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let last_data_line = body
        .lines()
        .rev()
        .find(|line| line.starts_with("data: "))
        .expect("SSE body should contain at least one data line");
    assert_eq!(
        last_data_line.trim(),
        "data: [DONE]",
        "AI-SDK SSE stream must end with [DONE] trailer, got: {last_data_line}"
    );
}

#[tokio::test]
async fn agui_sse_has_no_done_trailer_or_custom_headers() {
    let (app, _store, _os) = make_http_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ag-ui/agents/test/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({
                        "threadId": "agui-no-trailer-thread",
                        "runId": "agui-no-trailer-run",
                        "messages": [{"role": "user", "content": "hello"}]
                    })
                    .to_string(),
                ))
                .expect("request build should succeed"),
        )
        .await
        .expect("app should handle request");

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("x-vercel-ai-ui-message-stream")
            .is_none(),
        "AG-UI response must NOT include x-vercel-ai-ui-message-stream header"
    );

    let body_bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body should be readable");
    let body = String::from_utf8(body_bytes.to_vec()).expect("body should be utf-8");
    assert!(
        !body.contains("data: [DONE]"),
        "AG-UI SSE stream must NOT contain [DONE] trailer"
    );
}

// ---- AI-SDK resume_stream -----------------------------------------------

#[tokio::test]
async fn ai_sdk_resume_stream_returns_no_content_when_idle() {
    let (app, _store, _os) = make_http_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/ai-sdk/agents/test/chats/nonexistent-chat/stream")
                .body(axum::body::Body::empty())
                .expect("request build should succeed"),
        )
        .await
        .expect("app should handle request");

    assert_eq!(
        response.status(),
        StatusCode::NO_CONTENT,
        "resume_stream with no active stream should return 204"
    );
}

// ---- AI-SDK RegenerateMessage -------------------------------------------

#[tokio::test]
async fn ai_sdk_regenerate_truncates_thread() {
    let (app, store, _os) = make_http_app();

    // Seed a thread with 3 messages.
    let thread_id = "regenerate-test-thread";
    let msg1 = CoreMessage::user("first").with_id("msg-1".to_string());
    let msg2 = CoreMessage::assistant("second").with_id("msg-2".to_string());
    let msg3 = CoreMessage::assistant("third").with_id("msg-3".to_string());
    let thread = tirea_contract::Thread::new(thread_id)
        .with_message(msg1)
        .with_message(msg2)
        .with_message(msg3);
    ThreadWriter::save(store.as_ref(), &thread)
        .await
        .expect("save seed thread");

    // Verify 3 messages before.
    let before = ThreadReader::load(store.as_ref(), thread_id)
        .await
        .expect("load should succeed")
        .expect("thread should exist");
    assert_eq!(before.thread.message_count(), 3);

    // POST with trigger=regenerate-message, messageId pointing to 2nd message.
    let (_status, _body) = post_sse(
        app,
        "/v1/ai-sdk/agents/test/runs",
        json!({
            "id": thread_id,
            "trigger": "regenerate-message",
            "messageId": "msg-2",
            "messages": []
        }),
    )
    .await;

    // Verify truncation: the 3rd message should be gone.
    let after = ThreadReader::load(store.as_ref(), thread_id)
        .await
        .expect("load should succeed")
        .expect("thread should exist");
    let ids: Vec<Option<&str>> = after
        .thread
        .messages
        .iter()
        .map(|m| m.id.as_deref())
        .collect();
    assert!(
        ids.contains(&Some("msg-1")),
        "msg-1 should survive truncation"
    );
    assert!(
        ids.contains(&Some("msg-2")),
        "msg-2 should survive truncation (inclusive)"
    );
    assert!(
        !ids.contains(&Some("msg-3")),
        "msg-3 should be removed by truncation, got ids: {ids:?}"
    );
}
