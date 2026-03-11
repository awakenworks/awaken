//! End-to-end tests using TensorZero as LLM gateway and evaluation tool.
//!
//! These tests require:
//! 1. TensorZero + ClickHouse running via docker-compose
//! 2. `DEEPSEEK_API_KEY` set in the environment
//!
//! Run with:
//! ```bash
//! ./scripts/e2e-tensorzero.sh
//! ```
//!
//! Or manually:
//! ```bash
//! docker compose -f e2e/tensorzero/docker-compose.yml up -d --wait
//! DEEPSEEK_API_KEY=<key> cargo test --package tirea-agentos-server --test e2e_tensorzero -- --ignored --nocapture
//! docker compose -f e2e/tensorzero/docker-compose.yml down -v
//! ```

use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tirea_agentos::composition::AgentDefinition;
use tirea_agentos::composition::{AgentOsBuilder, ModelDefinition};
use tirea_agentos::contracts::runtime::tool_call::Tool;
use tirea_agentos::contracts::storage::{MailboxStore, ThreadReader, ThreadStore};
use tirea_agentos::runtime::AgentOs;
use tirea_agentos_server::service::{AppState, MailboxService};
use tirea_store_adapters::MemoryStore;
use tokio::sync::OnceCell;
use tower::ServiceExt;

mod common;

use common::{
    ai_sdk_messages_payload, compose_http_app, extract_agui_text, extract_ai_sdk_text,
    get_json_text as get_json, post_sse, CalculatorTool,
};

fn test_mailbox_svc(os: &Arc<AgentOs>, store: Arc<dyn MailboxStore>) -> Arc<MailboxService> {
    Arc::new(MailboxService::new(os.clone(), store, "test"))
}

/// Trailing slash is required: genai's OpenAI adapter uses Url::join("chat/completions"),
/// which needs a trailing slash to resolve correctly.
const TENSORZERO_ENDPOINT: &str = "http://localhost:3000/openai/v1/";
const TENSORZERO_CHAT_URL: &str = "http://localhost:3000/openai/v1/chat/completions";
const TENSORZERO_FEEDBACK_URL: &str = "http://localhost:3000/feedback";
static TENSORZERO_READINESS: OnceCell<Result<(), String>> = OnceCell::const_new();

fn has_deepseek_key() -> bool {
    std::env::var("DEEPSEEK_API_KEY").is_ok()
}

fn tensorzero_reachable() -> bool {
    std::net::TcpStream::connect("127.0.0.1:3000").is_ok()
}

async fn tensorzero_chat_endpoint_ready() -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let status = client
        .get(TENSORZERO_CHAT_URL)
        .send()
        .await
        .map_err(|e| format!("GET {TENSORZERO_CHAT_URL} failed: {e}"))?
        .status();

    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(format!(
            "{TENSORZERO_CHAT_URL} returned 404; OpenAI-compatible chat route is not available"
        ));
    }

    Ok(())
}

async fn tensorzero_chat_smoke_ready() -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let payload = json!({
        "model": "tensorzero::function_name::agent_chat",
        "messages": [{"role":"user","content":"Reply with OK"}],
        "max_tokens": 8,
        "temperature": 0
    });

    let resp = client
        .post(TENSORZERO_CHAT_URL)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("POST {TENSORZERO_CHAT_URL} failed: {e}"))?;
    let status = resp.status();

    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "{TENSORZERO_CHAT_URL} smoke request failed: status={status}, body={body}"
        ));
    }

    Ok(())
}

fn make_os(write_store: Arc<dyn ThreadStore>) -> tirea_agentos::runtime::AgentOs {
    // Model name: "openai::tensorzero::function_name::agent_chat"
    //   - genai sees "openai::" prefix → selects OpenAI adapter (→ /v1/chat/completions)
    //   - genai strips the "openai::" namespace → sends "tensorzero::function_name::agent_chat"
    //     as the model in the request body, which is what TensorZero expects.
    let def = AgentDefinition {
        id: "deepseek".to_string(),
        model: "deepseek".to_string(),
        system_prompt: "You are a helpful assistant. Keep answers very brief.".to_string(),
        max_rounds: 1,
        ..Default::default()
    };

    AgentOsBuilder::new()
        .with_provider("tz", make_tz_client())
        .with_model(
            "deepseek",
            ModelDefinition::new("tz", "openai::tensorzero::function_name::agent_chat"),
        )
        .with_agent("deepseek", def)
        .with_agent_state_store(write_store)
        .build()
        .expect("failed to build AgentOs with TensorZero")
}

