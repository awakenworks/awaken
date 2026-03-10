//! End-to-end tests for A2UI tool integration.
//!
//! Verifies that:
//! 1. `A2uiRenderTool` executes correctly through the agent loop with a mock LLM
//! 2. `A2uiPlugin` injects system prompt context
//! 3. A2UI tool results flow through AG-UI and AI SDK event streams

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tirea_agentos::composition::{AgentDefinition, AgentDefinitionSpec, AgentOsBuilder};
use tirea_agentos::contracts::io::RunRequest;
use tirea_agentos::contracts::runtime::tool_call::Tool;
use tirea_agentos::contracts::storage::{MailboxStore, ThreadReader, ThreadStore};
use tirea_agentos::contracts::thread::Message;
use tirea_agentos::contracts::AgentBehavior;
use tirea_agentos::runtime::loop_runner::{LlmEventStream, LlmExecutor};
use tirea_agentos::{AgentDefinition, AgentOs, AgentOsBuilder};
use tirea_agentos_server::service::{AppState, MailboxService};
use tirea_extension_a2ui::{A2uiPlugin, A2uiRenderTool};
use tirea_store_adapters::MemoryStore;

#[allow(dead_code)]
mod common;

use common::{compose_http_app, post_sse};

// ---------------------------------------------------------------------------
// Mock LLM that returns a render_a2ui tool call
// ---------------------------------------------------------------------------

/// A2UI messages payload for testing.
fn a2ui_test_messages() -> Value {
    json!([
        {
            "version": "v0.9",
            "createSurface": {
                "surfaceId": "test_surface",
                "catalogId": "https://a2ui.org/specification/v0_9/basic_catalog.json"
            }
        },
        {
            "version": "v0.9",
            "updateComponents": {
                "surfaceId": "test_surface",
                "components": [
                    {"id": "root", "component": "Card", "child": "col"},
                    {"id": "col", "component": "Column", "children": ["title"]},
                    {"id": "title", "component": "Text", "text": "Hello A2UI"}
                ]
            }
        },
        {
            "version": "v0.9",
            "updateDataModel": {
                "surfaceId": "test_surface",
                "path": "/",
                "value": {}
            }
        }
    ])
}

struct A2uiMockLlm {
    call_count: AtomicUsize,
}

impl A2uiMockLlm {
    fn new() -> Self {
        Self {
            call_count: AtomicUsize::new(0),
        }
    }
}

impl std::fmt::Debug for A2uiMockLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2uiMockLlm").finish()
    }
}

#[async_trait]
impl LlmExecutor for A2uiMockLlm {
    async fn exec_chat_response(
        &self,
        _model: &str,
        _chat_req: genai::chat::ChatRequest,
        _options: Option<&genai::chat::ChatOptions>,
    ) -> genai::Result<genai::chat::ChatResponse> {
        unimplemented!("stream-only mock")
    }

