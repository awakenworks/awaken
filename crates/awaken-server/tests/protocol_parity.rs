//! Protocol parity tests — verifies AG-UI, AI-SDK, and ACP produce
//! equivalent event sequences for the same runtime input.
//!
//! Mirrors uncarve's tirea-agentos-server/tests/protocol_parity.rs,
//! adapted to awaken's encoder infrastructure.

use async_trait::async_trait;
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::transport::Transcoder;
use awaken_contract::registry_spec::AgentSpec;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::registry::traits::ModelEntry;
use awaken_server::app::{AppState, ServerConfig};
use awaken_server::protocols::{
    acp::encoder::AcpEncoder, ag_ui::encoder::AgUiEncoder, ai_sdk_v6::encoder::AiSdkEncoder,
};
use awaken_server::routes::build_router;
use awaken_stores::memory::InMemoryStore;
use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

// ── Mock executor ──

struct ImmediateExecutor;

#[async_trait]
impl awaken_contract::contract::executor::LlmExecutor for ImmediateExecutor {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        Ok(StreamResult {
            content: vec![],
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        "immediate"
    }
}

// ── Helpers ──

fn make_app() -> axum::Router {
    let runtime = {
        let builder = AgentRuntimeBuilder::new()
            .with_model(
                "test-model",
                ModelEntry {
                    provider: "mock".into(),
                    model_name: "mock-model".into(),
                },
            )
            .with_provider("mock", Arc::new(ImmediateExecutor))
            .with_agent_spec(AgentSpec {
                id: "test".into(),
                model: "test-model".into(),
                system_prompt: "test".into(),
                max_rounds: 0,
                ..Default::default()
            });
        Arc::new(builder.build().expect("build runtime"))
    };
    let store = Arc::new(InMemoryStore::new());
    let mailbox_store = std::sync::Arc::new(awaken_stores::InMemoryMailboxStore::new());
    let mailbox = std::sync::Arc::new(awaken_server::mailbox::Mailbox::new(
        runtime.clone(),
        mailbox_store,
        "test".to_string(),
        awaken_server::mailbox::MailboxConfig::default(),
    ));
    let state = AppState::new(
        runtime.clone(),
        mailbox,
        store.clone(),
        runtime.resolver_arc(),
        ServerConfig::default(),
    );
    build_router().with_state(state)
}

async fn post_sse(app: axum::Router, uri: &str, payload: Value) -> (StatusCode, String) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .expect("request build"),
        )
        .await
        .expect("app should handle request");
    let status = resp.status();
    let body = to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("body readable");
    let text = String::from_utf8(body.to_vec()).expect("utf-8");
    (status, text)
}

fn extract_sse_event_types(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .filter_map(|v| v.get("type").and_then(Value::as_str).map(String::from))
        .collect()
}

// ============================================================================
// Encoder-level parity: same runtime events produce output for all protocols
// ============================================================================

#[test]
fn text_delta_has_output_in_all_protocols() {
    let ev = AgentEvent::TextDelta {
        delta: "delta".into(),
    };
    assert!(!AcpEncoder::new().transcode(&ev).is_empty());
    assert!(!AgUiEncoder::new().transcode(&ev).is_empty());
    assert!(!AiSdkEncoder::new().transcode(&ev).is_empty());
}

#[test]
fn run_start_has_output_in_agui_and_aisdk() {
    let ev = AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    };
    let agui = AgUiEncoder::new().transcode(&ev);
    let aisdk = AiSdkEncoder::new().transcode(&ev);
    assert!(!agui.is_empty(), "AG-UI should emit run_start");
    assert!(!aisdk.is_empty(), "AI-SDK should emit run_start");
}

#[test]
fn run_finish_has_output_in_agui_and_aisdk() {
    use awaken_contract::contract::lifecycle::TerminationReason;

    let ev = AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    };
    let agui = AgUiEncoder::new().transcode(&ev);
    let aisdk = AiSdkEncoder::new().transcode(&ev);
    assert!(!agui.is_empty(), "AG-UI should emit run_finish");
    assert!(!aisdk.is_empty(), "AI-SDK should emit run_finish");
}