async fn skip_unless_ready() -> bool {
    let readiness = TENSORZERO_READINESS
        .get_or_init(|| async {
            if !has_deepseek_key() {
                return Err("DEEPSEEK_API_KEY not set".to_string());
            }
            if !tensorzero_reachable() {
                return Err(
                    "TensorZero not reachable at :3000. Run: docker compose -f e2e/tensorzero/docker-compose.yml up -d --wait"
                        .to_string(),
                );
            }
            tensorzero_chat_endpoint_ready().await?;
            tensorzero_chat_smoke_ready().await?;
            Ok(())
        })
        .await;

    if let Err(err) = readiness {
        eprintln!("TensorZero not ready, skipping: {err}");
        return true;
    }

    false
}

// ============================================================================
// AI SDK SSE via TensorZero
// ============================================================================

#[tokio::test]
async fn e2e_tensorzero_ai_sdk_sse() {
    if skip_unless_ready().await {
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(storage.clone()));
    let mbox = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), mbox));

    let payload = ai_sdk_messages_payload(
        "tz-sdk-1",
        "What is 2+2? Reply with just the number.",
        Some("r1"),
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
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );

    let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();

    println!("=== AI SDK SSE via TensorZero ===");
    println!("{text}");

    // Protocol correctness.
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

    // Output quality: extract text-delta values and verify the answer.
    let deltas: String = text
        .lines()
        .filter(|l| l.starts_with("data: "))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(&l[6..]).ok())
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("text-delta"))
        .filter_map(|v| v.get("delta").and_then(|t| t.as_str()).map(String::from))
        .collect();
    assert!(
        deltas.contains('4'),
        "LLM did not answer '4' for 2+2. Got: {deltas}"
    );

    // Thread persistence.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let saved = storage.load_thread("tz-sdk-1").await.unwrap();
    assert!(saved.is_some(), "thread not persisted");
}

// ============================================================================
// AG-UI SSE via TensorZero
// ============================================================================

#[tokio::test]
async fn e2e_tensorzero_ag_ui_sse() {
    if skip_unless_ready().await {
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(storage.clone()));
    let mbox = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), mbox));

    let payload = json!({
        "threadId": "tz-agui-1",
        "runId": "r2",
        "messages": [
            {"role": "user", "content": "What is 3+3? Reply with just the number."}
        ],
        "tools": []
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ag-ui/agents/deepseek/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );

    let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();

    println!("=== AG-UI SSE via TensorZero ===");
    println!("{text}");

    // Protocol correctness.
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

    // Output quality: extract text content and verify the answer.
    let content: String = text
        .lines()
        .filter(|l| l.starts_with("data: "))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(&l[6..]).ok())
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("TEXT_MESSAGE_CONTENT"))
        .filter_map(|v| v.get("delta").and_then(|d| d.as_str()).map(String::from))
        .collect();
    assert!(
        content.contains('6'),
        "LLM did not answer '6' for 3+3. Got: {content}"
    );

    // Thread persistence.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let saved = storage.load_thread("tz-agui-1").await.unwrap();
    assert!(saved.is_some(), "thread not persisted");
}

// ============================================================================
// TensorZero feedback API integration
// ============================================================================

#[tokio::test]
async fn e2e_tensorzero_feedback() {
    if skip_unless_ready().await {
        return;
    }

    // First, make a direct inference call to TensorZero to get an inference_id.
    let http = reqwest::Client::new();
    let resp = http
        .post(TENSORZERO_CHAT_URL)
        .json(&json!({
            "model": "tensorzero::function_name::agent_chat",
            "messages": [
                {"role": "user", "content": "What is 5+5? Reply with just the number."}
            ]
        }))
        .send()
        .await
        .expect("TensorZero inference request failed");

    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "TensorZero returned non-200"
    );

    let body: serde_json::Value = resp.json().await.expect("invalid JSON response");
    println!("=== TensorZero Inference Response ===");
    println!("{}", serde_json::to_string_pretty(&body).unwrap());

    let inference_id = body["id"]
        .as_str()
        .expect("missing inference_id in TensorZero response");
    let content = body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");

    println!("Inference ID: {inference_id}");
    println!("Content: {content}");

    // Verify output quality.
    let answer_correct = content.contains("10");

    // Submit feedback to TensorZero.
    let feedback_resp = http
        .post(TENSORZERO_FEEDBACK_URL)
        .json(&json!({
            "inference_id": inference_id,
            "metric_name": "answer_correct",
            "value": answer_correct
        }))
        .send()
        .await
        .expect("TensorZero feedback request failed");

    println!(
        "Feedback response: {} {}",
        feedback_resp.status(),
        feedback_resp
            .text()
            .await
            .unwrap_or_else(|_| "<no body>".to_string())
    );

    assert!(
        answer_correct,
        "LLM did not answer '10' for 5+5. Got: {content}"
    );
}

// ============================================================================
// Helpers
// ============================================================================

