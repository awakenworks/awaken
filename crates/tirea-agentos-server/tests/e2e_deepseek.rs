//! End-to-end tests for AI SDK and AG-UI SSE endpoints using DeepSeek.
//!
//! These tests require a valid `DEEPSEEK_API_KEY` environment variable.
//! Run with:
//! ```bash
//! DEEPSEEK_API_KEY=<key> cargo test --package tirea-agentos-server --test e2e_deepseek -- --ignored --nocapture
//! ```

use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tirea_agentos::composition::{AgentDefinition, AgentDefinitionSpec, AgentOsBuilder};
use tirea_agentos::contracts::runtime::tool_call::Tool;
use tirea_agentos::contracts::storage::{MailboxStore, ThreadReader, ThreadStore};
use tirea_agentos_server::service::{AppState, MailboxService};
use tirea_store_adapters::MemoryStore;
use tower::ServiceExt;

mod common;

use common::{
    ai_sdk_messages_payload, compose_http_app, extract_agui_text, extract_ai_sdk_text, post_sse,
    CalculatorTool,
};
use tirea_agentos::contracts::runtime::behavior::AgentBehavior;
use tirea_extension_a2ui::{A2uiPlugin, A2uiRenderTool};

fn has_deepseek_key() -> bool {
    std::env::var("DEEPSEEK_API_KEY").is_ok()
}

fn test_mailbox_svc(
    os: &Arc<tirea_agentos::runtime::AgentOs>,
    store: Arc<dyn MailboxStore>,
) -> Arc<MailboxService> {
    Arc::new(MailboxService::new(os.clone(), store, "test"))
}

fn is_transient_upstream_stream_error(status: StatusCode, body: &str) -> bool {
    if status != StatusCode::OK || !body.contains(r#""type":"error""#) {
        return false;
    }

    [
        "error sending request for url",
        "connection refused",
        "Connection reset",
        "timed out",
        "timeout",
        "503 Service Unavailable",
        "502 Bad Gateway",
    ]
    .iter()
    .any(|needle| body.contains(needle))
}

async fn post_sse_with_retry(app: axum::Router, uri: &str, payload: Value) -> (StatusCode, String) {
    let mut last: Option<(StatusCode, String)> = None;

    for attempt in 1..=3 {
        let (status, body) = post_sse(app.clone(), uri, payload.clone()).await;
        if !is_transient_upstream_stream_error(status, &body) {
            return (status, body);
        }

        eprintln!("transient upstream stream error on {uri} (attempt {attempt}/3), retrying");
        last = Some((status, body));
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }

    last.expect("retry loop must produce at least one response")
}

fn make_os(write_store: Arc<dyn ThreadStore>) -> tirea_agentos::runtime::AgentOs {
    let def = AgentDefinition {
        id: "deepseek".to_string(),
        model: "deepseek-chat".to_string(),
        system_prompt: "You are a helpful assistant. Keep answers very brief. \
If runtime context entries are provided, treat them as authoritative facts and answer directly from them."
            .to_string(),
        max_rounds: 1,
        ..Default::default()
    };

    AgentOsBuilder::new()
        .with_agent_spec(AgentDefinitionSpec::local_with_id("deepseek", def))
        .with_agent_state_store(write_store)
        .build()
        .expect("failed to build AgentOs")
}

/// Build AgentOs with a calculator tool and multi-round support.
fn make_tool_os(write_store: Arc<dyn ThreadStore>) -> tirea_agentos::runtime::AgentOs {
    let def = AgentDefinition {
        id: "calc".to_string(),
        model: "deepseek-chat".to_string(),
        system_prompt: "You are a calculator assistant.\n\
            Rules:\n\
            - You MUST use the `calculator` tool to perform arithmetic.\n\
            - After getting the tool result, reply with just the number.\n\
            - Never compute in your head — always call the tool."
            .to_string(),
        max_rounds: 3,
        ..Default::default()
    };

    let tools: HashMap<String, Arc<dyn Tool>> =
        HashMap::from([("calculator".to_string(), Arc::new(CalculatorTool) as _)]);

    AgentOsBuilder::new()
        .with_tools(tools)
        .with_agent_spec(AgentDefinitionSpec::local_with_id("calc", def))
        .with_agent_state_store(write_store)
        .build()
        .expect("failed to build AgentOs with calculator")
}

/// Build AgentOs for multi-turn conversation tests.
fn make_multiturn_os(write_store: Arc<dyn ThreadStore>) -> tirea_agentos::runtime::AgentOs {
    let def = AgentDefinition {
        id: "chat".to_string(),
        model: "deepseek-chat".to_string(),
        system_prompt: "You are a helpful assistant. Keep answers very brief.".to_string(),
        max_rounds: 1,
        ..Default::default()
    };

    AgentOsBuilder::new()
        .with_agent_spec(AgentDefinitionSpec::local_with_id("chat", def))
        .with_agent_state_store(write_store)
        .build()
        .expect("failed to build AgentOs for multi-turn")
}

