mod common;

use async_trait::async_trait;
use common::{compose_http_app, SlowTerminatePlugin, TerminatePlugin};
use futures::StreamExt;
use genai::chat::{ChatStreamEvent, MessageContent, StreamChunk, StreamEnd, ToolChunk};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tirea_agentos::composition::{
    A2aAgentBinding, AgentDefinition, AgentDefinitionSpec, AgentOsBuilder,
};
use tirea_agentos::contracts::runtime::tool_call::ToolStatus;
use tirea_agentos::contracts::storage::{MailboxStore, ThreadReader};
use tirea_agentos::contracts::thread::{Message, Role};
use tirea_agentos::contracts::{AgentBehavior, AgentEvent, RunRequest};
use tirea_agentos::runtime::loop_runner::{LlmEventStream, LlmExecutor};
use tirea_agentos::runtime::AgentOs;
use tirea_agentos_server::service::{AppState, MailboxService};
use tirea_contract::storage::RunOrigin;
use tirea_store_adapters::MemoryStore;
use uuid::Uuid;

struct LiveServer {
    base_url: String,
    task: tokio::task::JoinHandle<()>,
}

impl LiveServer {
    async fn spawn(app: axum::Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind live test server");
        let addr = listener.local_addr().expect("read live test server addr");
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve live test server");
        });
        Self {
            base_url: format!("http://{addr}"),
            task,
        }
    }

    fn a2a_base_url(&self) -> String {
        format!("{}/v1/a2a", self.base_url)
    }
}

impl Drop for LiveServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug)]
struct ToolCallMockLlm {
    tool_name: String,
    tool_args: Value,
    call_count: AtomicUsize,
}

