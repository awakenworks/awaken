#![allow(missing_docs)]

use async_trait::async_trait;
use futures::StreamExt;
use genai::chat::{
    ChatRequest, ChatResponse, ChatStreamEvent, MessageContent, StreamChunk, StreamEnd, ToolChunk,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use tirea_agent_loop::runtime::loop_runner::{LlmEventStream, LlmExecutor};
use tirea_agentos::composition::AgentDefinition;
use tirea_agentos::runtime::AgentOs;
use tirea_contract::runtime::tool_call::{
    Tool, ToolCallProgressState, ToolCallProgressStatus, ToolCallProgressUpdate, ToolDescriptor,
    ToolError, ToolResult,
};
use tirea_contract::storage::{RunOrigin, RunReader, ThreadReader, ThreadStore};
use tirea_contract::{AgentEvent, Message, Role, RunRequest};
use tirea_store_adapters::MemoryStore;

const PARENT_AGENT_ID: &str = "parent-agent";
const CHILD_AGENT_ID: &str = "child-agent";
const GRANDCHILD_AGENT_ID: &str = "grandchild-agent";

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

impl std::fmt::Debug for ToolCallMockLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCallMockLlm")
            .field("tool_name", &self.tool_name)
            .finish()
    }
}

#[async_trait]
impl LlmExecutor for ToolCallMockLlm {
    async fn exec_chat_response(
        &self,
        _model: &str,
        _chat_req: ChatRequest,
        _options: Option<&genai::chat::ChatOptions>,
    ) -> genai::Result<ChatResponse> {
        unimplemented!("stream-only mock")
    }

    async fn exec_chat_stream_events(
        &self,
        _model: &str,
        _chat_req: ChatRequest,
        _options: Option<&genai::chat::ChatOptions>,
    ) -> genai::Result<LlmEventStream> {
        let call_index = self.call_count.fetch_add(1, Ordering::SeqCst);
        if call_index == 0 {
            let tool_call = genai::chat::ToolCall {
                call_id: "tc-001".to_string(),
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
                    content: format!("mock analysis complete #{call_index}"),
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

#[derive(Debug)]
struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("echo", "Echo", "Echoes the provided input").with_parameters(json!({
            "type": "object",
            "properties": {
                "input": {"type": "string"}
            },
            "required": ["input"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &tirea_contract::ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success("echo", json!({"echoed": args})))
    }
}

#[derive(Debug)]
struct ProgressEchoTool;

#[async_trait]
impl Tool for ProgressEchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "progress_echo",
            "Progress Echo",
            "Reports progress before succeeding",
        )
        .with_parameters(json!({"type": "object", "properties": {}}))
    }

    async fn execute(
        &self,
        _args: Value,
        ctx: &tirea_contract::ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        ctx.report_tool_call_progress(ToolCallProgressUpdate {
            status: ToolCallProgressStatus::Running,
            progress: Some(0.25),
            loaded: Some(1.0),
            total: Some(4.0),
            message: Some("warming up".to_string()),
        })
        .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
        ctx.report_tool_call_progress(ToolCallProgressUpdate {
            status: ToolCallProgressStatus::Done,
            progress: Some(1.0),
            loaded: Some(4.0),
            total: Some(4.0),
            message: Some("done".to_string()),
        })
        .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;

        Ok(ToolResult::success(
            "progress_echo",
            json!({"status": "ok"}),
        ))
    }
}

#[derive(Debug, Deserialize)]
struct PromptArgs {
    prompt: String,
}

struct SpawnGrandchildTool {
    os: Arc<OnceLock<AgentOs>>,
}

#[async_trait]
impl Tool for SpawnGrandchildTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "spawn_grandchild",
            "Spawn Grandchild",
            "Executes a grandchild subagent run",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string"}
            },
            "required": ["prompt"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &tirea_contract::ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let args: PromptArgs = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let os = configured_os(&self.os)?;
        let parent_thread_id = caller_thread_id(ctx)?;
        let parent_run_id = caller_run_id(ctx)?;
        let request = subagent_request(
            GRANDCHILD_AGENT_ID,
            nested_id("grandchild-thread", ctx),
            nested_id("grandchild-run", ctx),
            Some(parent_run_id),
            Some(parent_thread_id),
            vec![Message::user(args.prompt.clone())],
        );
        let executed = execute_mock_run(
            &os,
            request,
            Some(ctx.call_id().to_string()),
            Arc::new(ToolCallMockLlm::new("echo", json!({"input": args.prompt}))),
        )
        .await?;
        let echo_result = tool_result_data(&executed.events, "echo")?;

        Ok(ToolResult::success(
            "spawn_grandchild",
            json!({
                "grandchild_thread_id": executed.thread_id,
                "grandchild_run_id": executed.run_id,
                "echo": echo_result,
            }),
        ))
    }
}