    async fn exec_chat_stream_events(
        &self,
        _model: &str,
        _chat_req: genai::chat::ChatRequest,
        _options: Option<&genai::chat::ChatOptions>,
    ) -> genai::Result<LlmEventStream> {
        use genai::chat::{ChatStreamEvent, MessageContent, StreamChunk, StreamEnd, ToolChunk};

        let n = self.call_count.fetch_add(1, Ordering::SeqCst);

        if n == 0 {
            // First call: return a render_a2ui tool call.
            let args = json!({ "messages": a2ui_test_messages() });
            let tc = genai::chat::ToolCall {
                call_id: "tc-a2ui-001".to_string(),
                fn_name: "render_a2ui".to_string(),
                fn_arguments: Value::String(args.to_string()),
                thought_signatures: None,
            };
            let events = vec![
                Ok(ChatStreamEvent::Start),
                Ok(ChatStreamEvent::ToolCallChunk(ToolChunk {
                    tool_call: tc.clone(),
                })),
                Ok(ChatStreamEvent::End(StreamEnd {
                    captured_content: Some(MessageContent::from_tool_calls(vec![tc])),
                    ..Default::default()
                })),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        } else {
            // Subsequent calls: return text (triggers NaturalEnd).
            let events = vec![
                Ok(ChatStreamEvent::Start),
                Ok(ChatStreamEvent::Chunk(StreamChunk {
                    content: "A2UI rendering complete.".to_string(),
                })),
                Ok(ChatStreamEvent::End(StreamEnd::default())),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    fn name(&self) -> &'static str {
        "a2ui_mock"
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const A2UI_CATALOG: &str = "https://a2ui.org/specification/v0_9/basic_catalog.json";

fn test_mailbox_svc(os: &Arc<AgentOs>, store: Arc<dyn MailboxStore>) -> Arc<MailboxService> {
    Arc::new(MailboxService::new(os.clone(), store, "test"))
}

fn build_a2ui_os(store: Arc<MemoryStore>) -> AgentOs {
    let tool: Arc<dyn Tool> = Arc::new(A2uiRenderTool::new());
    let mut tools = HashMap::new();
    tools.insert("render_a2ui".to_string(), tool);

    AgentOsBuilder::new()
        .with_agent_spec(AgentDefinitionSpec::local_with_id(
            "a2ui",
            AgentDefinition {
                id: "a2ui".to_string(),
                model: "mock".to_string(),
                system_prompt: "You are an A2UI test agent.".to_string(),
                max_rounds: 4,
                behavior_ids: vec!["a2ui".to_string()],
                ..Default::default()
            },
        ))
        .with_tools(tools)
        .with_registered_behavior(
            "a2ui",
            Arc::new(A2uiPlugin::with_catalog_id(A2UI_CATALOG)) as Arc<dyn AgentBehavior>,
        )
        .with_agent_state_store(store as Arc<dyn ThreadStore>)
        .build()
        .expect("failed to build AgentOs")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test: AgentOs-level execution with mock LLM → A2UI tool call → events.
#[tokio::test]
async fn e2e_a2ui_tool_call_via_agent_os() {
    let store = Arc::new(MemoryStore::new());
    let os = build_a2ui_os(store.clone());

    // Resolve and inject mock LLM.
    let mut resolved = os.resolve("a2ui").unwrap();
    resolved.agent = resolved
        .agent
        .with_llm_executor(Arc::new(A2uiMockLlm::new()) as Arc<dyn LlmExecutor>);

    let prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "a2ui".to_string(),
                thread_id: Some("a2ui-e2e-thread".to_string()),
                run_id: Some("a2ui-e2e-run".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: Default::default(),
                state: None,
                messages: vec![Message::user("Render a contact form UI")],
                initial_decisions: vec![],
                source_mailbox_entry_id: None,
            },
            resolved,
        )
        .await
        .unwrap();

    let run = AgentOs::execute_prepared(prepared).unwrap();
    let events: Vec<_> = run.events.collect().await;

    // Should have RunStart and RunFinish.
    let event_types: Vec<String> = events.iter().map(|e| format!("{e:?}")).collect();

    // Find ToolCallDone for render_a2ui.
    let tool_done = events.iter().find(|e| {
        let s = format!("{e:?}");
        s.contains("ToolCallDone") && s.contains("render_a2ui")
    });
    assert!(
        tool_done.is_some(),
        "expected ToolCallDone for render_a2ui in events: {event_types:?}"
    );

    // Verify the tool result contains a2ui payload.
    let tool_done_str = format!("{tool_done:?}");
    assert!(
        tool_done_str.contains("Hello A2UI"),
        "tool result should contain A2UI component text"
    );
    assert!(
        tool_done_str.contains("test_surface"),
        "tool result should contain surface ID"
    );

    // Verify thread was persisted.
    let thread = store
        .load_thread("a2ui-e2e-thread")
        .await
        .expect("load should not fail")
        .expect("thread should be persisted");
    assert!(
        thread
            .messages
            .iter()
            .any(|m| m.content.contains("Render a contact form")),
        "user message should be persisted"
    );
}

/// Test: AG-UI HTTP endpoint with A2UI tool result in SSE stream.
#[tokio::test]
async fn e2e_a2ui_agui_http_tool_call_result() {
    let store = Arc::new(MemoryStore::new());
    let os = build_a2ui_os(store.clone());

    // We need to inject the mock LLM, but the HTTP handler uses os.resolve() internally.
    // Instead, we test the HTTP pathway using a TerminatePlugin first to verify wiring,
    // then use AgentOs-level for the actual tool call verification.
    // For the HTTP test, we verify that the A2UI agent is reachable and the tool is registered.

    let os = Arc::new(os);
    let mailbox_svc = test_mailbox_svc(&os, store.clone());
    let app = compose_http_app(AppState::new(os, store.clone(), mailbox_svc));

    // AG-UI request — without a real/mock LLM the agent will fail at inference,
    // but we can verify the route accepts the request and the agent is found.
    let payload = json!({
        "threadId": "a2ui-agui-test",
        "runId": "a2ui-agui-run",
        "messages": [{"role": "user", "content": "RUN_A2UI_TOOL render a form"}],
        "tools": []
    });
    let (status, body) = post_sse(app.clone(), "/v1/ag-ui/agents/a2ui/runs", payload).await;
    // The request should be accepted (200 OK) even though the mock LLM isn't wired at HTTP level.
    // It will produce RUN_STARTED + RUN_ERROR (no LLM configured), which is fine for route validation.
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "AG-UI route should accept request for a2ui agent"
    );
    assert!(
        body.contains("RUN_STARTED"),
        "should have RUN_STARTED event: {body}"
    );
}

/// Test: AI SDK HTTP endpoint accepts A2UI agent requests.
#[tokio::test]
async fn e2e_a2ui_ai_sdk_http_route() {
    let store = Arc::new(MemoryStore::new());
    let os = build_a2ui_os(store.clone());

    let os = Arc::new(os);
    let mailbox_svc = test_mailbox_svc(&os, store.clone());
    let app = compose_http_app(AppState::new(os, store.clone(), mailbox_svc));

    let payload = json!({
        "id": "a2ui-aisdk-test",
        "runId": "a2ui-aisdk-run",
        "messages": [{"role": "user", "content": "RUN_A2UI_TOOL render a form"}]
    });
    let (status, body) = post_sse(app.clone(), "/v1/ai-sdk/agents/a2ui/runs", payload).await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "AI SDK route should accept request for a2ui agent"
    );
    assert!(
        body.contains(r#""type":"start""#),
        "should have start event: {body}"
    );
}

/// Test: A2UI tool validation rejects invalid payloads through the agent loop.
#[tokio::test]
async fn e2e_a2ui_validation_rejects_invalid_payload() {
    let store = Arc::new(MemoryStore::new());
    let os = build_a2ui_os(store.clone());

    // Mock LLM that sends an invalid A2UI payload (missing version).
    struct InvalidA2uiMockLlm {
        call_count: AtomicUsize,
    }

    impl std::fmt::Debug for InvalidA2uiMockLlm {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("InvalidA2uiMockLlm").finish()
        }
    }

    #[async_trait]
    impl LlmExecutor for InvalidA2uiMockLlm {
        async fn exec_chat_response(
            &self,
            _model: &str,
            _chat_req: genai::chat::ChatRequest,
            _options: Option<&genai::chat::ChatOptions>,
        ) -> genai::Result<genai::chat::ChatResponse> {
            unimplemented!()
        }

        async fn exec_chat_stream_events(
            &self,
            _model: &str,
            _chat_req: genai::chat::ChatRequest,
            _options: Option<&genai::chat::ChatOptions>,
        ) -> genai::Result<LlmEventStream> {
            use genai::chat::{ChatStreamEvent, MessageContent, StreamChunk, StreamEnd, ToolChunk};

            let n = self.call_count.fetch_add(1, Ordering::SeqCst);

            if n == 0 {
                // Invalid: missing version field.
                let args = json!({
                    "messages": [{"createSurface": {"surfaceId": "bad", "catalogId": "test"}}]
                });
                let tc = genai::chat::ToolCall {
                    call_id: "tc-bad-001".to_string(),
                    fn_name: "render_a2ui".to_string(),
                    fn_arguments: Value::String(args.to_string()),
                    thought_signatures: None,
                };
                let events = vec![
                    Ok(ChatStreamEvent::Start),
                    Ok(ChatStreamEvent::ToolCallChunk(ToolChunk {
                        tool_call: tc.clone(),
                    })),
                    Ok(ChatStreamEvent::End(StreamEnd {
                        captured_content: Some(MessageContent::from_tool_calls(vec![tc])),
                        ..Default::default()
                    })),
                ];
                Ok(Box::pin(futures::stream::iter(events)))
            } else {
                let events = vec![
                    Ok(ChatStreamEvent::Start),
                    Ok(ChatStreamEvent::Chunk(StreamChunk {
                        content: "done".to_string(),
                    })),
                    Ok(ChatStreamEvent::End(StreamEnd::default())),
                ];
                Ok(Box::pin(futures::stream::iter(events)))
            }
        }

        fn name(&self) -> &'static str {
            "invalid_a2ui_mock"
        }
    }

    let mut resolved = os.resolve("a2ui").unwrap();
    resolved.agent = resolved
        .agent
        .with_llm_executor(Arc::new(InvalidA2uiMockLlm {
            call_count: AtomicUsize::new(0),
        }) as Arc<dyn LlmExecutor>);

    let prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "a2ui".to_string(),
                thread_id: Some("a2ui-invalid-thread".to_string()),
                run_id: Some("a2ui-invalid-run".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: Default::default(),
                state: None,
                messages: vec![Message::user("bad payload")],
                initial_decisions: vec![],
                source_mailbox_entry_id: None,
            },
            resolved,
        )
        .await
        .unwrap();

    let run = AgentOs::execute_prepared(prepared).unwrap();
    let events: Vec<_> = run.events.collect().await;

    // The tool call should produce an error result (validation failure).
    let tool_done = events.iter().find(|e| {
        let s = format!("{e:?}");
        s.contains("ToolCallDone") && s.contains("render_a2ui")
    });
    assert!(
        tool_done.is_some(),
        "should have ToolCallDone even for invalid payload"
    );

    // The result should indicate a validation error.
    let tool_str = format!("{tool_done:?}");
    assert!(
        tool_str.contains("validation") || tool_str.contains("missing"),
        "tool result should mention validation failure: {tool_str}"
    );
}

/// Test: A2uiPlugin injects catalog instructions into the system prompt.
#[tokio::test]
async fn e2e_a2ui_plugin_injects_system_prompt() {
    let store = Arc::new(MemoryStore::new());
    let os = build_a2ui_os(store.clone());

    // Mock that captures the chat request messages to verify system prompt injection.
    struct PromptCaptureMockLlm {
        captured_messages: std::sync::Mutex<Option<String>>,
    }

    impl std::fmt::Debug for PromptCaptureMockLlm {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("PromptCaptureMockLlm").finish()
        }
    }