#[test]
fn tool_call_ready_produces_output_in_all_protocols() {
    // ToolCallReady is the fully-accumulated event — all protocols emit for it.
    // ToolCallStart/Delta are incremental and may be skipped by some protocols
    // (ACP defers until ToolCallReady; AG-UI and AI-SDK emit incrementally).
    let ready = AgentEvent::ToolCallReady {
        id: "c1".into(),
        name: "search".into(),
        arguments: json!({"q": "rust"}),
    };
    assert!(
        !AcpEncoder::new().transcode(&ready).is_empty(),
        "ACP should emit for ToolCallReady"
    );
    assert!(
        !AgUiEncoder::new().transcode(&ready).is_empty(),
        "AG-UI should emit for ToolCallReady"
    );
    assert!(
        !AiSdkEncoder::new().transcode(&ready).is_empty(),
        "AI-SDK should emit for ToolCallReady"
    );
}

#[test]
fn tool_call_incremental_events_in_agui_and_aisdk() {
    // AG-UI and AI-SDK emit for incremental tool call events; ACP does not.
    let start = AgentEvent::ToolCallStart {
        id: "c1".into(),
        name: "search".into(),
    };
    let delta = AgentEvent::ToolCallDelta {
        id: "c1".into(),
        args_delta: r#"{"q":"rust"}"#.into(),
    };

    for ev in [&start, &delta] {
        assert!(
            !AgUiEncoder::new().transcode(ev).is_empty(),
            "AG-UI should emit for {ev:?}"
        );
        assert!(
            !AiSdkEncoder::new().transcode(ev).is_empty(),
            "AI-SDK should emit for {ev:?}"
        );
    }

    // ACP intentionally skips incremental events
    assert!(
        AcpEncoder::new().transcode(&start).is_empty(),
        "ACP should skip ToolCallStart"
    );
    assert!(
        AcpEncoder::new().transcode(&delta).is_empty(),
        "ACP should skip ToolCallDelta"
    );
}

#[test]
fn error_event_produces_output_in_all_protocols() {
    let ev = AgentEvent::Error {
        message: "fatal".into(),
        code: Some("E001".into()),
    };
    assert!(!AcpEncoder::new().transcode(&ev).is_empty());
    assert!(!AgUiEncoder::new().transcode(&ev).is_empty());
    assert!(!AiSdkEncoder::new().transcode(&ev).is_empty());
}

#[test]
fn state_events_produce_output_in_all_protocols() {
    let snapshot = AgentEvent::StateSnapshot {
        snapshot: json!({"key": "value"}),
    };
    let delta = AgentEvent::StateDelta {
        delta: vec![json!({"op": "add", "path": "/x", "value": 1})],
    };

    for ev in [&snapshot, &delta] {
        assert!(
            !AgUiEncoder::new().transcode(ev).is_empty(),
            "AG-UI should emit for state event"
        );
        assert!(
            !AiSdkEncoder::new().transcode(ev).is_empty(),
            "AI-SDK should emit for state event"
        );
    }
}

// ============================================================================
// HTTP-level parity: both protocols accept equivalent inputs and stream SSE
// ============================================================================

#[tokio::test]
async fn agui_and_aisdk_both_return_sse_for_same_agent() {
    let app = make_app();

    let (agui_status, agui_body) = post_sse(
        app.clone(),
        "/v1/ag-ui/run",
        json!({
            "agentId": "test",
            "messages": [{"role": "user", "content": "hello parity"}]
        }),
    )
    .await;
    assert_eq!(agui_status, StatusCode::OK, "AG-UI failed: {agui_body}");

    let (aisdk_status, aisdk_body) = post_sse(
        app,
        "/v1/ai-sdk/chat",
        json!({
            "agentId": "test",
            "messages": [{"role": "user", "content": "hello parity"}]
        }),
    )
    .await;
    assert_eq!(aisdk_status, StatusCode::OK, "AI-SDK failed: {aisdk_body}");

    // Both should produce SSE data lines
    let agui_events = extract_sse_event_types(&agui_body);
    let aisdk_events = extract_sse_event_types(&aisdk_body);

    assert!(
        !agui_events.is_empty(),
        "AG-UI should produce SSE events: {agui_body}"
    );
    assert!(
        !aisdk_events.is_empty(),
        "AI-SDK should produce SSE events: {aisdk_body}"
    );
}