// ============================================================================
// Basic chat (existing tests, preserved)
// ============================================================================

#[ignore]
#[tokio::test]
async fn e2e_ai_sdk_sse_with_deepseek() {
    if !has_deepseek_key() {
        eprintln!("DEEPSEEK_API_KEY not set, skipping");
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(storage.clone()));
    let svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), svc));

    let payload = ai_sdk_messages_payload(
        "e2e-sdk",
        "What is 2+2? Reply with just the number.",
        Some("r1"),
    );

    let (status, text) = post_sse_with_retry(app, "/v1/ai-sdk/agents/deepseek/runs", payload).await;

    println!("=== AI SDK SSE Response ===\n{text}");

    assert_eq!(status, StatusCode::OK);
    assert!(text.contains(r#""type":"start""#), "missing start event");
    assert!(
        text.contains(r#""type":"text-start""#),
        "missing text-start"
    );
    assert!(
        text.contains(r#""type":"text-delta""#),
        "missing text-delta — LLM produced no text?"
    );
    assert!(text.contains(r#""type":"text-end""#), "missing text-end");
    assert!(text.contains(r#""type":"finish""#), "missing finish");

    assert!(
        text.contains(r#""threadId":"e2e-sdk""#),
        "missing threadId in run-info"
    );
    assert!(
        !text.contains(r#""runId":"r1""#),
        "runInfo should not expose request runId for AI SDK"
    );

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let saved = storage.load_thread("e2e-sdk").await.unwrap();
    assert!(saved.is_some(), "thread not persisted");
    let saved = saved.unwrap();
    assert!(
        saved.messages.iter().any(|m| m.content.contains("2+2")),
        "user message not found in persisted thread"
    );
}

#[ignore]
#[tokio::test]
async fn e2e_ag_ui_sse_with_deepseek() {
    if !has_deepseek_key() {
        eprintln!("DEEPSEEK_API_KEY not set, skipping");
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(storage.clone()));
    let svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), svc));

    let payload = json!({
        "threadId": "e2e-agui",
        "runId": "r2",
        "messages": [
            {"role": "user", "content": "What is 3+3? Reply with just the number."}
        ],
        "tools": []
    });

    let (status, text) = post_sse_with_retry(app, "/v1/ag-ui/agents/deepseek/runs", payload).await;

    println!("=== AG-UI SSE Response ===\n{text}");

    assert_eq!(status, StatusCode::OK);
    assert!(
        text.contains(r#""type":"RUN_STARTED""#),
        "missing RUN_STARTED"
    );
    assert!(
        text.contains(r#""type":"RUN_FINISHED""#),
        "missing RUN_FINISHED"
    );
    assert!(
        text.contains(r#""type":"TEXT_MESSAGE_CONTENT""#),
        "missing TEXT_MESSAGE_CONTENT — LLM produced no text?"
    );

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let saved = storage.load_thread("e2e-agui").await.unwrap();
    assert!(saved.is_some(), "thread not persisted");
    let saved = saved.unwrap();
    assert!(
        saved
            .messages
            .iter()
            .any(|m| m.content.contains("3+3") || m.content.contains("3 + 3")),
        "user message not found in persisted thread"
    );
}

#[ignore]
#[tokio::test]
async fn e2e_ai_sdk_client_disconnect_cancels_inflight_stream() {
    if !has_deepseek_key() {
        eprintln!("DEEPSEEK_API_KEY not set, skipping");
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(storage.clone()));
    let svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), svc));

    let payload = ai_sdk_messages_payload(
        "e2e-sdk-cancel",
        "Write a very long response: output numbers from 1 to 500 with short commentary.",
        Some("r-cancel-1"),
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/agents/deepseek/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let mut body = resp.into_body();
    let mut first_data = None;
    for _ in 0..8usize {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(8), body.frame())
            .await
            .expect("timed out waiting for stream frame");
        let Some(frame) = frame else {
            break;
        };
        let frame = frame.expect("body frame should be readable");
        if let Ok(data) = frame.into_data() {
            first_data = Some(String::from_utf8_lossy(&data).to_string());
            break;
        }
    }
    let first_data = first_data.expect("expected at least one SSE data chunk before disconnect");
    assert!(
        first_data.contains("data:"),
        "first stream chunk should be SSE data: {first_data}"
    );

    // Simulate client disconnect by dropping the body stream mid-run.
    drop(body);

    tokio::time::sleep(std::time::Duration::from_millis(900)).await;

    let saved = storage
        .load_thread("e2e-sdk-cancel")
        .await
        .unwrap()
        .expect("thread should exist after disconnect");
    assert!(
        saved
            .messages
            .iter()
            .any(|m| m.role == tirea_agentos::contracts::thread::Role::User
                && m.content.contains("1 to 500")),
        "user ingress message should persist even when stream is cancelled"
    );
    let assistant_count = saved
        .messages
        .iter()
        .filter(|m| m.role == tirea_agentos::contracts::thread::Role::Assistant)
        .count();
    assert_eq!(
        assistant_count, 0,
        "disconnect should cooperatively cancel before assistant turn commit"
    );
}

// ============================================================================
// Tool-using agent: AI SDK v6 endpoint
// ============================================================================

#[ignore]
#[tokio::test]
async fn e2e_ai_sdk_tool_call_with_deepseek() {
    if !has_deepseek_key() {
        eprintln!("DEEPSEEK_API_KEY not set, skipping");
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_tool_os(storage.clone()));
    let svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), svc));

    let payload = ai_sdk_messages_payload(
        "e2e-sdk-tool",
        "Use the calculator tool to compute 17 * 3. Reply with just the number.",
        Some("r-tool-1"),
    );

    let (status, text) = post_sse_with_retry(app, "/v1/ai-sdk/agents/calc/runs", payload).await;

    println!("=== AI SDK Tool Call Response ===\n{text}");

    assert_eq!(status, StatusCode::OK);

    // Tool call events should be present in the stream (AI SDK v6 event names).
    assert!(
        text.contains(r#""type":"tool-input-start""#),
        "missing tool-input-start — LLM didn't invoke the calculator tool"
    );
    assert!(
        text.contains(r#""type":"tool-output-available""#),
        "missing tool-output-available — tool execution didn't complete"
    );

    // The final text should contain "51".
    let answer = extract_ai_sdk_text(&text);
    println!("Extracted text: {answer}");
    assert!(
        answer.contains("51"),
        "LLM did not answer '51' for 17*3. Got: {answer}"
    );

    // Thread should be persisted with tool call history.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let saved = storage.load_thread("e2e-sdk-tool").await.unwrap();
    assert!(saved.is_some(), "thread not persisted");
}

// ============================================================================
// Tool-using agent: AG-UI endpoint
// ============================================================================

#[ignore]
#[tokio::test]
async fn e2e_ag_ui_tool_call_with_deepseek() {
    if !has_deepseek_key() {
        eprintln!("DEEPSEEK_API_KEY not set, skipping");
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_tool_os(storage.clone()));
    let svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), svc));

    let payload = json!({
        "threadId": "e2e-agui-tool",
        "runId": "r-tool-2",
        "messages": [
            {"role": "user", "content": "Use the calculator tool to add 123 and 456. Reply with just the number."}
        ],
        "tools": []
    });

    let (status, text) = post_sse_with_retry(app, "/v1/ag-ui/agents/calc/runs", payload).await;

    println!("=== AG-UI Tool Call Response ===\n{text}");

    assert_eq!(status, StatusCode::OK);

    // AG-UI protocol: lifecycle + tool call events.
    assert!(
        text.contains(r#""type":"RUN_STARTED""#),
        "missing RUN_STARTED"
    );
    assert!(
        text.contains(r#""type":"RUN_FINISHED""#),
        "missing RUN_FINISHED"
    );
    assert!(
        text.contains(r#""type":"STEP_STARTED""#),
        "missing STEP_STARTED — agent loop should emit step boundaries"
    );
    assert!(
        text.contains(r#""type":"STEP_FINISHED""#),
        "missing STEP_FINISHED — agent loop should emit step boundaries"
    );
    assert!(
        text.contains(r#""type":"TOOL_CALL_START""#),
        "missing TOOL_CALL_START — LLM didn't invoke the calculator"
    );
    assert!(
        text.contains(r#""type":"TOOL_CALL_END""#),
        "missing TOOL_CALL_END — tool execution didn't complete"
    );

    // The response should contain "579".
    let answer = extract_agui_text(&text);
    println!("Extracted text: {answer}");
    assert!(
        answer.contains("579"),
        "LLM did not answer '579' for 123+456. Got: {answer}"
    );
}

// ============================================================================
// Multi-turn conversation via AI SDK endpoint
// ============================================================================

#[ignore]
#[tokio::test]
async fn e2e_ai_sdk_multiturn_with_deepseek() {
    if !has_deepseek_key() {
        eprintln!("DEEPSEEK_API_KEY not set, skipping");
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_multiturn_os(storage.clone()));

    // Turn 1: ask the agent to remember a number.
    let svc = test_mailbox_svc(&os, storage.clone());
    let app1 = compose_http_app(AppState::new(os.clone(), storage.clone(), svc));

    let (status, text1) = post_sse_with_retry(
        app1,
        "/v1/ai-sdk/agents/chat/runs",
        ai_sdk_messages_payload(
            "e2e-sdk-multi",
            "Remember the secret number 42. Just say OK.",
            Some("r-m1"),
        ),
    )
    .await;

    println!("=== Turn 1 ===\n{text1}");
    assert_eq!(status, StatusCode::OK);
    assert!(
        text1.contains(r#""type":"finish""#),
        "turn 1 did not finish"
    );

    // Wait for checkpoint.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Verify session has messages from turn 1.
    let saved = storage.load_thread("e2e-sdk-multi").await.unwrap().unwrap();
    assert!(
        saved.messages.len() >= 2,
        "turn 1 should persist at least user + assistant messages, got {}",
        saved.messages.len()
    );

    // Turn 2: AI SDK client sends full message history.
    let svc = test_mailbox_svc(&os, storage.clone());
    let app2 = compose_http_app(AppState::new(os.clone(), storage.clone(), svc));

    let (status, text2) = post_sse_with_retry(
        app2,
        "/v1/ai-sdk/agents/chat/runs",
        json!({
            "id": "e2e-sdk-multi",
            "runId": "r-m2",
            "messages": [
                {"role": "user", "content": "Remember the secret number 42. Just say OK."},
                {"role": "assistant", "content": "OK."},
                {"role": "user", "content": "What secret number did I tell you? Reply with just the number."}
            ]
        }),
    )
    .await;

    println!("=== Turn 2 ===\n{text2}");
    assert_eq!(status, StatusCode::OK);

    let answer = extract_ai_sdk_text(&text2);
    println!("Turn 2 extracted text: {answer}");
    assert!(
        answer.contains("42"),
        "LLM did not recall '42' from previous turn. Got: {answer}"
    );
}

// ============================================================================
// AI SDK: graceful finish via max rounds exceeded
// ============================================================================

#[ignore]
#[tokio::test]
async fn e2e_ai_sdk_finish_max_rounds_with_deepseek() {
    if !has_deepseek_key() {
        eprintln!("DEEPSEEK_API_KEY not set, skipping");
        return;
    }

    let def = AgentDefinition {
        id: "limited".to_string(),
        model: "deepseek-chat".to_string(),
        system_prompt:
            "You MUST use the calculator tool for every question. Never answer directly."
                .to_string(),
        max_rounds: 1,
        ..Default::default()
    };

    let tools: HashMap<String, Arc<dyn Tool>> =
        HashMap::from([("calculator".to_string(), Arc::new(CalculatorTool) as _)]);

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(
        AgentOsBuilder::new()
            .with_tools(tools)
            .with_agent_spec(AgentDefinitionSpec::local_with_id("limited", def))
            .with_agent_state_store(storage.clone())
            .build()
            .expect("failed to build limited AgentOs"),
    );

    let svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), svc));

    let (status, text) = post_sse_with_retry(
        app,
        "/v1/ai-sdk/agents/limited/runs",
        ai_sdk_messages_payload(
            "e2e-sdk-error",
            "Use the calculator to add 1 and 2.",
            Some("r-err-sdk"),
        ),
    )
    .await;

    println!("=== AI SDK Max-Rounds Response ===\n{text}");

    assert_eq!(status, StatusCode::OK);
    assert!(text.contains(r#""type":"start""#), "missing start event");

    // Max rounds should end the stream gracefully (finish), not as an error.
    assert!(
        text.contains(r#""type":"finish""#),
        "missing finish event — max rounds should end gracefully. Response:\n{text}"
    );
    assert!(
        text.contains(r#""finishReason":"length""#),
        "finish event should map max rounds to finishReason=length"
    );
    assert!(
        !text.contains(r#""type":"error""#),
        "max rounds should not emit error event. Response:\n{text}"
    );

    // Tool call events should still appear before finish.
    assert!(
        text.contains(r#""type":"tool-input-start""#),
        "missing tool-input-start — LLM should have called the tool before hitting max rounds"
    );
    assert!(
        text.contains(r#""type":"tool-output-available""#),
        "missing tool-output-available — tool should have executed before max rounds"
    );
}

// ============================================================================
// AI SDK: Multi-step tool call (verify step events and ordering)
// ============================================================================

#[ignore]
#[tokio::test]
async fn e2e_ai_sdk_multistep_tool_with_deepseek() {
    if !has_deepseek_key() {
        eprintln!("DEEPSEEK_API_KEY not set, skipping");
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_tool_os(storage.clone()));
    let svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), svc));

    let (status, text) = post_sse_with_retry(
        app,
        "/v1/ai-sdk/agents/calc/runs",
        ai_sdk_messages_payload(
            "e2e-sdk-multistep",
            "Use the calculator to multiply 11 by 9. Reply with just the number.",
            Some("r-ms-sdk"),
        ),
    )
    .await;

    println!("=== AI SDK Multi-Step Response ===\n{text}");

    assert_eq!(status, StatusCode::OK);
    assert!(text.contains(r#""type":"start""#), "missing start event");
    assert!(text.contains(r#""type":"finish""#), "missing finish event");

    // Should have >= 2 start-step / finish-step pairs (tool round + text round).
    let start_step_count = text.matches(r#""type":"start-step""#).count();
    let finish_step_count = text.matches(r#""type":"finish-step""#).count();

    println!("start-step count: {start_step_count}, finish-step count: {finish_step_count}");

    assert!(
        start_step_count >= 2,
        "expected >= 2 start-step events (tool call round + text round), got {start_step_count}"
    );
    assert!(
        finish_step_count >= 2,
        "expected >= 2 finish-step events, got {finish_step_count}"
    );

    // Tool call events.
    assert!(
        text.contains(r#""type":"tool-input-start""#),
        "missing tool-input-start"
    );
    assert!(
        text.contains(r#""type":"tool-output-available""#),
        "missing tool-output-available"
    );

    // Final answer should contain 99.
    let answer = extract_ai_sdk_text(&text);
    println!("Extracted text: {answer}");
    assert!(
        answer.contains("99"),
        "LLM did not answer '99' for 11*9. Got: {answer}"
    );

    // Verify event ordering: start → start-step → ... → finish-step → start-step → ... → finish-step → finish
    let events: Vec<String> = text
        .lines()
        .filter(|l| l.starts_with("data: "))
        .filter_map(|l| serde_json::from_str::<Value>(&l[6..]).ok())
        .filter_map(|v| {
            let t = v.get("type")?.as_str()?;
            match t {
                "start" | "finish" | "start-step" | "finish-step" => Some(t.to_string()),
                _ => None,
            }
        })
        .collect();

    println!("Lifecycle event sequence: {events:?}");
    assert_eq!(events.first().map(|s| s.as_str()), Some("start"));
    assert_eq!(events.last().map(|s| s.as_str()), Some("finish"));
}

// ============================================================================
// Multi-turn conversation via AG-UI endpoint
// ============================================================================

#[ignore]
#[tokio::test]
async fn e2e_ag_ui_multiturn_with_deepseek() {
    if !has_deepseek_key() {
        eprintln!("DEEPSEEK_API_KEY not set, skipping");
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_multiturn_os(storage.clone()));

    // Turn 1.
    let svc = test_mailbox_svc(&os, storage.clone());
    let app1 = compose_http_app(AppState::new(os.clone(), storage.clone(), svc));

    let (status, text1) = post_sse_with_retry(
        app1,
        "/v1/ag-ui/agents/chat/runs",
        json!({
            "threadId": "e2e-agui-multi",
            "runId": "r-am1",
            "messages": [
                {"role": "user", "content": "Remember the secret word: pineapple. Just say OK."}
            ],
            "tools": []
        }),
    )
    .await;

    println!("=== AG-UI Turn 1 ===\n{text1}");
    assert_eq!(status, StatusCode::OK);
    assert!(
        text1.contains(r#""type":"RUN_FINISHED""#),
        "turn 1 missing RUN_FINISHED"
    );

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Turn 2: AG-UI sends full message history from client.
    let svc = test_mailbox_svc(&os, storage.clone());
    let app2 = compose_http_app(AppState::new(os.clone(), storage.clone(), svc));

    let (status, text2) = post_sse_with_retry(
        app2,
        "/v1/ag-ui/agents/chat/runs",
        json!({
            "threadId": "e2e-agui-multi",
            "runId": "r-am2",
            "messages": [
                {"role": "user", "content": "The secret word is pineapple. Just acknowledge by saying OK."},
                {"role": "assistant", "content": "OK, I've noted the secret word."},
                {"role": "user", "content": "Now tell me: what is the secret word I told you earlier? Reply with ONLY the single word, nothing else."}
            ],
            "tools": []
        }),
    )
    .await;

    println!("=== AG-UI Turn 2 ===\n{text2}");
    assert_eq!(status, StatusCode::OK);

    let answer = extract_agui_text(&text2);
    println!("Turn 2 extracted text: {answer}");
    assert!(
        answer.to_lowercase().contains("pineapple"),
        "LLM did not recall 'pineapple'. Got: {answer}"
    );
}

// ============================================================================
// AG-UI with frontend tool definitions
// ============================================================================

#[ignore]
#[tokio::test]
async fn e2e_ag_ui_frontend_tools_with_deepseek() {
    if !has_deepseek_key() {
        eprintln!("DEEPSEEK_API_KEY not set, skipping");
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(storage.clone()));
    let svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), svc));

    // Provide a frontend tool definition — simulating CopilotKit's useCopilotAction.
    let payload = json!({
        "threadId": "e2e-agui-ft",
        "runId": "r-ft-1",
        "messages": [
            {"role": "user", "content": "Add a task called 'Deploy v2' to the task list using the addTask tool."}
        ],
        "tools": [
            {
                "name": "addTask",
                "description": "Add a new task to the task list",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string", "description": "Task title" }
                    },
                    "required": ["title"]
                },
                "execute": "frontend"
            }
        ]
    });

    let (status, text) = post_sse_with_retry(app, "/v1/ag-ui/agents/deepseek/runs", payload).await;

    println!("=== AG-UI Frontend Tools Response ===\n{text}");

    assert_eq!(status, StatusCode::OK);
    assert!(
        text.contains(r#""type":"RUN_STARTED""#),
        "missing RUN_STARTED"
    );

    // The LLM should attempt to call the frontend tool.
    // Check for TOOL_CALL_START with the addTask tool name.
    let has_tool_call = text.contains("addTask");
    println!("Contains addTask tool reference: {has_tool_call}");

    // The stream should eventually finish (either RUN_FINISHED or a suspended interaction).
    let has_run_finished = text.contains(r#""type":"RUN_FINISHED""#);
    let has_tool_call_start = text.contains(r#""type":"TOOL_CALL_START""#);

    assert!(
        has_run_finished || has_tool_call_start,
        "stream should contain RUN_FINISHED or TOOL_CALL_START, got neither"
    );

    // If the LLM made a tool call, verify it targeted addTask.
    if has_tool_call_start {
        assert!(
            text.contains(r#""toolCallName":"addTask""#),
            "tool call should target 'addTask'"
        );
        println!("LLM correctly invoked frontend tool 'addTask'");
    }
}

// ============================================================================
// AG-UI with context entries (useCopilotReadable)
// ============================================================================

#[ignore]
#[tokio::test]
async fn e2e_ag_ui_context_readable_with_deepseek() {
    if !has_deepseek_key() {
        eprintln!("DEEPSEEK_API_KEY not set, skipping");
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(storage.clone()));
    let svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), svc));

    // Provide context entries — simulating CopilotKit's useCopilotReadable.
    let payload = json!({
        "threadId": "e2e-agui-ctx",
        "runId": "r-ctx-1",
        "messages": [
            {"role": "user", "content": "Using ONLY the provided context, list shopping cart item names separated by commas."}
        ],
        "tools": [],
        "context": [
            {
                "name": "shoppingCart",
                "description": "Current items in the user's shopping cart",
                "value": "Laptop, Mouse, Keyboard"
            }
        ]
    });

    let (status, text) = post_sse_with_retry(app, "/v1/ag-ui/agents/deepseek/runs", payload).await;

    println!("=== AG-UI Context Response ===\n{text}");

    assert_eq!(status, StatusCode::OK);
    assert!(
        text.contains(r#""type":"RUN_FINISHED""#),
        "missing RUN_FINISHED"
    );

    let answer = extract_agui_text(&text);
    println!("Extracted text: {answer}");

    assert!(
        !answer.trim().is_empty(),
        "context flow should produce a non-empty assistant response"
    );
}

// ============================================================================
// AG-UI: RUN_FINISHED via max rounds exceeded
// ============================================================================

#[ignore]
#[tokio::test]
async fn e2e_ag_ui_run_finished_max_rounds_with_deepseek() {
    if !has_deepseek_key() {
        eprintln!("DEEPSEEK_API_KEY not set, skipping");
        return;
    }

    // Build agent with max_rounds=1 and a tool — the LLM will call the tool,
    // then the second round is needed to produce text, exceeding the limit.
    let def = AgentDefinition {
        id: "limited".to_string(),
        model: "deepseek-chat".to_string(),
        system_prompt:
            "You MUST use the calculator tool for every question. Never answer directly."
                .to_string(),
        max_rounds: 1,
        ..Default::default()
    };

    let tools: HashMap<String, Arc<dyn Tool>> =
        HashMap::from([("calculator".to_string(), Arc::new(CalculatorTool) as _)]);

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(
        AgentOsBuilder::new()
            .with_tools(tools)
            .with_agent_spec(AgentDefinitionSpec::local_with_id("limited", def))
            .with_agent_state_store(storage.clone())
            .build()
            .expect("failed to build limited AgentOs"),
    );

    let svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), svc));

    let payload = json!({
        "threadId": "e2e-agui-error",
        "runId": "r-err-1",
        "messages": [
            {"role": "user", "content": "Use the calculator to add 1 and 2."}
        ],
        "tools": []
    });

    let (status, text) = post_sse_with_retry(app, "/v1/ag-ui/agents/limited/runs", payload).await;

    println!("=== AG-UI Max-Rounds Response ===\n{text}");

    assert_eq!(status, StatusCode::OK);
    assert!(
        text.contains(r#""type":"RUN_STARTED""#),
        "missing RUN_STARTED"
    );

    // Max rounds should end with RUN_FINISHED (graceful stop), not RUN_ERROR.
    assert!(
        text.contains(r#""type":"RUN_FINISHED""#),
        "missing RUN_FINISHED — max rounds should end gracefully. Response:\n{text}"
    );
    assert!(
        !text.contains(r#""type":"RUN_ERROR""#),
        "max rounds should not emit RUN_ERROR. Response:\n{text}"
    );

    // Tool call should still happen before finishing.
    assert!(
        text.contains(r#""type":"TOOL_CALL_START""#),
        "missing TOOL_CALL_START — LLM should have called the tool before hitting max rounds"
    );
}

// ============================================================================
// AG-UI: Multi-step tool call (verify multiple STEP cycles)
// ============================================================================

#[ignore]
#[tokio::test]
async fn e2e_ag_ui_multistep_tool_with_deepseek() {
    if !has_deepseek_key() {
        eprintln!("DEEPSEEK_API_KEY not set, skipping");
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_tool_os(storage.clone()));
    let svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), svc));

    // Ask a question that requires a tool call, then a text response — two steps.
    let payload = json!({
        "threadId": "e2e-agui-multistep",
        "runId": "r-ms-1",
        "messages": [
            {"role": "user", "content": "Use the calculator to multiply 7 by 8. Reply with just the number."}
        ],
        "tools": []
    });

    let (status, text) = post_sse_with_retry(app, "/v1/ag-ui/agents/calc/runs", payload).await;

    println!("=== AG-UI Multi-Step Response ===\n{text}");

    assert_eq!(status, StatusCode::OK);
    assert!(
        text.contains(r#""type":"RUN_STARTED""#),
        "missing RUN_STARTED"
    );
    assert!(
        text.contains(r#""type":"RUN_FINISHED""#),
        "missing RUN_FINISHED"
    );

    // Count STEP_STARTED and STEP_FINISHED events — should be >= 2 of each
    // (step 1: LLM→tool call, step 2: LLM→text response).
    let step_started_count = text.matches(r#""type":"STEP_STARTED""#).count();
    let step_finished_count = text.matches(r#""type":"STEP_FINISHED""#).count();

    println!(
        "STEP_STARTED count: {step_started_count}, STEP_FINISHED count: {step_finished_count}"
    );

    assert!(
        step_started_count >= 2,
        "expected >= 2 STEP_STARTED events (tool call round + text round), got {step_started_count}"
    );
    assert!(
        step_finished_count >= 2,
        "expected >= 2 STEP_FINISHED events, got {step_finished_count}"
    );

    // Tool call events should be present.
    assert!(
        text.contains(r#""type":"TOOL_CALL_START""#),
        "missing TOOL_CALL_START"
    );
    assert!(
        text.contains(r#""type":"TOOL_CALL_RESULT""#),
        "missing TOOL_CALL_RESULT"
    );

    // Final answer should contain 56.
    let answer = extract_agui_text(&text);
    println!("Extracted text: {answer}");
    assert!(
        answer.contains("56"),
        "LLM did not answer '56' for 7*8. Got: {answer}"
    );

    // Verify event ordering: RUN_STARTED → STEP_STARTED → ... → STEP_FINISHED → STEP_STARTED → ... → STEP_FINISHED → RUN_FINISHED
    let events: Vec<String> = text
        .lines()
        .filter(|l| l.starts_with("data: "))
        .filter_map(|l| serde_json::from_str::<Value>(&l[6..]).ok())
        .filter_map(|v| {
            let t = v.get("type")?.as_str()?;
            match t {
                "RUN_STARTED" | "RUN_FINISHED" | "STEP_STARTED" | "STEP_FINISHED" => {
                    Some(t.to_string())
                }
                _ => None,
            }
        })
        .collect();

    println!("Lifecycle event sequence: {events:?}");

    assert_eq!(events.first().map(|s| s.as_str()), Some("RUN_STARTED"));
    assert_eq!(events.last().map(|s| s.as_str()), Some("RUN_FINISHED"));
}

// ============================================================================
// A2UI: Tool-based declarative UI rendering
// ============================================================================

const A2UI_CATALOG: &str = "https://a2ui.org/specification/v0_9/basic_catalog.json";

/// Build AgentOs with A2UI tool + plugin for real LLM testing.
fn make_a2ui_os(write_store: Arc<dyn ThreadStore>) -> tirea_agentos::AgentOs {
    let def = AgentDefinition {
        id: "a2ui".to_string(),
        model: "deepseek-chat".to_string(),
        system_prompt: format!(
            "You are an A2UI demo assistant that renders declarative UI.\n\
            You MUST use the render_a2ui tool to send A2UI messages.\n\
            Rules:\n\
            - Always call render_a2ui with an array of A2UI v0.9 messages.\n\
            - First send createSurface, then updateComponents, then updateDataModel.\n\
            - Use catalogId: \"{A2UI_CATALOG}\"\n\
            - After sending the UI, reply with a brief confirmation."
        ),
        max_rounds: 3,
        behavior_ids: vec!["a2ui".to_string()],
        ..Default::default()
    };

    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::from([(
        "render_a2ui".to_string(),
        Arc::new(A2uiRenderTool::new()) as _,
    )]);

    AgentOsBuilder::new()
        .with_tools(tools)
        .with_agent("a2ui", def)
        .with_registered_behavior(
            "a2ui",
            Arc::new(A2uiPlugin::with_catalog_id(A2UI_CATALOG)) as Arc<dyn AgentBehavior>,
        )
        .with_agent_state_store(write_store)
        .build()
        .expect("failed to build A2UI AgentOs")
}

#[ignore]
#[tokio::test]
async fn e2e_ag_ui_a2ui_tool_call_with_deepseek() {
    if !has_deepseek_key() {
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_a2ui_os(storage.clone()));
    let svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), svc));

    let payload = json!({
        "threadId": "e2e-agui-a2ui",
        "runId": "r-a2ui-1",
        "messages": [
            {"role": "user", "content": "Create a simple greeting card UI with a title saying 'Welcome' and a text field for name input."}
        ],
        "tools": []
    });

    let (status, text) = post_sse_with_retry(app, "/v1/ag-ui/agents/a2ui/runs", payload).await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        text.contains(r#""type":"RUN_STARTED""#),
        "missing RUN_STARTED"
    );
    assert!(
        text.contains(r#""type":"RUN_FINISHED""#),
        "missing RUN_FINISHED — A2UI run should complete. Response:\n{text}"
    );

    // The LLM should call render_a2ui.
    assert!(
        text.contains(r#""type":"TOOL_CALL_START""#),
        "missing TOOL_CALL_START — LLM didn't call render_a2ui. Response:\n{text}"
    );
    assert!(
        text.contains("render_a2ui"),
        "tool call should reference render_a2ui"
    );

    // Tool result should contain A2UI payload markers.
    assert!(
        text.contains(r#""type":"TOOL_CALL_RESULT""#),
        "missing TOOL_CALL_RESULT — render_a2ui should produce a result"
    );
    assert!(
        text.contains("createSurface") || text.contains("updateComponents"),
        "tool call args should contain A2UI message types"
    );
}

#[ignore]
#[tokio::test]
async fn e2e_ai_sdk_a2ui_tool_call_with_deepseek() {
    if !has_deepseek_key() {
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_a2ui_os(storage.clone()));
    let svc = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), svc));

    let payload = ai_sdk_messages_payload(
        "e2e-sdk-a2ui",
        "Build a task list UI with a text input and an add button using render_a2ui.",
        Some("r-a2ui-sdk-1"),
    );

    let (status, text) = post_sse_with_retry(app, "/v1/ai-sdk/agents/a2ui/runs", payload).await;

    assert_eq!(status, StatusCode::OK);
    assert!(text.contains(r#""type":"start""#), "missing start event");
    assert!(
        text.contains(r#""type":"finish""#),
        "missing finish event — A2UI run should complete. Response:\n{text}"
    );

    // The LLM should invoke render_a2ui tool.
    assert!(
        text.contains(r#""type":"tool-input-start""#),
        "missing tool-input-start — LLM didn't call render_a2ui"
    );
    assert!(
        text.contains("render_a2ui"),
        "tool events should reference render_a2ui"
    );
    assert!(
        text.contains(r#""type":"tool-output-available""#),
        "missing tool-output-available — render_a2ui should complete"
    );

    // Thread should be persisted.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let saved = storage.load_thread("e2e-sdk-a2ui").await.unwrap();
    assert!(saved.is_some(), "A2UI thread not persisted");
}