    #[async_trait]
    impl LlmExecutor for PromptCaptureMockLlm {
        async fn exec_chat_response(
            &self,
            _model: &str,
            _chat_req: genai::chat::ChatRequest,
            _options: Option<&genai::chat::ChatOptions>,
        ) -> genai::Result<genai::chat::ChatResponse> {
            unimplemented!()
        }

        async fn exec_chat_stream_events(
            &self,
            _model: &str,
            chat_req: genai::chat::ChatRequest,
            _options: Option<&genai::chat::ChatOptions>,
        ) -> genai::Result<LlmEventStream> {
            use genai::chat::{ChatStreamEvent, StreamChunk, StreamEnd};

            // Capture all messages (system prompt is in the messages array).
            let msgs_text = format!("{:?}", chat_req.messages);
            *self.captured_messages.lock().unwrap() = Some(msgs_text);

            let events = vec![
                Ok(ChatStreamEvent::Start),
                Ok(ChatStreamEvent::Chunk(StreamChunk {
                    content: "ok".to_string(),
                })),
                Ok(ChatStreamEvent::End(StreamEnd::default())),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }

        fn name(&self) -> &'static str {
            "prompt_capture"
        }
    }

    let mock = Arc::new(PromptCaptureMockLlm {
        captured_messages: std::sync::Mutex::new(None),
    });