struct SpawnChildChainTool {
    os: Arc<OnceLock<AgentOs>>,
}

#[async_trait]
impl Tool for SpawnChildChainTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "spawn_child_chain",
            "Spawn Child Chain",
            "Executes a child subagent that delegates to a grandchild",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string"}
            },
            "required": ["prompt"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &tirea_contract::ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let args: PromptArgs = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let os = configured_os(&self.os)?;
        let request = subagent_request(
            CHILD_AGENT_ID,
            nested_id("child-thread", ctx),
            nested_id("child-run", ctx),
            Some(caller_run_id(ctx)?),
            Some(caller_thread_id(ctx)?),
            vec![Message::user(args.prompt.clone())],
        );
        let executed = execute_mock_run(
            &os,
            request,
            Some(ctx.call_id().to_string()),
            Arc::new(ToolCallMockLlm::new(
                "spawn_grandchild",
                json!({"prompt": args.prompt}),
            )),
        )
        .await?;
        let grandchild = tool_result_data(&executed.events, "spawn_grandchild")?;

        Ok(ToolResult::success(
            "spawn_child_chain",
            json!({
                "child_thread_id": executed.thread_id,
                "child_run_id": executed.run_id,
                "grandchild": grandchild,
            }),
        ))
    }
}

struct SpawnChildWithProgressTool {
    os: Arc<OnceLock<AgentOs>>,
}

#[async_trait]
impl Tool for SpawnChildWithProgressTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "spawn_child_with_progress",
            "Spawn Child With Progress",
            "Executes a child subagent and forwards its tool progress",
        )
        .with_parameters(json!({"type": "object", "properties": {}}))
    }

    async fn execute(
        &self,
        _args: Value,
        ctx: &tirea_contract::ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let os = configured_os(&self.os)?;
        let request = subagent_request(
            CHILD_AGENT_ID,
            nested_id("progress-child-thread", ctx),
            nested_id("progress-child-run", ctx),
            Some(caller_run_id(ctx)?),
            Some(caller_thread_id(ctx)?),
            vec![Message::user("report progress")],
        );
        let forwarded = execute_mock_run_with_progress_forwarding(
            &os,
            request,
            Some(ctx.call_id().to_string()),
            Arc::new(ToolCallMockLlm::new("progress_echo", json!({}))),
            ctx,
        )
        .await?;

        Ok(ToolResult::success(
            "spawn_child_with_progress",
            json!({
                "child_thread_id": forwarded.executed.thread_id,
                "child_run_id": forwarded.executed.run_id,
                "forwarded_progress_updates": forwarded.forwarded_updates.len(),
            }),
        ))
    }
}

struct ExecutedRun {
    thread_id: String,
    run_id: String,
    events: Vec<AgentEvent>,
}

struct ForwardedRun {
    executed: ExecutedRun,
    forwarded_updates: Vec<ToolCallProgressUpdate>,
}

fn configured_os(slot: &Arc<OnceLock<AgentOs>>) -> Result<AgentOs, ToolError> {
    slot.get()
        .cloned()
        .ok_or_else(|| ToolError::Internal("AgentOs not initialized".to_string()))
}

fn caller_thread_id(ctx: &tirea_contract::ToolCallContext<'_>) -> Result<String, ToolError> {
    ctx.caller_context()
        .thread_id()
        .map(ToOwned::to_owned)
        .ok_or_else(|| ToolError::Internal("missing caller thread id".to_string()))
}

fn caller_run_id(ctx: &tirea_contract::ToolCallContext<'_>) -> Result<String, ToolError> {
    ctx.run_identity()
        .run_id_opt()
        .map(ToOwned::to_owned)
        .ok_or_else(|| ToolError::Internal("missing caller run id".to_string()))
}