#[tokio::test]
async fn agui_and_aisdk_both_contain_run_lifecycle_markers() {
    let app = make_app();

    let (_, agui_body) = post_sse(
        app.clone(),
        "/v1/ag-ui/run",
        json!({
            "agentId": "test",
            "messages": [{"role": "user", "content": "lifecycle test"}]
        }),
    )
    .await;

    let (_, aisdk_body) = post_sse(
        app,
        "/v1/ai-sdk/chat",
        json!({
            "agentId": "test",
            "messages": [{"role": "user", "content": "lifecycle test"}]
        }),
    )
    .await;

    // AG-UI uses RUN_STARTED/RUN_FINISHED event types
    let agui_has_start = agui_body.contains("RUN_STARTED");
    let agui_has_finish = agui_body.contains("RUN_FINISHED");
    assert!(
        agui_has_start,
        "AG-UI should contain RUN_STARTED: {agui_body}"
    );
    assert!(
        agui_has_finish,
        "AG-UI should contain RUN_FINISHED: {agui_body}"
    );

    // AI-SDK uses start/finish event types
    let aisdk_has_start = aisdk_body.contains("\"type\":\"start\"");
    let aisdk_has_finish = aisdk_body.contains("\"type\":\"finish\"");
    assert!(
        aisdk_has_start,
        "AI-SDK should contain start event: {aisdk_body}"
    );
    assert!(
        aisdk_has_finish,
        "AI-SDK should contain finish event: {aisdk_body}"
    );
}

// ============================================================================
// Reasoning event parity
// ============================================================================

#[test]
fn reasoning_delta_produces_output_in_agui_and_aisdk() {
    let ev = AgentEvent::ReasoningDelta {
        delta: "thinking".into(),
    };
    let agui = AgUiEncoder::new().transcode(&ev);
    let aisdk = AiSdkEncoder::new().transcode(&ev);
    assert!(!agui.is_empty(), "AG-UI should emit for reasoning_delta");
    assert!(!aisdk.is_empty(), "AI-SDK should emit for reasoning_delta");
}

#[test]
fn reasoning_encrypted_value_produces_output_in_agui_and_aisdk() {
    let ev = AgentEvent::ReasoningEncryptedValue {
        encrypted_value: "opaque".into(),
    };
    // Need encoder with run context for AG-UI reasoning
    let mut agui = AgUiEncoder::new();
    agui.transcode(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    });
    let agui_out = agui.transcode(&ev);

    let mut aisdk = AiSdkEncoder::new();
    aisdk.transcode(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    });
    let aisdk_out = aisdk.transcode(&ev);

    assert!(
        !agui_out.is_empty(),
        "AG-UI should emit for reasoning_encrypted_value"
    );
    assert!(
        !aisdk_out.is_empty(),
        "AI-SDK should emit for reasoning_encrypted_value"
    );
}

// ============================================================================
// Activity event parity
// ============================================================================

#[test]
fn activity_events_produce_output_in_agui_and_aisdk() {
    let snapshot = AgentEvent::ActivitySnapshot {
        message_id: "m1".into(),
        activity_type: "thinking".into(),
        content: json!({"text": "processing"}),
        replace: Some(true),
    };
    let delta = AgentEvent::ActivityDelta {
        message_id: "m1".into(),
        activity_type: "progress".into(),
        patch: vec![json!({"op": "replace", "path": "/p", "value": 50})],
    };

    for ev in [&snapshot, &delta] {
        let agui = AgUiEncoder::new().transcode(ev);
        let aisdk = AiSdkEncoder::new().transcode(ev);
        assert!(!agui.is_empty(), "AG-UI should emit for activity event");
        assert!(!aisdk.is_empty(), "AI-SDK should emit for activity event");
    }
}

// ============================================================================
// Messages snapshot parity
// ============================================================================

#[test]
fn messages_snapshot_produces_output_in_agui_and_aisdk() {
    let ev = AgentEvent::MessagesSnapshot {
        messages: vec![json!({"role": "user", "content": "hi"})],
    };
    let agui = AgUiEncoder::new().transcode(&ev);
    let aisdk = AiSdkEncoder::new().transcode(&ev);
    assert!(!agui.is_empty(), "AG-UI should emit for messages_snapshot");
    assert!(
        !aisdk.is_empty(),
        "AI-SDK should emit for messages_snapshot"
    );
}