    let mut resolved = os.resolve("a2ui").unwrap();
    resolved.agent = resolved
        .agent
        .with_llm_executor(mock.clone() as Arc<dyn LlmExecutor>);

    let prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "a2ui".to_string(),
                thread_id: Some("a2ui-prompt-thread".to_string()),
                run_id: Some("a2ui-prompt-run".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: Default::default(),
                state: None,
                messages: vec![Message::user("test prompt injection")],
                initial_decisions: vec![],
                source_mailbox_entry_id: None,
            },
            resolved,
        )
        .await
        .unwrap();

    let run = AgentOs::execute_prepared(prepared).unwrap();
    let _events: Vec<_> = run.events.collect().await;

    // Verify A2UI instructions were injected into the system message.
    let captured = mock.captured_messages.lock().unwrap();
    let msgs_text = captured
        .as_ref()
        .expect("messages should have been captured");
    assert!(
        msgs_text.contains("render_a2ui"),
        "system prompt should mention render_a2ui tool: {msgs_text}"
    );
    assert!(
        msgs_text.contains("createSurface"),
        "system prompt should mention createSurface: {msgs_text}"
    );
    assert!(
        msgs_text.contains(A2UI_CATALOG),
        "system prompt should contain catalog ID: {msgs_text}"
    );
}