fn nested_id(prefix: &str, ctx: &tirea_contract::ToolCallContext<'_>) -> String {
    let parent_run_id = ctx.run_identity().run_id_opt().unwrap_or("run");
    format!("{prefix}-{parent_run_id}-{}", ctx.call_id())
}

fn subagent_request(
    agent_id: &str,
    thread_id: String,
    run_id: String,
    parent_run_id: Option<String>,
    parent_thread_id: Option<String>,
    messages: Vec<Message>,
) -> RunRequest {
    RunRequest {
        agent_id: agent_id.to_string(),
        thread_id: Some(thread_id),
        run_id: Some(run_id),
        parent_run_id,
        parent_thread_id,
        resource_id: None,
        origin: RunOrigin::Subagent,
        state: None,
        messages,
        initial_decisions: vec![],
        source_mailbox_entry_id: None,
    }
}

async fn execute_mock_run(
    os: &AgentOs,
    request: RunRequest,
    parent_tool_call_id: Option<String>,
    llm_executor: Arc<dyn LlmExecutor>,
) -> Result<ExecutedRun, ToolError> {
    let mut resolved = os
        .resolve(&request.agent_id)
        .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
    resolved.parent_tool_call_id = parent_tool_call_id;
    resolved.agent = resolved.agent.with_llm_executor(llm_executor);

    let prepared = os
        .prepare_run(request, resolved)
        .await
        .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
    let thread_id = prepared.thread_id().to_string();
    let run_id = prepared.run_id().to_string();
    let run = AgentOs::execute_prepared(prepared)
        .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
    let events: Vec<_> = run.events.collect().await;

    if let Some(error) = first_error(&events) {
        return Err(ToolError::ExecutionFailed(error));
    }

    Ok(ExecutedRun {
        thread_id,
        run_id,
        events,
    })
}

async fn execute_mock_run_with_progress_forwarding(
    os: &AgentOs,
    request: RunRequest,
    parent_tool_call_id: Option<String>,
    llm_executor: Arc<dyn LlmExecutor>,
    parent_ctx: &tirea_contract::ToolCallContext<'_>,
) -> Result<ForwardedRun, ToolError> {
    let mut resolved = os
        .resolve(&request.agent_id)
        .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
    resolved.parent_tool_call_id = parent_tool_call_id;
    resolved.agent = resolved.agent.with_llm_executor(llm_executor);

    let prepared = os
        .prepare_run(request, resolved)
        .await
        .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
    let thread_id = prepared.thread_id().to_string();
    let run_id = prepared.run_id().to_string();
    let run = AgentOs::execute_prepared(prepared)
        .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;

    let mut child_tool_calls = HashSet::new();
    let mut forwarded_updates = Vec::new();
    let mut events = Vec::new();
    let mut stream = run.events;
    while let Some(event) = stream.next().await {
        match &event {
            AgentEvent::ToolCallStart { id, .. } => {
                child_tool_calls.insert(id.clone());
            }
            AgentEvent::ActivitySnapshot {
                activity_type,
                content,
                ..
            } if is_tool_call_progress_activity(activity_type) => {
                if let Some((child_call_id, update)) = decode_tool_call_progress_snapshot(content) {
                    if child_tool_calls.contains(&child_call_id) {
                        parent_ctx
                            .report_tool_call_progress(update.clone())
                            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
                        forwarded_updates.push(update);
                    }
                }
            }
            _ => {}
        }
        events.push(event);
    }

    if let Some(error) = first_error(&events) {
        return Err(ToolError::ExecutionFailed(error));
    }

    Ok(ForwardedRun {
        executed: ExecutedRun {
            thread_id,
            run_id,
            events,
        },
        forwarded_updates,
    })
}

fn first_error(events: &[AgentEvent]) -> Option<String> {
    events.iter().find_map(|event| match event {
        AgentEvent::Error { message, .. } => Some(message.clone()),
        _ => None,
    })
}

fn tool_result_data(events: &[AgentEvent], tool_name: &str) -> Result<Value, ToolError> {
    events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolCallDone { result, .. } if result.tool_name == tool_name => {
                Some(result.data.clone())
            }
            _ => None,
        })
        .ok_or_else(|| ToolError::ExecutionFailed(format!("missing tool result for {tool_name}")))
}