impl ToolCallMockLlm {
    fn new(tool_name: &str, tool_args: Value) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            tool_args,
            call_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl LlmExecutor for ToolCallMockLlm {
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
        let call_index = self.call_count.fetch_add(1, Ordering::SeqCst);

        if call_index == 0 {
            let tool_call = genai::chat::ToolCall {
                call_id: "tc-self-a2a".to_string(),
                fn_name: self.tool_name.clone(),
                fn_arguments: Value::String(self.tool_args.to_string()),
                thought_signatures: None,
            };
            let events = vec![
                Ok(ChatStreamEvent::Start),
                Ok(ChatStreamEvent::ToolCallChunk(ToolChunk {
                    tool_call: tool_call.clone(),
                })),
                Ok(ChatStreamEvent::End(StreamEnd {
                    captured_content: Some(MessageContent::from_tool_calls(vec![tool_call])),
                    ..Default::default()
                })),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        } else {
            let events = vec![
                Ok(ChatStreamEvent::Start),
                Ok(ChatStreamEvent::Chunk(StreamChunk {
                    content: format!("self a2a complete #{call_index}"),
                })),
                Ok(ChatStreamEvent::End(StreamEnd::default())),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    fn name(&self) -> &'static str {
        "tool_call_mock"
    }
}

async fn spawn_remote_service(
    behavior_id: String,
    behavior: Arc<dyn AgentBehavior>,
) -> (LiveServer, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    let read_store: Arc<dyn ThreadReader> = store.clone();
    let os = Arc::new(
        AgentOsBuilder::new()
            .with_registered_behavior(&behavior_id, behavior)
            .with_agent_spec(AgentDefinitionSpec::local_with_id(
                "remote-worker",
                AgentDefinition {
                    id: "remote-worker".to_string(),
                    behavior_ids: vec![behavior_id],
                    ..Default::default()
                },
            ))
            .with_agent_state_store(store.clone())
            .build()
            .expect("build remote AgentOs"),
    );
    let mailbox_svc = Arc::new(MailboxService::new(
        os.clone(),
        store.clone() as Arc<dyn MailboxStore>,
        "test",
    ));
    let app = compose_http_app(AppState::new(os, read_store, mailbox_svc));
    (LiveServer::spawn(app).await, store)
}

fn build_caller_os(remote_a2a_base_url: String, store: Arc<MemoryStore>) -> Arc<AgentOs> {
    Arc::new(
        AgentOsBuilder::new()
            .with_agent_spec(AgentDefinitionSpec::local_with_id(
                "caller",
                AgentDefinition::new("mock-model"),
            ))
            .with_agent_spec(AgentDefinitionSpec::a2a_with_id(
                "remote-worker",
                A2aAgentBinding::new(remote_a2a_base_url, "remote-worker").with_poll_interval_ms(5),
            ))
            .with_agent_state_store(store)
            .build()
            .expect("build caller AgentOs"),
    )
}

async fn execute_tool_run(
    os: &Arc<AgentOs>,
    store: &Arc<MemoryStore>,
    thread_id: &str,
    run_id: &str,
    prompt: &str,
    tool_name: &str,
    tool_args: Value,
) -> Value {
    let mut resolved = os.resolve("caller").expect("resolve caller");
    resolved.agent =
        resolved.agent.with_llm_executor(
            Arc::new(ToolCallMockLlm::new(tool_name, tool_args)) as Arc<dyn LlmExecutor>
        );

    let prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "caller".to_string(),
                thread_id: Some(thread_id.to_string()),
                run_id: Some(run_id.to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: RunOrigin::default(),
                state: None,
                messages: vec![Message::user(prompt)],
                initial_decisions: vec![],
                source_mailbox_entry_id: None,
            },
            resolved,
        )
        .await
        .expect("prepare caller run");
    let run = AgentOs::execute_prepared(prepared).expect("execute caller run");
    let events: Vec<_> = run.events.collect().await;
    let tool_result = events.iter().find_map(|event| match event {
        AgentEvent::ToolCallDone { result, .. } => Some(result),
        _ => None,
    });
    assert!(
        tool_result.is_some(),
        "expected delegated tool execution events: {events:?}"
    );
    assert_eq!(
        tool_result.expect("tool result should exist").status,
        ToolStatus::Success,
        "delegated tool call should succeed: {events:?}"
    );

    store
        .load(thread_id)
        .await
        .expect("load caller thread")
        .expect("caller thread should exist")
        .thread
        .rebuild_state()
        .expect("rebuild caller state")
}

#[tokio::test]
async fn live_agentos_server_a2a_foreground_delegation_persists_remote_locator() {
    let behavior_id = format!("terminate-{}", Uuid::new_v4().simple());
    let (remote, _remote_store) = spawn_remote_service(
        behavior_id.clone(),
        Arc::new(TerminatePlugin::new(behavior_id)),
    )
    .await;

    let caller_store = Arc::new(MemoryStore::new());
    let caller_os = build_caller_os(remote.a2a_base_url(), caller_store.clone());
    let thread_id = format!("caller-thread-{}", Uuid::new_v4().simple());
    let state = execute_tool_run(
        &caller_os,
        &caller_store,
        &thread_id,
        "caller-run-foreground",
        "delegate to live A2A service",
        "agent_run",
        json!({
            "agent_id": "remote-worker",
            "prompt": "hello live a2a",
            "background": false
        }),
    )
    .await;

    let runs = state["sub_agents"]["runs"]
        .as_object()
        .expect("sub-agent runs should exist");
    assert_eq!(runs.len(), 1);
    let (_sub_run_id, entry) = runs.iter().next().expect("one sub-agent run");
    assert_eq!(entry["protocol"], json!("a2a"));
    assert_eq!(entry["target_id"], json!("remote-worker"));
    assert_eq!(entry["status"], json!("completed"));
    assert!(entry["remote_run_id"].as_str().is_some());

    let mirror_thread_id = entry["mirror_thread_id"]
        .as_str()
        .expect("mirror thread id should be persisted");
    let mirrored = caller_store
        .load(mirror_thread_id)
        .await
        .expect("load mirrored thread")
        .expect("mirrored thread should exist");
    assert!(mirrored
        .thread
        .messages
        .iter()
        .any(|message| message.role == Role::User && message.content == "hello live a2a"));
    assert!(mirrored.thread.messages.iter().any(|message| {
        message.role == Role::System && message.content.contains("\"kind\":\"remote_a2a_task\"")
    }));
}

#[tokio::test]
async fn live_agentos_server_a2a_background_cancel_stops_remote_run() {
    let behavior_id = format!("slow-terminate-{}", Uuid::new_v4().simple());
    let (remote, _remote_store) = spawn_remote_service(
        behavior_id.clone(),
        Arc::new(SlowTerminatePlugin::new(
            behavior_id,
            Duration::from_secs(2),
        )),
    )
    .await;

    let caller_store = Arc::new(MemoryStore::new());
    let caller_os = build_caller_os(remote.a2a_base_url(), caller_store.clone());
    let thread_id = format!("caller-thread-{}", Uuid::new_v4().simple());

    let started_state = execute_tool_run(
        &caller_os,
        &caller_store,
        &thread_id,
        "caller-run-start",
        "start background remote run",
        "agent_run",
        json!({
            "agent_id": "remote-worker",
            "prompt": "background live a2a",
            "background": true
        }),
    )
    .await;
    let runs = started_state["sub_agents"]["runs"]
        .as_object()
        .expect("sub-agent runs should exist");
    assert_eq!(runs.len(), 1);
    let (sub_run_id, entry) = runs.iter().next().expect("one sub-agent run");
    assert_eq!(entry["status"], json!("running"));

    let stopped_state = execute_tool_run(
        &caller_os,
        &caller_store,
        &thread_id,
        "caller-run-stop",
        "stop background remote run",
        "agent_stop",
        json!({
            "run_id": sub_run_id
        }),
    )
    .await;
    assert_eq!(
        stopped_state["sub_agents"]["runs"][sub_run_id]["status"],
        json!("stopped")
    );
}
