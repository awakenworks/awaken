//! Regression test: multi-turn AI SDK v6 chat must not break after tool calls.
//!
//! AI SDK v6 sends the full message history on every turn. Completed tool calls
//! carry `providerExecuted: true`. Without filtering, `extract_tool_call_decisions()`
//! misidentifies these as new resume decisions, routing the request to the
//! "resume pending run" path which returns an empty stream.

use async_trait::async_trait;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::registry_spec::AgentSpec;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::registry::traits::ModelEntry;
use awaken_server::app::{AppState, ServerConfig};
use awaken_server::routes::build_router;
use awaken_stores::memory::InMemoryStore;
use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;

// ── Mock executor that always returns a text response ──

struct EchoExecutor;

#[async_trait]
impl awaken_contract::contract::executor::LlmExecutor for EchoExecutor {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        use awaken_contract::contract::content::ContentBlock;
        Ok(StreamResult {
            content: vec![ContentBlock::Text {
                text: "Here is the response.".into(),
            }],
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        "echo"
    }
}

fn make_app() -> axum::Router {
    let store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_model(
                "test-model",
                ModelEntry {
                    provider: "mock".into(),
                    model_name: "mock-model".into(),
                },
            )
            .with_provider("mock", Arc::new(EchoExecutor))
            .with_thread_run_store(store.clone())
            .with_agent_spec(AgentSpec {
                id: "default".into(),
                model: "test-model".into(),
                system_prompt: "You are a test assistant.".into(),
                max_rounds: 3,
                ..Default::default()
            })
            .build()
            .expect("build runtime"),
    );
    let mailbox_store = Arc::new(awaken_stores::InMemoryMailboxStore::new());
    let mailbox = Arc::new(awaken_server::mailbox::Mailbox::new(
        runtime.clone(),
        mailbox_store,
        "test".into(),
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

/// Turn 3 of a multi-turn conversation where turns 1-2 included tool calls.
///
/// The payload contains the full history with providerExecuted: true on
/// completed tool results — exactly what AI SDK v6 sends in practice.
///
/// Before the fix: the server would return an empty SSE stream (status 200,
/// zero data bytes) because it mistakenly entered the "resume pending run" path.
///
/// After the fix: the server processes the new user message normally.
#[tokio::test]
async fn turn3_with_provider_executed_history_returns_non_empty_response() {
    let app = make_app();

    // Simulate Turn 3: full history with completed tool calls from Turn 2.
    let payload = json!({
        "threadId": "thread-multi-turn-test",
        "messages": [
            // Turn 1: user
            {
                "id": "msg-1",
                "role": "user",
                "parts": [{"type": "text", "text": "Show me the fleet status"}]
            },
            // Turn 1: assistant with completed tool call
            {
                "id": "msg-2",
                "role": "assistant",
                "parts": [
                    {"type": "text", "text": "Let me check the fleet."},
                    {
                        "type": "tool-invocation",
                        "toolCallId": "call_fleet_1",
                        "toolName": "get_fleet_status",
                        "args": {},
                        "state": "output-available",
                        "output": [{"id": "ship-1", "status": "active"}],
                        "providerExecuted": true
                    }
                ]
            },
            // Turn 2: user
            {
                "id": "msg-3",
                "role": "user",
                "parts": [{"type": "text", "text": "Show me the anomalies"}]
            },
            // Turn 2: assistant with another completed tool call
            {
                "id": "msg-4",
                "role": "assistant",
                "parts": [
                    {"type": "text", "text": "Checking anomalies now."},
                    {
                        "type": "tool-invocation",
                        "toolCallId": "call_anomaly_1",
                        "toolName": "get_anomalies",
                        "args": {},
                        "state": "output-available",
                        "output": [{"id": "anomaly-1", "severity": "high"}],
                        "providerExecuted": true
                    }
                ]
            },
            // Turn 3: new user message (this is the one that should get a response)
            {
                "id": "msg-5",
                "role": "user",
                "parts": [{"type": "text", "text": "Generate a report"}]
            }
        ]
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/chat")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let body_str = String::from_utf8_lossy(&body);

    // The response must not be empty — it should contain SSE events.
    assert!(
        body.len() > 10,
        "Turn 3 response body must not be empty (got {} bytes: {:?}). \
         This indicates the server mistakenly treated providerExecuted \
         tool results as resume decisions.",
        body.len(),
        &body_str[..body_str.len().min(200)],
    );

    // Verify the response contains actual text content from the mock executor.
    assert!(
        body_str.contains("Here is the response"),
        "Response should contain executor output, got: {:?}",
        &body_str[..body_str.len().min(500)],
    );
}

/// Conversation with NO tool calls in history — plain text multi-turn.
/// Verifies that the basic multi-turn path works without any tool complexity.
#[tokio::test]
async fn plain_multi_turn_without_tools_returns_non_empty_response() {
    let app = make_app();

    let payload = json!({
        "threadId": "thread-plain-multi-turn",
        "messages": [
            {
                "id": "msg-1",
                "role": "user",
                "parts": [{"type": "text", "text": "Hello"}]
            },
            {
                "id": "msg-2",
                "role": "assistant",
                "parts": [{"type": "text", "text": "Hi there!"}]
            },
            {
                "id": "msg-3",
                "role": "user",
                "parts": [{"type": "text", "text": "How are you?"}]
            }
        ]
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/chat")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    assert!(
        body.len() > 10,
        "Plain multi-turn response should not be empty (got {} bytes)",
        body.len(),
    );
}

/// AI SDK v6 must receive canonical `UIMessage.parts`.
/// Legacy `content` payloads bypass the official transport normalization and
/// are rejected at the HTTP boundary.
#[tokio::test]
async fn legacy_content_messages_are_rejected() {
    let app = make_app();

    let payload = json!({
        "threadId": "thread-legacy-content",
        "agentId": "default",
        "messages": [
            {
                "id": "msg-system",
                "role": "system",
                "content": "You are concise."
            },
            {
                "id": "msg-user",
                "role": "user",
                "content": "Say hello"
            }
        ]
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/chat")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("no new messages or interaction responses"),
        "legacy content payload should be rejected, got: {:?}",
        &body_str[..body_str.len().min(500)],
    );
}