fn is_tool_call_progress_activity(activity_type: &str) -> bool {
    activity_type == tirea_contract::TOOL_CALL_PROGRESS_ACTIVITY_TYPE
        || activity_type == tirea_contract::TOOL_PROGRESS_ACTIVITY_TYPE
        || activity_type == tirea_contract::TOOL_PROGRESS_ACTIVITY_TYPE_LEGACY
}

fn decode_tool_call_progress_snapshot(content: &Value) -> Option<(String, ToolCallProgressUpdate)> {
    let payload = serde_json::from_value::<ToolCallProgressState>(content.clone()).ok()?;
    Some((
        payload.call_id,
        ToolCallProgressUpdate {
            status: payload.status,
            progress: payload.progress,
            loaded: payload.loaded,
            total: payload.total,
            message: payload.message,
        },
    ))
}

fn progress_snapshots(events: &[AgentEvent]) -> Vec<ToolCallProgressState> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ActivitySnapshot {
                activity_type,
                content,
                ..
            } if is_tool_call_progress_activity(activity_type) => {
                serde_json::from_value::<ToolCallProgressState>(content.clone()).ok()
            }
            _ => None,
        })
        .collect()
}

fn build_mock_subagent_os(storage: Arc<MemoryStore>, slot: Arc<OnceLock<AgentOs>>) -> AgentOs {
    let os = AgentOs::builder()
        .with_agent_state_store(storage as Arc<dyn ThreadStore>)
        .with_agent(PARENT_AGENT_ID, AgentDefinition::new("mock"))
        .with_agent(CHILD_AGENT_ID, AgentDefinition::new("mock"))
        .with_agent(GRANDCHILD_AGENT_ID, AgentDefinition::new("mock"))
        .with_tools(HashMap::from([
            ("echo".to_string(), Arc::new(EchoTool) as Arc<dyn Tool>),
            (
                "progress_echo".to_string(),
                Arc::new(ProgressEchoTool) as Arc<dyn Tool>,
            ),
            (
                "spawn_grandchild".to_string(),
                Arc::new(SpawnGrandchildTool { os: slot.clone() }) as Arc<dyn Tool>,
            ),
            (
                "spawn_child_chain".to_string(),
                Arc::new(SpawnChildChainTool { os: slot.clone() }) as Arc<dyn Tool>,
            ),
            (
                "spawn_child_with_progress".to_string(),
                Arc::new(SpawnChildWithProgressTool { os: slot.clone() }) as Arc<dyn Tool>,
            ),
        ]))
        .build()
        .expect("build mock subagent os");
    slot.set(os.clone()).expect("initialize shared AgentOs");
    os
}