fn make_tz_client() -> genai::Client {
    genai::Client::builder()
        .with_service_target_resolver_fn(|mut t: genai::ServiceTarget| {
            t.endpoint = genai::resolver::Endpoint::from_owned(TENSORZERO_ENDPOINT);
            t.auth = genai::resolver::AuthData::from_single("test");
            Ok(t)
        })
        .build()
}

fn make_tool_os(write_store: Arc<dyn ThreadStore>) -> tirea_agentos::runtime::AgentOs {
    let def = AgentDefinition {
        id: "calc".to_string(),
        model: "deepseek".to_string(),
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
        .with_provider("tz", make_tz_client())
        .with_model(
            "deepseek",
            ModelDefinition::new("tz", "openai::tensorzero::function_name::agent_chat"),
        )
        .with_tools(tools)
        .with_agent("calc", def)
        .with_agent_state_store(write_store)
        .build()
        .expect("failed to build AgentOs with TensorZero + calculator")
}

// ============================================================================
// Tool-using agent via TensorZero (AI SDK)
// ============================================================================

#[tokio::test]
async fn e2e_tensorzero_ai_sdk_tool_call() {
    if skip_unless_ready().await {
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_tool_os(storage.clone()));
    let mbox = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), mbox));

    let (status, text) = post_sse(
        app,
        "/v1/ai-sdk/agents/calc/runs",
        ai_sdk_messages_payload(
            "tz-sdk-tool",
            "Use the calculator tool to compute 25 * 4. Reply with just the number.",
            Some("r-tool-1"),
        ),
    )
    .await;

    println!("=== AI SDK Tool Call via TensorZero ===\n{text}");

    assert_eq!(status, StatusCode::OK);
    assert!(
        text.contains(r#""type":"tool-input-start""#),
        "missing tool-input-start — LLM didn't invoke calculator"
    );
    assert!(
        text.contains(r#""type":"tool-output-available""#),
        "missing tool-output-available"
    );

    let answer = extract_ai_sdk_text(&text);
    println!("Extracted text: {answer}");
    assert!(
        answer.contains("100"),
        "LLM did not answer '100' for 25*4. Got: {answer}"
    );
}

// ============================================================================
// Tool-using agent via TensorZero (AG-UI)
// ============================================================================

#[tokio::test]
async fn e2e_tensorzero_ag_ui_tool_call() {
    if skip_unless_ready().await {
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_tool_os(storage.clone()));
    let mbox = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), mbox));

    let (status, text) = post_sse(
        app,
        "/v1/ag-ui/agents/calc/runs",
        json!({
            "threadId": "tz-agui-tool",
            "runId": "r-tool-2",
            "messages": [
                {"role": "user", "content": "Use the calculator tool to add 200 and 300. Reply with just the number."}
            ],
            "tools": []
        }),
    )
    .await;

    println!("=== AG-UI Tool Call via TensorZero ===\n{text}");

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
        text.contains(r#""type":"STEP_STARTED""#),
        "missing STEP_STARTED — agent loop should emit step boundaries"
    );
    assert!(
        text.contains(r#""type":"STEP_FINISHED""#),
        "missing STEP_FINISHED — agent loop should emit step boundaries"
    );
    assert!(
        text.contains(r#""type":"TOOL_CALL_START""#),
        "missing TOOL_CALL_START"
    );
    assert!(
        text.contains(r#""type":"TOOL_CALL_END""#),
        "missing TOOL_CALL_END"
    );

    let answer = extract_agui_text(&text);
    println!("Extracted text: {answer}");
    assert!(
        answer.contains("500"),
        "LLM did not answer '500' for 200+300. Got: {answer}"
    );
}

// ============================================================================
// Multi-turn conversation via TensorZero (AI SDK)
// ============================================================================

#[tokio::test]
async fn e2e_tensorzero_ai_sdk_multiturn() {
    if skip_unless_ready().await {
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(storage.clone()));

    // Turn 1.
    let app1 = compose_http_app(AppState::new(os.clone(), storage.clone(), test_mailbox_svc(&os, storage.clone())));

    let (status, text1) = post_sse(
        app1,
        "/v1/ai-sdk/agents/deepseek/runs",
        ai_sdk_messages_payload(
            "tz-sdk-multi",
            "Remember the code word: banana. Just say OK.",
            Some("r-m1"),
        ),
    )
    .await;

    println!("=== TZ Turn 1 ===\n{text1}");
    assert_eq!(status, StatusCode::OK);
    assert!(
        text1.contains(r#""type":"finish""#),
        "turn 1 did not finish"
    );

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Turn 2.
    let app2 = compose_http_app(AppState::new(os.clone(), storage.clone(), test_mailbox_svc(&os, storage.clone())));

    let (status, text2) = post_sse(
        app2,
        "/v1/ai-sdk/agents/deepseek/runs",
        ai_sdk_messages_payload(
            "tz-sdk-multi",
            "What was the code word? Reply with just the word.",
            Some("r-m2"),
        ),
    )
    .await;

    println!("=== TZ Turn 2 ===\n{text2}");
    assert_eq!(status, StatusCode::OK);

    let answer = extract_ai_sdk_text(&text2);
    println!("Turn 2 text: {answer}");
    assert!(
        answer.to_lowercase().contains("banana"),
        "LLM did not recall 'banana'. Got: {answer}"
    );
}

// ============================================================================
// AI SDK: graceful finish via max rounds exceeded (TensorZero)
// ============================================================================

#[tokio::test]
async fn e2e_tensorzero_ai_sdk_finish_max_rounds() {
    if skip_unless_ready().await {
        return;
    }

    let def = AgentDefinition {
        id: "limited".to_string(),
        model: "deepseek".to_string(),
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
            .with_provider("tz", make_tz_client())
            .with_model(
                "deepseek",
                ModelDefinition::new("tz", "openai::tensorzero::function_name::agent_chat"),
            )
            .with_tools(tools)
            .with_agent("limited", def)
            .with_agent_state_store(storage.clone())
            .build()
            .expect("failed to build limited AgentOs"),
    );

    let mbox = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), mbox));

    let (status, text) = post_sse(
        app,
        "/v1/ai-sdk/agents/limited/runs",
        ai_sdk_messages_payload(
            "tz-sdk-error",
            "Use the calculator to add 1 and 2.",
            Some("r-err-sdk"),
        ),
    )
    .await;

    println!("=== TZ AI SDK Max-Rounds Response ===\n{text}");

    assert_eq!(status, StatusCode::OK);
    assert!(text.contains(r#""type":"start""#), "missing start event");

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

    assert!(
        text.contains(r#""type":"tool-input-start""#),
        "missing tool-input-start — LLM should have called the tool before hitting max rounds"
    );
}

// ============================================================================
// AI SDK: Multi-step tool call (TensorZero)
// ============================================================================

#[tokio::test]
async fn e2e_tensorzero_ai_sdk_multistep_tool() {
    if skip_unless_ready().await {
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_tool_os(storage.clone()));
    let mbox = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), mbox));

    let (status, text) = post_sse(
        app,
        "/v1/ai-sdk/agents/calc/runs",
        ai_sdk_messages_payload(
            "tz-sdk-multistep",
            "Use the calculator to multiply 12 by 5. Reply with just the number.",
            Some("r-ms-sdk"),
        ),
    )
    .await;

    println!("=== TZ AI SDK Multi-Step Response ===\n{text}");

    assert_eq!(status, StatusCode::OK);
    assert!(text.contains(r#""type":"start""#), "missing start event");
    assert!(text.contains(r#""type":"finish""#), "missing finish event");

    let start_step_count = text.matches(r#""type":"start-step""#).count();
    let finish_step_count = text.matches(r#""type":"finish-step""#).count();

    println!("start-step count: {start_step_count}, finish-step count: {finish_step_count}");

    assert!(
        start_step_count >= 2,
        "expected >= 2 start-step events, got {start_step_count}"
    );
    assert!(
        finish_step_count >= 2,
        "expected >= 2 finish-step events, got {finish_step_count}"
    );

    assert!(
        text.contains(r#""type":"tool-input-start""#),
        "missing tool-input-start"
    );
    assert!(
        text.contains(r#""type":"tool-output-available""#),
        "missing tool-output-available"
    );

    let answer = extract_ai_sdk_text(&text);
    println!("Extracted text: {answer}");
    assert!(
        answer.contains("60"),
        "LLM did not answer '60' for 12*5. Got: {answer}"
    );

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
// Multi-turn conversation via TensorZero (AG-UI)
// ============================================================================

#[tokio::test]
async fn e2e_tensorzero_ag_ui_multiturn() {
    if skip_unless_ready().await {
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(storage.clone()));

    // Turn 1.
    let app1 = compose_http_app(AppState::new(os.clone(), storage.clone(), test_mailbox_svc(&os, storage.clone())));

    let (status, _) = post_sse(
        app1,
        "/v1/ag-ui/agents/deepseek/runs",
        json!({
            "threadId": "tz-agui-multi",
            "runId": "r-am1",
            "messages": [
                {"role": "user", "content": "Remember the color: purple. Just say OK."}
            ],
            "tools": []
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Turn 2: AG-UI sends full history.
    let app2 = compose_http_app(AppState::new(os.clone(), storage.clone(), test_mailbox_svc(&os, storage.clone())));

    let (status, text2) = post_sse(
        app2,
        "/v1/ag-ui/agents/deepseek/runs",
        json!({
            "threadId": "tz-agui-multi",
            "runId": "r-am2",
            "messages": [
                {"role": "user", "content": "Remember the color: purple. Just say OK."},
                {"role": "assistant", "content": "OK"},
                {"role": "user", "content": "What color did I mention? Reply with just the color."}
            ],
            "tools": []
        }),
    )
    .await;

    println!("=== TZ AG-UI Turn 2 ===\n{text2}");
    assert_eq!(status, StatusCode::OK);

    let answer = extract_agui_text(&text2);
    println!("Turn 2 text: {answer}");
    assert!(
        answer.to_lowercase().contains("purple"),
        "LLM did not recall 'purple'. Got: {answer}"
    );
}

// ============================================================================
// AG-UI: RUN_FINISHED via max rounds exceeded (TensorZero)
// ============================================================================

#[tokio::test]
async fn e2e_tensorzero_ag_ui_run_finished_max_rounds() {
    if skip_unless_ready().await {
        return;
    }

    // max_rounds=1 + tool = the agent will call the tool, then need a second round → error.
    let def = AgentDefinition {
        id: "limited".to_string(),
        model: "deepseek".to_string(),
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
            .with_provider("tz", make_tz_client())
            .with_model(
                "deepseek",
                ModelDefinition::new("tz", "openai::tensorzero::function_name::agent_chat"),
            )
            .with_tools(tools)
            .with_agent("limited", def)
            .with_agent_state_store(storage.clone())
            .build()
            .expect("failed to build limited AgentOs"),
    );

    let mbox = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), mbox));

    let (status, text) = post_sse(
        app,
        "/v1/ag-ui/agents/limited/runs",
        json!({
            "threadId": "tz-agui-error",
            "runId": "r-err-1",
            "messages": [
                {"role": "user", "content": "Use the calculator to add 1 and 2."}
            ],
            "tools": []
        }),
    )
    .await;

    println!("=== TZ AG-UI Max-Rounds Response ===\n{text}");

    assert_eq!(status, StatusCode::OK);
    assert!(
        text.contains(r#""type":"RUN_STARTED""#),
        "missing RUN_STARTED"
    );

    assert!(
        text.contains(r#""type":"RUN_FINISHED""#),
        "missing RUN_FINISHED — max rounds should end gracefully. Response:\n{text}"
    );
    assert!(
        !text.contains(r#""type":"RUN_ERROR""#),
        "max rounds should not emit RUN_ERROR. Response:\n{text}"
    );
    assert!(
        text.contains(r#""type":"TOOL_CALL_START""#),
        "missing TOOL_CALL_START — LLM should have called the tool before hitting max rounds"
    );
}

// ============================================================================
// AG-UI: Multi-step tool call (verify multiple STEP cycles, TensorZero)
// ============================================================================

#[tokio::test]
async fn e2e_tensorzero_ag_ui_multistep_tool() {
    if skip_unless_ready().await {
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_tool_os(storage.clone()));
    let mbox = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage.clone(), mbox));

    let (status, text) = post_sse(
        app,
        "/v1/ag-ui/agents/calc/runs",
        json!({
            "threadId": "tz-agui-multistep",
            "runId": "r-ms-1",
            "messages": [
                {"role": "user", "content": "Use the calculator to multiply 9 by 6. Reply with just the number."}
            ],
            "tools": []
        }),
    )
    .await;

    println!("=== TZ AG-UI Multi-Step Response ===\n{text}");

    assert_eq!(status, StatusCode::OK);
    assert!(
        text.contains(r#""type":"RUN_STARTED""#),
        "missing RUN_STARTED"
    );
    assert!(
        text.contains(r#""type":"RUN_FINISHED""#),
        "missing RUN_FINISHED"
    );

    // Should have >= 2 STEP cycles (tool call round + text round).
    let step_started_count = text.matches(r#""type":"STEP_STARTED""#).count();
    let step_finished_count = text.matches(r#""type":"STEP_FINISHED""#).count();

    println!(
        "STEP_STARTED count: {step_started_count}, STEP_FINISHED count: {step_finished_count}"
    );

    assert!(
        step_started_count >= 2,
        "expected >= 2 STEP_STARTED events, got {step_started_count}"
    );
    assert!(
        step_finished_count >= 2,
        "expected >= 2 STEP_FINISHED events, got {step_finished_count}"
    );

    // Tool events.
    assert!(
        text.contains(r#""type":"TOOL_CALL_START""#),
        "missing TOOL_CALL_START"
    );
    assert!(
        text.contains(r#""type":"TOOL_CALL_RESULT""#),
        "missing TOOL_CALL_RESULT"
    );

    // Final answer.
    let answer = extract_agui_text(&text);
    println!("Extracted text: {answer}");
    assert!(
        answer.contains("54"),
        "LLM did not answer '54' for 9*6. Got: {answer}"
    );

    // Event ordering: RUN_STARTED first, RUN_FINISHED last.
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
// History message loading: AI SDK format
// ============================================================================

#[tokio::test]
async fn e2e_tensorzero_ai_sdk_load_history() {
    if skip_unless_ready().await {
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(storage.clone()));
    let thread_id = "tz-sdk-history";

    // Turn 1: agent run to populate the thread.
    let app1 = compose_http_app(AppState::new(os.clone(), storage.clone(), test_mailbox_svc(&os, storage.clone())));
    let (status, text1) = post_sse(
        app1,
        "/v1/ai-sdk/agents/deepseek/runs",
        ai_sdk_messages_payload(
            thread_id,
            "Remember the animal: elephant. Just say OK.",
            Some("r-hist-1"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        text1.contains(r#""type":"finish""#),
        "turn 1 did not finish"
    );

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Load history via AI SDK encoded endpoint.
    let app2 = compose_http_app(AppState::new(os.clone(), storage.clone(), test_mailbox_svc(&os, storage.clone())));
    let (status, history_text) =
        get_json(app2, &format!("/v1/ai-sdk/threads/{thread_id}/messages")).await;

    println!("=== AI SDK History ===\n{history_text}");
    assert_eq!(status, StatusCode::OK);

    let history: Value = serde_json::from_str(&history_text).expect("invalid JSON");

    // Must have messages array.
    let messages = history["messages"]
        .as_array()
        .expect("messages must be array");
    assert!(
        messages.len() >= 2,
        "expected at least 2 messages (user + assistant), got {}",
        messages.len()
    );

    // First message should be the user message.
    let user_msg = &messages[0];
    assert_eq!(user_msg["role"], "user", "first message should be user");
    let user_parts = user_msg["parts"].as_array().expect("parts must be array");
    let user_text: String = user_parts
        .iter()
        .filter(|p| p["type"] == "text")
        .filter_map(|p| p["text"].as_str())
        .collect();
    assert!(
        user_text.contains("elephant"),
        "user message should contain 'elephant'. Got: {user_text}"
    );

    // At least one assistant message should exist.
    let has_assistant = messages.iter().any(|m| m["role"] == "assistant");
    assert!(has_assistant, "history should include assistant message");

    // AI SDK format: parts should have state=done.
    let assistant_msg = messages.iter().find(|m| m["role"] == "assistant").unwrap();
    let assistant_parts = assistant_msg["parts"]
        .as_array()
        .expect("parts must be array");
    assert!(
        !assistant_parts.is_empty(),
        "assistant parts should not be empty"
    );

    // Pagination fields should be present.
    assert!(history.get("has_more").is_some(), "missing has_more field");
}

// ============================================================================
// History message loading: AG-UI format
// ============================================================================

#[tokio::test]
async fn e2e_tensorzero_ag_ui_load_history() {
    if skip_unless_ready().await {
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(storage.clone()));
    let thread_id = "tz-agui-history";

    // Turn 1: agent run to populate the thread.
    let app1 = compose_http_app(AppState::new(os.clone(), storage.clone(), test_mailbox_svc(&os, storage.clone())));
    let (status, _) = post_sse(
        app1,
        "/v1/ag-ui/agents/deepseek/runs",
        json!({
            "threadId": thread_id,
            "runId": "r-agui-hist-1",
            "messages": [
                {"role": "user", "content": "Remember the city: Tokyo. Just say OK."}
            ],
            "tools": []
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Load history via AG-UI encoded endpoint.
    let app2 = compose_http_app(AppState::new(os.clone(), storage.clone(), test_mailbox_svc(&os, storage.clone())));
    let (status, history_text) =
        get_json(app2, &format!("/v1/ag-ui/threads/{thread_id}/messages")).await;

    println!("=== AG-UI History ===\n{history_text}");
    assert_eq!(status, StatusCode::OK);

    let history: Value = serde_json::from_str(&history_text).expect("invalid JSON");

    // Must have messages array.
    let messages = history["messages"]
        .as_array()
        .expect("messages must be array");
    assert!(
        messages.len() >= 2,
        "expected at least 2 messages (user + assistant), got {}",
        messages.len()
    );

    // First message should be the user message.
    let user_msg = &messages[0];
    assert_eq!(user_msg["role"], "user", "first message should be user");
    assert!(
        user_msg["content"].as_str().unwrap_or("").contains("Tokyo"),
        "user message should contain 'Tokyo'"
    );

    // At least one assistant message should exist.
    let has_assistant = messages.iter().any(|m| m["role"] == "assistant");
    assert!(has_assistant, "history should include assistant message");

    // AG-UI format: should use camelCase (toolCallId, not tool_call_id).
    let json_str = serde_json::to_string(&messages).unwrap();
    assert!(
        !json_str.contains("tool_call_id"),
        "AG-UI history should use camelCase fields"
    );

    // Pagination fields should be present.
    assert!(history.get("has_more").is_some(), "missing has_more field");
}

// ============================================================================
// History loading after multi-turn: verify complete conversation persisted
// ============================================================================

#[tokio::test]
async fn e2e_tensorzero_ai_sdk_multiturn_history() {
    if skip_unless_ready().await {
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(storage.clone()));
    let thread_id = "tz-sdk-mt-history";

    // Turn 1.
    let app1 = compose_http_app(AppState::new(os.clone(), storage.clone(), test_mailbox_svc(&os, storage.clone())));
    let (status, _) = post_sse(
        app1,
        "/v1/ai-sdk/agents/deepseek/runs",
        ai_sdk_messages_payload(
            thread_id,
            "Remember: the secret is mango. Just say OK.",
            Some("r-mth-1"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Turn 2.
    let app2 = compose_http_app(AppState::new(os.clone(), storage.clone(), test_mailbox_svc(&os, storage.clone())));
    let (status, text2) = post_sse(
        app2,
        "/v1/ai-sdk/agents/deepseek/runs",
        ai_sdk_messages_payload(
            thread_id,
            "What was the secret? Reply with just the word.",
            Some("r-mth-2"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let answer = extract_ai_sdk_text(&text2);
    println!("Turn 2 answer: {answer}");
    assert!(
        answer.to_lowercase().contains("mango"),
        "LLM did not recall 'mango'. Got: {answer}"
    );

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Load full history.
    let app3 = compose_http_app(AppState::new(os.clone(), storage.clone(), test_mailbox_svc(&os, storage.clone())));
    let (status, history_text) = get_json(
        app3,
        &format!("/v1/ai-sdk/threads/{thread_id}/messages?limit=200"),
    )
    .await;

    println!("=== Multi-Turn History ===\n{history_text}");
    assert_eq!(status, StatusCode::OK);

    let history: Value = serde_json::from_str(&history_text).expect("invalid JSON");
    let messages = history["messages"]
        .as_array()
        .expect("messages must be array");

    // Should have at least 4 messages: user1, assistant1, user2, assistant2.
    assert!(
        messages.len() >= 4,
        "expected >= 4 messages for 2-turn conversation, got {}",
        messages.len()
    );

    // Verify user messages appear in order.
    let user_messages: Vec<&str> = messages
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| {
            m["parts"]
                .as_array()
                .and_then(|parts| parts.iter().find(|p| p["type"] == "text"))
                .and_then(|p| p["text"].as_str())
        })
        .collect();

    assert!(
        user_messages.len() >= 2,
        "expected >= 2 user messages, got {}",
        user_messages.len()
    );
    assert!(
        user_messages[0].contains("mango"),
        "first user message should mention 'mango'"
    );
    assert!(
        user_messages[1].contains("secret"),
        "second user message should ask about the secret"
    );
}

// ============================================================================
// History loading: raw format (no protocol encoding)
// ============================================================================

#[tokio::test]
async fn e2e_tensorzero_raw_message_history() {
    if skip_unless_ready().await {
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(storage.clone()));
    let thread_id = "tz-raw-history";

    // Run one turn to create the thread.
    let app1 = compose_http_app(AppState::new(os.clone(), storage.clone(), test_mailbox_svc(&os, storage.clone())));
    let (status, sse_text) = post_sse(
        app1,
        "/v1/ai-sdk/agents/deepseek/runs",
        ai_sdk_messages_payload(thread_id, "Say hello.", Some("r-raw-1")),
    )
    .await;
    println!("=== SSE Response (status={status}) ===\n{sse_text}");
    assert_eq!(status, StatusCode::OK, "SSE run failed: {sse_text}");

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Load raw messages.
    let app2 = compose_http_app(AppState::new(os.clone(), storage.clone(), test_mailbox_svc(&os, storage.clone())));
    let (status, raw_text) = get_json(app2, &format!("/v1/threads/{thread_id}/messages")).await;

    println!("=== Raw Message History (status={status}) ===\n{raw_text}");
    assert_eq!(status, StatusCode::OK, "raw messages failed: {raw_text}");

    let raw: Value = serde_json::from_str(&raw_text).expect("invalid JSON");
    let messages = raw["messages"].as_array().expect("messages must be array");
    assert!(
        messages.len() >= 2,
        "expected at least 2 raw messages, got {}",
        messages.len()
    );

    // Raw format has flat fields: role, content, id, cursor.
    let first = &messages[0];
    assert!(first["role"].is_string(), "raw message should have role");
    assert!(
        first["content"].is_string(),
        "raw message should have content"
    );

    // Load thread metadata.
    let app3 = compose_http_app(AppState::new(os.clone(), storage.clone(), test_mailbox_svc(&os, storage.clone())));
    let (status, thread_text) = get_json(app3, &format!("/v1/threads/{thread_id}")).await;

    println!("=== Thread Metadata ===\n{thread_text}");
    assert_eq!(status, StatusCode::OK);

    let thread: Value = serde_json::from_str(&thread_text).expect("invalid JSON");
    assert_eq!(thread["id"], thread_id, "thread ID should match");
}

// ============================================================================
// History loading: thread not found returns 404
// ============================================================================

#[tokio::test]
async fn e2e_tensorzero_history_not_found() {
    if skip_unless_ready().await {
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(storage.clone()));
    let mbox = test_mailbox_svc(&os, storage.clone());
    let app = compose_http_app(AppState::new(os, storage, mbox));

    let (status, _) = get_json(app, "/v1/ai-sdk/threads/nonexistent-thread/messages").await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "querying nonexistent thread should return 404"
    );
}

// ============================================================================
// History loading with tool calls: verify tool messages persisted
// ============================================================================

#[tokio::test]
async fn e2e_tensorzero_tool_call_history() {
    if skip_unless_ready().await {
        return;
    }

    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_tool_os(storage.clone()));
    let thread_id = "tz-tool-history";

    // Run a tool-using conversation.
    let app1 = compose_http_app(AppState::new(os.clone(), storage.clone(), test_mailbox_svc(&os, storage.clone())));
    let (status, text) = post_sse(
        app1,
        "/v1/ai-sdk/agents/calc/runs",
        ai_sdk_messages_payload(
            thread_id,
            "Use the calculator to add 7 and 8. Reply with just the number.",
            Some("r-tool-hist"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        text.contains(r#""type":"tool-input-start""#),
        "agent should have called calculator tool"
    );

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Load AI SDK history — should include tool parts.
    let app2 = compose_http_app(AppState::new(os.clone(), storage.clone(), test_mailbox_svc(&os, storage.clone())));
    let (status, history_text) = get_json(
        app2,
        &format!("/v1/ai-sdk/threads/{thread_id}/messages?limit=200&visibility=none"),
    )
    .await;

    println!("=== Tool Call History (AI SDK) ===\n{history_text}");
    assert_eq!(status, StatusCode::OK);

    let history: Value = serde_json::from_str(&history_text).expect("invalid JSON");
    let messages = history["messages"]
        .as_array()
        .expect("messages must be array");

    // Should have user message + assistant messages (including tool calls).
    assert!(
        messages.len() >= 2,
        "expected >= 2 messages, got {}",
        messages.len()
    );

    // Check that at least one assistant message has tool parts.
    let has_tool_part = messages.iter().any(|m| {
        m["parts"]
            .as_array()
            .map(|parts| parts.iter().any(|p| p["type"] == "tool-invocation"))
            .unwrap_or(false)
    });

    // Also check for text parts (the tool result or final answer).
    let has_text_part = messages.iter().any(|m| {
        m["role"] == "assistant"
            && m["parts"]
                .as_array()
                .map(|parts| parts.iter().any(|p| p["type"] == "text"))
                .unwrap_or(false)
    });

    // At minimum, assistant should have responded with text.
    assert!(
        has_text_part,
        "assistant history should include text response"
    );
    assert!(
        has_tool_part,
        "assistant history should include tool-invocation parts"
    );

    // Load AG-UI history for comparison.
    let app3 = compose_http_app(AppState::new(os.clone(), storage.clone(), test_mailbox_svc(&os, storage.clone())));
    let (status, agui_text) = get_json(
        app3,
        &format!("/v1/ag-ui/threads/{thread_id}/messages?limit=200&visibility=none"),
    )
    .await;

    println!("=== Tool Call History (AG-UI) ===\n{agui_text}");
    assert_eq!(status, StatusCode::OK);

    let agui_history: Value = serde_json::from_str(&agui_text).expect("invalid JSON");
    let agui_messages = agui_history["messages"]
        .as_array()
        .expect("messages must be array");

    // AG-UI should also have the conversation.
    assert!(
        agui_messages.len() >= 2,
        "AG-UI history should have >= 2 messages, got {}",
        agui_messages.len()
    );

    // Verify AG-UI format uses camelCase.
    assert!(
        !agui_text.contains("tool_call_id"),
        "AG-UI format should use camelCase (toolCallId)"
    );

    // Print summary for debugging.
    println!(
        "Tool history: AI SDK {} messages, AG-UI {} messages, has_tool_part={}",
        messages.len(),
        agui_messages.len(),
        has_tool_part,
    );
}