#[tokio::test]
async fn mock_nested_subagent_chain_executes_grandchild_tool() {
    let storage = Arc::new(MemoryStore::new());
    let os_slot = Arc::new(OnceLock::new());
    let os = build_mock_subagent_os(storage.clone(), os_slot);

    let parent = execute_mock_run(
        &os,
        RunRequest {
            agent_id: PARENT_AGENT_ID.to_string(),
            thread_id: Some("mock-parent-thread".to_string()),
            run_id: Some("mock-parent-run".to_string()),
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            origin: RunOrigin::User,
            state: None,
            messages: vec![Message::user("run nested delegation")],
            initial_decisions: vec![],
            source_mailbox_entry_id: None,
        },
        None,
        Arc::new(ToolCallMockLlm::new(
            "spawn_child_chain",
            json!({"prompt": "nested hello"}),
        )),
    )
    .await
    .expect("parent run should succeed");

    let nested = tool_result_data(&parent.events, "spawn_child_chain")
        .expect("parent should surface child chain tool result");
    let child_thread_id = nested["child_thread_id"].as_str().expect("child thread id");
    let child_run_id = nested["child_run_id"].as_str().expect("child run id");
    let grandchild = &nested["grandchild"];
    let grandchild_thread_id = grandchild["grandchild_thread_id"]
        .as_str()
        .expect("grandchild thread id");
    let grandchild_run_id = grandchild["grandchild_run_id"]
        .as_str()
        .expect("grandchild run id");

    let child_head = storage
        .load(child_thread_id)
        .await
        .expect("load child thread")
        .expect("child thread exists");
    assert_eq!(
        child_head.thread.parent_thread_id.as_deref(),
        Some("mock-parent-thread")
    );

    let child_record = RunReader::load_run(storage.as_ref(), child_run_id)
        .await
        .expect("load child run")
        .expect("child run exists");
    assert_eq!(
        child_record.parent_run_id.as_deref(),
        Some("mock-parent-run")
    );
    assert_eq!(
        child_record.parent_thread_id.as_deref(),
        Some("mock-parent-thread")
    );
    assert_eq!(child_record.origin, RunOrigin::Subagent);

    let grandchild_head = storage
        .load(grandchild_thread_id)
        .await
        .expect("load grandchild thread")
        .expect("grandchild thread exists");
    assert_eq!(
        grandchild_head.thread.parent_thread_id.as_deref(),
        Some(child_thread_id)
    );

    let grandchild_record = RunReader::load_run(storage.as_ref(), grandchild_run_id)
        .await
        .expect("load grandchild run")
        .expect("grandchild run exists");
    assert_eq!(
        grandchild_record.parent_run_id.as_deref(),
        Some(child_run_id)
    );
    assert_eq!(
        grandchild_record.parent_thread_id.as_deref(),
        Some(child_thread_id)
    );
    assert_eq!(grandchild_record.origin, RunOrigin::Subagent);

    let assistant_with_tool = grandchild_head
        .thread
        .messages
        .iter()
        .find(|message| message.role == Role::Assistant && message.tool_calls.is_some())
        .expect("grandchild assistant tool call message");
    assert_eq!(
        assistant_with_tool.tool_calls.as_ref().unwrap()[0].name,
        "echo"
    );

    let tool_message = grandchild_head
        .thread
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("grandchild tool result message");
    assert_eq!(tool_message.tool_call_id.as_deref(), Some("tc-001"));
    assert_eq!(grandchild["echo"]["echoed"]["input"], json!("nested hello"));
}

#[tokio::test]
async fn mock_subagent_progress_is_forwarded_to_parent_activity() {
    let storage = Arc::new(MemoryStore::new());
    let os_slot = Arc::new(OnceLock::new());
    let os = build_mock_subagent_os(storage.clone(), os_slot);

    let parent = execute_mock_run(
        &os,
        RunRequest {
            agent_id: PARENT_AGENT_ID.to_string(),
            thread_id: Some("mock-progress-parent-thread".to_string()),
            run_id: Some("mock-progress-parent-run".to_string()),
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            origin: RunOrigin::User,
            state: None,
            messages: vec![Message::user("run child with progress")],
            initial_decisions: vec![],
            source_mailbox_entry_id: None,
        },
        None,
        Arc::new(ToolCallMockLlm::new("spawn_child_with_progress", json!({}))),
    )
    .await
    .expect("parent progress run should succeed");

    let progress_events = progress_snapshots(&parent.events);
    assert!(
        progress_events.iter().any(|payload| {
            payload.status == ToolCallProgressStatus::Running
                && payload.progress == Some(0.25)
                && payload.message.as_deref() == Some("warming up")
        }),
        "parent stream should include forwarded running progress: {progress_events:?}"
    );
    assert!(
        progress_events.iter().any(|payload| {
            payload.status == ToolCallProgressStatus::Done
                && payload.progress == Some(1.0)
                && payload.message.as_deref() == Some("done")
        }),
        "parent stream should include forwarded terminal progress: {progress_events:?}"
    );

    let forwarded = tool_result_data(&parent.events, "spawn_child_with_progress")
        .expect("parent should receive child progress tool result");
    assert!(
        forwarded["forwarded_progress_updates"]
            .as_u64()
            .unwrap_or_default()
            >= 2,
        "expected at least two forwarded progress snapshots: {forwarded}"
    );

    let child_thread_id = forwarded["child_thread_id"]
        .as_str()
        .expect("progress child thread id");
    let child_head = storage
        .load(child_thread_id)
        .await
        .expect("load progress child thread")
        .expect("progress child thread exists");
    assert_eq!(
        child_head.thread.parent_thread_id.as_deref(),
        Some("mock-progress-parent-thread")
    );
}
