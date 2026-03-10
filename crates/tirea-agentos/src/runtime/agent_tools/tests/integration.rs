use super::*;
use crate::runtime::background_tasks::TaskOutputTool;

// ══════════════════════════════════════════════════════════════════════════════
// Integration tests: full sub-agent lifecycle through ThreadStore
// ══════════════════════════════════════════════════════════════════════════════

/// Build an AgentOs with MemoryStore + SlowTerminatePlugin for integration tests.
/// Sub-agents using this terminate before inference (no LLM needed).
fn build_integration_os() -> (AgentOs, Arc<tirea_store_adapters::MemoryStore>) {
    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "caller",
            crate::runtime::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .with_agent(
            "reviewer",
            crate::runtime::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    (os, storage)
}

fn integration_caller_scope(
    state: serde_json::Value,
    run_id: &str,
    messages: Vec<crate::contracts::thread::Message>,
) -> tirea_contract::RunConfig {
    let mut rt = tirea_contract::RunConfig::new();
    rt.set(TOOL_SCOPE_CALLER_THREAD_ID_KEY, "parent-thread")
        .unwrap();
    rt.set(TOOL_SCOPE_CALLER_AGENT_ID_KEY, "caller").unwrap();
    rt.set(SCOPE_RUN_ID_KEY, run_id).unwrap();
    rt.set(TOOL_SCOPE_CALLER_STATE_KEY, state).unwrap();
    rt.set(TOOL_SCOPE_CALLER_MESSAGES_KEY, messages).unwrap();
    rt
}

#[tokio::test]
async fn integration_foreground_sub_agent_creates_thread_in_store() {
    use crate::contracts::storage::ThreadReader;

    let (os, storage) = build_integration_os();

    let run_tool = AgentRunTool::new(os, Arc::new(BackgroundTaskManager::new()));

    let mut fix = TestFixture::new();
    fix.run_config = integration_caller_scope(
        json!({}),
        "parent-run-1",
        vec![crate::contracts::thread::Message::user("hi")],
    );

    let result = run_tool
        .execute(
            json!({
                "agent_id": "worker",
                "prompt": "do something",
                "background": false
            }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();

    assert_eq!(result.status, ToolStatus::Success);
    let status = result.data["status"].as_str().unwrap();
    assert!(
        status == "completed" || status == "failed",
        "foreground run should reach terminal state, got: {status}"
    );
    let run_id = result.data["run_id"].as_str().unwrap();
    let child_thread_id = super::super::tools::sub_agent_thread_id(run_id);

    // Verify: child thread exists independently in ThreadStore.
    let child_head = storage
        .load(&child_thread_id)
        .await
        .expect("load should not error")
        .expect("child thread should exist in store");

    assert_eq!(child_head.thread.id, child_thread_id);
    assert!(
        !child_head.thread.messages.is_empty(),
        "child thread should have at least the user prompt message"
    );
    // The user message should be present.
    let has_user_msg = child_head
        .thread
        .messages
        .iter()
        .any(|m| m.role == Role::User);
    assert!(has_user_msg, "child thread should have a user message");

    // Verify: child thread has parent_thread_id set.
    assert_eq!(
        child_head.thread.parent_thread_id.as_deref(),
        Some("parent-thread"),
        "child thread should have parent_thread_id set"
    );
}

#[tokio::test]
async fn integration_task_output_reads_from_thread_store() {
    use crate::contracts::storage::ThreadReader;
    use crate::runtime::background_tasks::TaskStore;

    let (os, storage) = build_integration_os();
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let run_tool = AgentRunTool::new(os, bg_mgr.clone());
    let output_tool = TaskOutputTool::new(
        bg_mgr,
        Some(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>),
    );

    let mut fix = TestFixture::new();
    fix.run_config = integration_caller_scope(
        json!({}),
        "parent-run-1",
        vec![crate::contracts::thread::Message::user("hi")],
    );

    // Launch foreground sub-agent.
    let result = run_tool
        .execute(
            json!({
                "agent_id": "worker",
                "prompt": "analyze data",
                "background": false
            }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    let run_id = result.data["run_id"].as_str().unwrap().to_string();
    let child_thread_id = super::super::tools::sub_agent_thread_id(&run_id);
    let task_store =
        TaskStore::new(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>);
    let task = task_store
        .load_task(&run_id)
        .await
        .unwrap()
        .expect("task should be persisted");
    assert_eq!(task.metadata["thread_id"], json!(child_thread_id));
    assert_eq!(task.metadata["agent_id"], json!("worker"));

    let mut output_fix = TestFixture::new();
    output_fix.run_config = integration_caller_scope(json!({}), "parent-run-1", vec![]);
    let output_result = output_tool
        .execute(
            json!({ "task_id": run_id }),
            &output_fix.ctx_with("call-output", "tool:task_output"),
        )
        .await
        .unwrap();

    // task_output should return success with sub-agent info.
    assert_eq!(output_result.status, ToolStatus::Success);
    assert_eq!(output_result.data["agent_id"], json!("worker"));
    assert_eq!(output_result.data["task_id"], json!(run_id));

    // The child thread should exist in store (verified by task_output reading it).
    let child_head = storage.load(&child_thread_id).await.unwrap();
    assert!(
        child_head.is_some(),
        "child thread should exist in store for task_output to read"
    );
}

#[tokio::test]
async fn integration_persisted_task_state_contains_only_lightweight_metadata() {
    use crate::runtime::background_tasks::TaskStore;

    let (os, storage) = build_integration_os();
    let run_tool = AgentRunTool::new(os, Arc::new(BackgroundTaskManager::new()));

    let mut fix = TestFixture::new();
    fix.run_config = integration_caller_scope(
        json!({}),
        "parent-run-1",
        vec![crate::contracts::thread::Message::user("hi")],
    );

    let result = run_tool
        .execute(
            json!({
                "agent_id": "worker",
                "prompt": "task",
                "background": false
            }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    let run_id = result.data["run_id"].as_str().unwrap();

    let task_store = TaskStore::new(storage as Arc<dyn crate::contracts::storage::ThreadStore>);
    let task = task_store
        .load_task(run_id)
        .await
        .unwrap()
        .expect("task should be persisted");
    let task_entry = serde_json::to_value(task).unwrap();

    // Should have lightweight fields.
    assert!(
        task_entry["metadata"]["thread_id"].as_str().is_some(),
        "should have thread_id in metadata"
    );
    assert!(
        task_entry["metadata"]["agent_id"].as_str().is_some(),
        "should have agent_id in metadata"
    );
    assert!(
        task_entry["status"].as_str().is_some(),
        "should have status"
    );
    assert!(
        task_entry["task_type"].as_str().is_some(),
        "should have task_type"
    );

    // Should NOT have embedded thread or messages.
    assert!(
        task_entry.get("thread").is_none() || task_entry["thread"].is_null(),
        "TaskState should NOT contain embedded Thread"
    );
    assert!(
        task_entry.get("messages").is_none() || task_entry["messages"].is_null(),
        "TaskState should NOT contain messages"
    );
    assert!(
        task_entry.get("state").is_none() || task_entry["state"].is_null(),
        "TaskState should NOT contain state snapshot"
    );
    assert!(
        task_entry.get("patches").is_none() || task_entry["patches"].is_null(),
        "TaskState should NOT contain patches"
    );
}

#[tokio::test]
async fn integration_background_sub_agent_persists_to_store_after_completion() {
    use crate::contracts::storage::ThreadReader;

    let (os, storage) = build_integration_os();
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let run_tool = AgentRunTool::new(os, bg_mgr.clone());

    let mut fix = TestFixture::new();
    fix.run_config = integration_caller_scope(
        json!({}),
        "parent-run-1",
        vec![crate::contracts::thread::Message::user("hi")],
    );

    let result = run_tool
        .execute(
            json!({
                "agent_id": "worker",
                "prompt": "background task",
                "background": true
            }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.data["status"], json!("running"));
    let run_id = result.data["run_id"].as_str().unwrap().to_string();
    let child_thread_id = super::super::tools::sub_agent_thread_id(&run_id);

    // Wait for background completion (SlowTerminatePlugin sleeps 120ms then terminates).
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify: child thread was persisted to ThreadStore.
    let child_head = storage
        .load(&child_thread_id)
        .await
        .expect("load should not error")
        .expect("child thread should exist in store after background completion");

    assert_eq!(child_head.thread.id, child_thread_id);
    assert!(
        child_head.thread.parent_thread_id.as_deref() == Some("parent-thread"),
        "child thread should have parent_thread_id"
    );

    // Verify: BackgroundTaskManager shows completed.
    let task = bg_mgr
        .get("parent-thread", &run_id)
        .await
        .expect("task should exist in bg manager");
    assert!(
        task.status == crate::runtime::background_tasks::TaskStatus::Completed
            || task.status == crate::runtime::background_tasks::TaskStatus::Failed,
        "background run should be terminal after completion: {:?}",
        task.status
    );
}

#[tokio::test]
async fn integration_background_sub_agent_preserves_parent_run_id_in_deltas() {
    use crate::contracts::storage::ThreadSync;

    let (os, storage) = build_integration_os();

    let run_tool = AgentRunTool::new(os, Arc::new(BackgroundTaskManager::new()));

    let mut fix = TestFixture::new();
    fix.run_config = integration_caller_scope(
        json!({}),
        "parent-run-lineage",
        vec![crate::contracts::thread::Message::user("hi")],
    );

    let result = run_tool
        .execute(
            json!({
                "agent_id": "worker",
                "prompt": "background lineage check",
                "background": true
            }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    let run_id = result.data["run_id"].as_str().unwrap().to_string();
    let child_thread_id = super::super::tools::sub_agent_thread_id(&run_id);

    let mut deltas = Vec::new();
    for _ in 0..20 {
        match storage.load_deltas(&child_thread_id, 0).await {
            Ok(found) if !found.is_empty() => {
                deltas = found;
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    assert!(
        !deltas.is_empty(),
        "expected at least one persisted delta for child thread"
    );
    assert_eq!(
        deltas[0].parent_run_id.as_deref(),
        Some("parent-run-lineage"),
        "background sub-agent delta should retain parent_run_id lineage"
    );
}

#[tokio::test]
async fn integration_fork_context_passes_state_to_sub_agent_thread() {
    use crate::contracts::storage::ThreadReader;

    let (os, storage) = build_integration_os();

    let run_tool = AgentRunTool::new(os, Arc::new(BackgroundTaskManager::new()));

    let fork_state = json!({
        "project": "tirea",
        "context": {"depth": 3}
    });
    let fork_messages = vec![
        crate::contracts::thread::Message::user("analyze the code"),
        crate::contracts::thread::Message::assistant("I'll start analyzing."),
    ];

    let mut fix = TestFixture::new();
    fix.run_config = integration_caller_scope(fork_state.clone(), "parent-run-1", fork_messages);

    let result = run_tool
        .execute(
            json!({
                "agent_id": "worker",
                "prompt": "continue analysis",
                "fork_context": true,
                "background": false
            }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    let run_id = result.data["run_id"].as_str().unwrap();
    let child_thread_id = super::super::tools::sub_agent_thread_id(run_id);

    // Verify: child thread exists in store with forked content.
    let child_head = storage
        .load(&child_thread_id)
        .await
        .expect("load should not error")
        .expect("child thread should exist");

    // Child should have messages (at minimum the fork + user prompt).
    assert!(
        child_head.thread.messages.len() >= 2,
        "child should have forked messages + user prompt, got {} messages",
        child_head.thread.messages.len()
    );

    // The forked state should be the initial state of the child thread.
    let child_state = child_head.thread.rebuild_state().unwrap();
    assert_eq!(
        child_state["project"],
        json!("tirea"),
        "child should have forked state: project"
    );
    assert_eq!(
        child_state["context"]["depth"],
        json!(3),
        "child should have forked state: nested context"
    );
}

#[tokio::test]
async fn integration_multiple_parallel_sub_agents_create_independent_threads() {
    use crate::contracts::storage::ThreadReader;

    let (os, storage) = build_integration_os();

    let run_tool = AgentRunTool::new(os, Arc::new(BackgroundTaskManager::new()));

    let mut run_ids = Vec::new();
    for i in 0..3 {
        let agent_id = match i {
            0 => "worker",
            1 => "reviewer",
            _ => "worker",
        };
        let mut fix = TestFixture::new();
        fix.run_config = integration_caller_scope(
            json!({}),
            "parent-run-1",
            vec![crate::contracts::thread::Message::user("hi")],
        );

        let result = run_tool
            .execute(
                json!({
                    "agent_id": agent_id,
                    "prompt": format!("task {i}"),
                    "background": true
                }),
                &fix.ctx_with(&format!("call-{i}"), "tool:agent_run"),
            )
            .await
            .unwrap();
        assert_eq!(result.status, ToolStatus::Success);
        run_ids.push(result.data["run_id"].as_str().unwrap().to_string());
    }

    // Wait for all background runs to complete.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify: each sub-agent has its own independent thread in store.
    for run_id in &run_ids {
        let child_thread_id = super::super::tools::sub_agent_thread_id(run_id);
        let child_head = storage
            .load(&child_thread_id)
            .await
            .expect("load should not error")
            .unwrap_or_else(|| panic!("child thread '{child_thread_id}' should exist in store"));

        assert_eq!(child_head.thread.id, child_thread_id);
        assert_eq!(
            child_head.thread.parent_thread_id.as_deref(),
            Some("parent-thread")
        );
    }

    // Verify: all thread IDs are unique.
    let thread_ids: std::collections::HashSet<String> = run_ids
        .iter()
        .map(|rid| super::super::tools::sub_agent_thread_id(rid))
        .collect();
    assert_eq!(thread_ids.len(), 3, "all child threads should be unique");
}

#[tokio::test]
async fn integration_background_stop_resume_full_lifecycle_with_store() {
    use crate::contracts::storage::ThreadReader;

    let (os, storage) = build_integration_os();
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let run_tool = AgentRunTool::new(os, bg_mgr.clone());

    // Phase 1: Launch background.
    let mut fix = TestFixture::new();
    fix.run_config = integration_caller_scope(
        json!({}),
        "parent-run-1",
        vec![crate::contracts::thread::Message::user("hi")],
    );

    let started = run_tool
        .execute(
            json!({
                "agent_id": "worker",
                "prompt": "long task",
                "background": true
            }),
            &fix.ctx_with("call-start", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(started.data["status"], json!("running"));
    let run_id = started.data["run_id"].as_str().unwrap().to_string();
    let child_thread_id = super::super::tools::sub_agent_thread_id(&run_id);

    // Phase 2: Cancel immediately via BackgroundTaskManager.
    bg_mgr.cancel("parent-thread", &run_id).await.unwrap();

    // Wait for background task to flush.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify: child thread exists in store (created during prepare_run before cancellation).
    let child_head = storage.load(&child_thread_id).await.unwrap();
    assert!(
        child_head.is_some(),
        "child thread should exist in store even after stop"
    );

    // Verify: bg_manager shows the task as cancelled.
    let cancelled_task = bg_mgr.get("parent-thread", &run_id).await.unwrap();
    assert_eq!(
        cancelled_task.status,
        crate::runtime::background_tasks::TaskStatus::Cancelled,
        "manager should show task as cancelled after stop"
    );

    // Phase 3: Resume (foreground). The bg_manager has the task as Cancelled,
    // which is terminal — run_tool will return the current status.
    let resumed = run_tool
        .execute(
            json!({
                "run_id": &run_id,
                "prompt": "continue",
                "background": false
            }),
            &fix.ctx_with("call-resume", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(resumed.status, ToolStatus::Success);
    let resumed_status = resumed.data["status"].as_str().unwrap();
    assert!(
        resumed_status == "completed"
            || resumed_status == "failed"
            || resumed_status == "cancelled",
        "resumed run should reach terminal state, got: {resumed_status}"
    );

    // Verify: the child thread was reused (same thread_id), not a new one.
    let final_head = storage.load(&child_thread_id).await.unwrap();
    assert!(
        final_head.is_some(),
        "resumed child should use the same thread_id"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Integration tests: sub-agent with mock LLM and tool execution
// ══════════════════════════════════════════════════════════════════════════════

/// Mock LLM that returns pre-configured responses.
/// First call returns a tool call, subsequent calls return text (triggering NaturalEnd).
struct ToolCallMockLlm {
    tool_name: String,
    tool_args: serde_json::Value,
    call_count: std::sync::atomic::AtomicUsize,
}

impl ToolCallMockLlm {
    fn new(tool_name: &str, tool_args: serde_json::Value) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            tool_args,
            call_count: std::sync::atomic::AtomicUsize::new(0),
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
impl crate::loop_runtime::loop_runner::LlmExecutor for ToolCallMockLlm {
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
    ) -> genai::Result<crate::loop_runtime::loop_runner::LlmEventStream> {
        use genai::chat::{ChatStreamEvent, MessageContent, StreamChunk, StreamEnd, ToolChunk};

        let n = self
            .call_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        if n == 0 {
            // First call: return a tool call.
            let tc = genai::chat::ToolCall {
                call_id: "tc-001".to_string(),
                fn_name: self.tool_name.clone(),
                fn_arguments: serde_json::Value::String(self.tool_args.to_string()),
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
            // Subsequent calls: return text (triggering NaturalEnd).
            let events = vec![
                Ok(ChatStreamEvent::Start),
                Ok(ChatStreamEvent::Chunk(StreamChunk {
                    content: format!("analysis complete (call #{n})"),
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

/// A simple echo tool that returns its arguments.
#[derive(Debug)]
struct EchoTool;

#[async_trait]
impl crate::contracts::runtime::tool_call::Tool for EchoTool {
    fn descriptor(&self) -> crate::contracts::runtime::tool_call::ToolDescriptor {
        crate::contracts::runtime::tool_call::ToolDescriptor::new("echo", "Echo", "Echoes input")
            .with_parameters(json!({"type": "object", "properties": {"input": {"type": "string"}}}))
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &crate::contracts::ToolCallContext<'_>,
    ) -> Result<
        crate::contracts::runtime::tool_call::ToolResult,
        crate::contracts::runtime::tool_call::ToolError,
    > {
        Ok(crate::contracts::runtime::tool_call::ToolResult::success(
            "echo",
            json!({"echoed": args}),
        ))
    }
}

/// A counting tool that tracks how many times it's been called.
#[derive(Debug)]
struct CountingTool {
    count: std::sync::atomic::AtomicUsize,
}

impl CountingTool {
    fn new() -> Self {
        Self {
            count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.count.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl crate::contracts::runtime::tool_call::Tool for CountingTool {
    fn descriptor(&self) -> crate::contracts::runtime::tool_call::ToolDescriptor {
        crate::contracts::runtime::tool_call::ToolDescriptor::new(
            "count",
            "Count",
            "Counts invocations",
        )
        .with_parameters(json!({"type": "object"}))
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &crate::contracts::ToolCallContext<'_>,
    ) -> Result<
        crate::contracts::runtime::tool_call::ToolResult,
        crate::contracts::runtime::tool_call::ToolError,
    > {
        let n = self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(crate::contracts::runtime::tool_call::ToolResult::success(
            "count",
            json!({"invocation": n + 1}),
        ))
    }
}

#[tokio::test]
async fn integration_sub_agent_executes_tool_via_mock_llm() {
    use crate::contracts::io::RunRequest;
    use crate::contracts::storage::ThreadReader;

    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_agent("worker", crate::runtime::AgentDefinition::new("mock"))
        .with_tools(HashMap::from([(
            "echo".to_string(),
            Arc::new(EchoTool) as Arc<dyn crate::contracts::runtime::tool_call::Tool>,
        )]))
        .build()
        .unwrap();

    // Resolve, inject mock LLM, prepare, execute.
    let mut resolved = os.resolve("worker").unwrap();
    resolved.agent = resolved
        .agent
        .with_llm_executor(Arc::new(ToolCallMockLlm::new(
            "echo",
            json!({"input": "hello world"}),
        ))
            as Arc<dyn crate::loop_runtime::loop_runner::LlmExecutor>);

    let child_thread_id = "sub-agent-tool-test";
    let prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "worker".to_string(),
                thread_id: Some(child_thread_id.to_string()),
                run_id: Some("run-tool-test".to_string()),
                parent_run_id: Some("parent-run-1".to_string()),
                parent_thread_id: Some("parent-thread".to_string()),
                resource_id: None,
                origin: RunOrigin::default(),
                state: None,
                messages: vec![crate::contracts::thread::Message::user("call echo tool")],
                initial_decisions: vec![],
            },
            resolved,
        )
        .await
        .unwrap();

    let run = AgentOs::execute_prepared(prepared).unwrap();
    let events: Vec<_> = run.events.collect().await;

    // Verify events include tool call and tool result.
    let has_tool_call_done = events
        .iter()
        .any(|ev| matches!(ev, AgentEvent::ToolCallDone { .. }));
    assert!(
        has_tool_call_done,
        "events should include ToolCallDone for echo tool"
    );

    let has_run_finish = events
        .iter()
        .any(|ev| matches!(ev, AgentEvent::RunFinish { .. }));
    assert!(has_run_finish, "events should include RunFinish");

    // Verify: child thread in store has tool call + tool result messages.
    let child_head = storage
        .load(child_thread_id)
        .await
        .unwrap()
        .expect("child thread should exist");

    let messages = &child_head.thread.messages;
    // Expected message sequence:
    // 1. User message ("call echo tool")
    // 2. Assistant message with tool_calls (from mock LLM)
    // 3. Tool result message
    // 4. Assistant message ("analysis complete")
    assert!(
        messages.len() >= 4,
        "child thread should have user + assistant(tool_call) + tool_result + assistant messages, got {}",
        messages.len()
    );

    // Verify user message.
    assert_eq!(messages[0].role, Role::User);

    // Verify assistant message with tool calls.
    let assistant_msg = messages
        .iter()
        .find(|m| m.role == Role::Assistant && m.tool_calls.is_some());
    assert!(
        assistant_msg.is_some(),
        "should have an assistant message with tool_calls"
    );
    let tool_calls = assistant_msg.unwrap().tool_calls.as_ref().unwrap();
    assert_eq!(tool_calls[0].name, "echo");

    // Verify tool result message.
    let tool_msg = messages.iter().find(|m| m.role == Role::Tool);
    assert!(tool_msg.is_some(), "should have a tool result message");
    assert_eq!(tool_msg.unwrap().tool_call_id.as_deref(), Some("tc-001"));

    // Verify final assistant message (text response after tool execution).
    let final_assistant = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant && m.tool_calls.is_none());
    assert!(
        final_assistant.is_some(),
        "should have a final text assistant message"
    );

    // Verify run lifecycle state.
    let child_state = child_head.thread.rebuild_state().unwrap();
    assert_eq!(
        child_state["__run"]["status"],
        json!("done"),
        "child run should be done"
    );

    // Verify parent_thread_id lineage.
    assert_eq!(
        child_head.thread.parent_thread_id.as_deref(),
        Some("parent-thread")
    );
}

#[tokio::test]
async fn integration_sub_agent_tool_invocation_counted() {
    use crate::contracts::io::RunRequest;

    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let counting_tool = Arc::new(CountingTool::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_agent("worker", crate::runtime::AgentDefinition::new("mock"))
        .with_tools(HashMap::from([(
            "count".to_string(),
            counting_tool.clone() as Arc<dyn crate::contracts::runtime::tool_call::Tool>,
        )]))
        .build()
        .unwrap();

    let mut resolved = os.resolve("worker").unwrap();
    resolved.agent = resolved
        .agent
        .with_llm_executor(Arc::new(ToolCallMockLlm::new("count", json!({})))
            as Arc<dyn crate::loop_runtime::loop_runner::LlmExecutor>);

    let prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "worker".to_string(),
                thread_id: Some("sub-agent-count-test".to_string()),
                run_id: Some("run-count-test".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: RunOrigin::default(),
                state: None,
                messages: vec![crate::contracts::thread::Message::user("count it")],
                initial_decisions: vec![],
            },
            resolved,
        )
        .await
        .unwrap();

    let run = AgentOs::execute_prepared(prepared).unwrap();
    let _events: Vec<_> = run.events.collect().await;

    // The counting tool should have been called exactly once.
    assert_eq!(
        counting_tool.call_count(),
        1,
        "counting tool should be invoked once by the mock LLM"
    );
}

#[tokio::test]
async fn integration_consecutive_runs_on_same_thread_accumulate_messages() {
    use crate::contracts::io::RunRequest;
    use crate::contracts::storage::ThreadReader;

    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("mock")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();

    let thread_id = "sub-agent-multi-run";

    // Run 1: initial prompt.
    let resolved1 = os.resolve("worker").unwrap();
    let prepared1 = os
        .prepare_run(
            RunRequest {
                agent_id: "worker".to_string(),
                thread_id: Some(thread_id.to_string()),
                run_id: Some("run-1".to_string()),
                parent_run_id: None,
                parent_thread_id: Some("parent-thread".to_string()),
                resource_id: None,
                origin: RunOrigin::default(),
                state: None,
                messages: vec![crate::contracts::thread::Message::user("first task")],
                initial_decisions: vec![],
            },
            resolved1,
        )
        .await
        .unwrap();
    let run1 = AgentOs::execute_prepared(prepared1).unwrap();
    let _: Vec<_> = run1.events.collect().await;

    let head1 = storage.load(thread_id).await.unwrap().unwrap();
    let msg_count_after_run1 = head1.thread.messages.len();
    assert!(
        msg_count_after_run1 >= 1,
        "should have at least the user message after run 1"
    );

    // Run 2: resume with new prompt on same thread.
    let resolved2 = os.resolve("worker").unwrap();
    let prepared2 = os
        .prepare_run(
            RunRequest {
                agent_id: "worker".to_string(),
                thread_id: Some(thread_id.to_string()),
                run_id: Some("run-2".to_string()),
                parent_run_id: None,
                parent_thread_id: Some("parent-thread".to_string()),
                resource_id: None,
                origin: RunOrigin::default(),
                state: None,
                messages: vec![crate::contracts::thread::Message::user("second task")],
                initial_decisions: vec![],
            },
            resolved2,
        )
        .await
        .unwrap();
    let run2 = AgentOs::execute_prepared(prepared2).unwrap();
    let _: Vec<_> = run2.events.collect().await;

    let head2 = storage.load(thread_id).await.unwrap().unwrap();
    let msg_count_after_run2 = head2.thread.messages.len();

    // Run 2 should have more messages (accumulated from both runs).
    assert!(
        msg_count_after_run2 > msg_count_after_run1,
        "messages should accumulate: run1={msg_count_after_run1}, run2={msg_count_after_run2}"
    );

    // Both user messages should be present.
    let user_messages: Vec<&str> = head2
        .thread
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| m.content.as_str())
        .collect();
    assert!(
        user_messages.contains(&"first task"),
        "should contain first task message"
    );
    assert!(
        user_messages.contains(&"second task"),
        "should contain second task message"
    );

    // Run lifecycle should reflect run-2.
    let state = head2.thread.rebuild_state().unwrap();
    assert_eq!(state["__run"]["id"], json!("run-2"));
}

#[tokio::test]
async fn integration_sub_agent_thread_independent_from_parent_thread() {
    use crate::contracts::io::RunRequest;
    use crate::contracts::storage::ThreadReader;

    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "parent-agent",
            crate::runtime::AgentDefinition::new("mock")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .with_agent(
            "child-agent",
            crate::runtime::AgentDefinition::new("mock")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();

    // Create parent thread.
    let parent_resolved = os.resolve("parent-agent").unwrap();
    let parent_prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "parent-agent".to_string(),
                thread_id: Some("parent-tid".to_string()),
                run_id: Some("parent-rid".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: RunOrigin::default(),
                state: Some(json!({"parent_data": "secret"})),
                messages: vec![crate::contracts::thread::Message::user("parent task")],
                initial_decisions: vec![],
            },
            parent_resolved,
        )
        .await
        .unwrap();
    let parent_run = AgentOs::execute_prepared(parent_prepared).unwrap();
    let _: Vec<_> = parent_run.events.collect().await;

    // Create child thread with parent lineage.
    let child_resolved = os.resolve("child-agent").unwrap();
    let child_prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "child-agent".to_string(),
                thread_id: Some("child-tid".to_string()),
                run_id: Some("child-rid".to_string()),
                parent_run_id: Some("parent-rid".to_string()),
                parent_thread_id: Some("parent-tid".to_string()),
                resource_id: None,
                origin: RunOrigin::default(),
                state: None,
                messages: vec![crate::contracts::thread::Message::user("child task")],
                initial_decisions: vec![],
            },
            child_resolved,
        )
        .await
        .unwrap();
    let child_run = AgentOs::execute_prepared(child_prepared).unwrap();
    let _: Vec<_> = child_run.events.collect().await;

    // Verify: both threads exist independently.
    let parent_head = storage.load("parent-tid").await.unwrap().unwrap();
    let child_head = storage.load("child-tid").await.unwrap().unwrap();

    // Parent should NOT contain child's messages.
    let parent_msg_contents: Vec<&str> = parent_head
        .thread
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert!(
        !parent_msg_contents.contains(&"child task"),
        "parent thread should not contain child messages"
    );

    // Child should NOT contain parent's messages.
    let child_msg_contents: Vec<&str> = child_head
        .thread
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert!(
        !child_msg_contents.contains(&"parent task"),
        "child thread should not contain parent messages"
    );

    // Child should NOT have parent's initial state.
    let child_state = child_head.thread.rebuild_state().unwrap();
    assert!(
        child_state.get("parent_data").is_none() || child_state["parent_data"].is_null(),
        "child should not inherit parent's state unless forked"
    );

    // Child should have parent lineage.
    assert_eq!(
        child_head.thread.parent_thread_id.as_deref(),
        Some("parent-tid")
    );
}

#[tokio::test]
async fn integration_task_output_reads_tool_result_from_sub_agent() {
    use crate::contracts::io::RunRequest;

    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_agent("worker", crate::runtime::AgentDefinition::new("mock"))
        .with_tools(HashMap::from([(
            "echo".to_string(),
            Arc::new(EchoTool) as Arc<dyn crate::contracts::runtime::tool_call::Tool>,
        )]))
        .build()
        .unwrap();

    // Run sub-agent with mock LLM that calls echo tool.
    let mut resolved = os.resolve("worker").unwrap();
    resolved.agent = resolved
        .agent
        .with_llm_executor(
            Arc::new(ToolCallMockLlm::new("echo", json!({"input": "test data"})))
                as Arc<dyn crate::loop_runtime::loop_runner::LlmExecutor>,
        );

    let child_thread_id = "sub-agent-output-test";
    let prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "worker".to_string(),
                thread_id: Some(child_thread_id.to_string()),
                run_id: Some("run-output-test".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: RunOrigin::default(),
                state: None,
                messages: vec![crate::contracts::thread::Message::user("echo test")],
                initial_decisions: vec![],
            },
            resolved,
        )
        .await
        .unwrap();
    let run = AgentOs::execute_prepared(prepared).unwrap();
    let _: Vec<_> = run.events.collect().await;

    // Now use TaskOutputTool to read the output.
    let output_tool = TaskOutputTool::new(
        Arc::new(BackgroundTaskManager::new()),
        Some(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>),
    );
    let task_store = crate::runtime::background_tasks::TaskStore::new(
        storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>
    );
    task_store
        .create_task(crate::runtime::background_tasks::NewTaskSpec {
            task_id: "run-output-test".to_string(),
            owner_thread_id: "parent-thread".to_string(),
            task_type: "agent_run".to_string(),
            description: "agent:worker".to_string(),
            parent_task_id: None,
            supports_resume: true,
            metadata: json!({
                "thread_id": child_thread_id,
                "agent_id": "worker"
            }),
        })
        .await
        .unwrap();
    task_store
        .persist_foreground_result(
            "run-output-test",
            crate::runtime::background_tasks::TaskStatus::Completed,
            None,
            None,
        )
        .await
        .unwrap();

    let mut fix = TestFixture::new();
    fix.run_config = integration_caller_scope(json!({}), "parent-run-1", vec![]);
    let result = output_tool
        .execute(
            json!({ "task_id": "run-output-test" }),
            &fix.ctx_with("call-output", "tool:task_output"),
        )
        .await
        .unwrap();

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.data["agent_id"], json!("worker"));
    assert_eq!(result.data["task_id"], json!("run-output-test"));
    assert_eq!(result.data["status"], json!("completed"));

    // The output should contain the last assistant message (the text response after tool execution).
    let output = result.data["output"].as_str();
    assert!(
        output.is_some(),
        "task_output should return the last assistant message"
    );
    assert!(
        output.unwrap().contains("analysis complete"),
        "output should contain the mock LLM's final response: got {:?}",
        output
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Integration tests: BackgroundTaskManager integration with agent tools
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn integration_background_run_tracked_in_background_task_manager() {
    let (os, _storage) = build_integration_os();
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let run_tool = AgentRunTool::new(os, bg_mgr.clone());

    let mut fix = TestFixture::new();
    fix.run_config = integration_caller_scope(
        json!({}),
        "parent-run-1",
        vec![crate::contracts::thread::Message::user("hi")],
    );

    let result = run_tool
        .execute(
            json!({
                "agent_id": "worker",
                "prompt": "background task",
                "background": true
            }),
            &fix.ctx_with("call-bg", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.data["status"], json!("running"));
    let run_id = result.data["run_id"].as_str().unwrap().to_string();

    // The run should be visible in BackgroundTaskManager.
    let task = bg_mgr.get("parent-thread", &run_id).await;
    assert!(
        task.is_some(),
        "background run should be tracked in manager"
    );
    let task = task.unwrap();
    assert_eq!(task.task_type, "agent_run");
    assert!(task.description.contains("worker"));
    assert_eq!(
        task.status,
        crate::runtime::background_tasks::TaskStatus::Running
    );

    // Wait for background run to complete (SlowTerminatePlugin terminates quickly).
    tokio::time::sleep(Duration::from_millis(500)).await;

    // After completion, manager should reflect terminal status.
    let completed_task = bg_mgr.get("parent-thread", &run_id).await.unwrap();
    assert!(
        completed_task.status != crate::runtime::background_tasks::TaskStatus::Running,
        "task should no longer be running after completion"
    );
}

#[tokio::test]
async fn integration_background_stop_cancels_in_both_systems() {
    let (os, _storage) = build_integration_os();
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let run_tool = AgentRunTool::new(os, bg_mgr.clone());

    let mut fix = TestFixture::new();
    fix.run_config = integration_caller_scope(
        json!({}),
        "parent-run-1",
        vec![crate::contracts::thread::Message::user("hi")],
    );

    // Launch background.
    let result = run_tool
        .execute(
            json!({
                "agent_id": "worker",
                "prompt": "long task",
                "background": true
            }),
            &fix.ctx_with("call-bg", "tool:agent_run"),
        )
        .await
        .unwrap();
    let run_id = result.data["run_id"].as_str().unwrap().to_string();

    // Verify running in manager.
    let task = bg_mgr.get("parent-thread", &run_id).await.unwrap();
    assert_eq!(
        task.status,
        crate::runtime::background_tasks::TaskStatus::Running
    );

    // Cancel via BackgroundTaskManager directly.
    bg_mgr.cancel("parent-thread", &run_id).await.unwrap();

    // Wait for background task to flush.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // BackgroundTaskManager should reflect cancellation.
    let cancelled_task = bg_mgr.get("parent-thread", &run_id).await.unwrap();
    assert_eq!(
        cancelled_task.status,
        crate::runtime::background_tasks::TaskStatus::Cancelled,
        "manager should show task as cancelled"
    );
}

#[tokio::test]
async fn integration_background_tasks_plugin_includes_all_task_types() {
    use crate::contracts::runtime::behavior::AgentBehavior;
    use crate::runtime::background_tasks::{BackgroundTasksPlugin, SpawnParams};

    let bg_mgr = Arc::new(BackgroundTaskManager::new());

    // Spawn a generic task.
    bg_mgr
        .spawn("thread-1", "shell", "build project", |cancel| async move {
            cancel.cancelled().await;
            crate::runtime::background_tasks::TaskResult::Cancelled
        })
        .await;

    // Spawn an agent_run task.
    let token = crate::loop_runtime::loop_runner::RunCancellationToken::new();
    bg_mgr
        .spawn_with_id(
            SpawnParams {
                task_id: "agent-run-123".to_string(),
                owner_thread_id: "thread-1".to_string(),
                task_type: "agent_run".to_string(),
                description: "agent:worker".to_string(),
                parent_task_id: None,
                metadata: serde_json::json!({}),
            },
            token.clone(),
            |cancel: crate::loop_runtime::loop_runner::RunCancellationToken| async move {
                cancel.cancelled().await;
                crate::runtime::background_tasks::TaskResult::Cancelled
            },
        )
        .await;

    let plugin = BackgroundTasksPlugin::new(bg_mgr.clone());

    let doc = tirea_state::DocCell::new(json!({}));
    let mut rc = crate::contracts::RunConfig::new();
    rc.set("__agent_tool_caller_thread_id", "thread-1".to_string())
        .unwrap();
    let ctx = crate::contracts::runtime::behavior::ReadOnlyContext::new(
        tirea_contract::runtime::phase::Phase::AfterToolExecute,
        "thread-1",
        &[],
        &rc,
        &doc,
    );

    let actions = plugin.after_tool_execute(&ctx).await;
    let mut reminder = None;
    for action in actions {
        match action {
            crate::contracts::runtime::phase::AfterToolExecuteAction::AddSystemReminder(s) => {
                reminder = Some(s);
            }
            crate::contracts::runtime::phase::AfterToolExecuteAction::State(_) => {}
            crate::contracts::runtime::phase::AfterToolExecuteAction::AddUserMessage(_) => {
                panic!("unexpected user message action")
            }
        }
    }
    let reminder = reminder.expect("background tasks reminder should be emitted");
    // All task types should now appear in the unified reminder.
    assert!(
        reminder.contains("build project"),
        "reminder should include shell task"
    );
    assert!(
        reminder.contains("agent:worker"),
        "reminder should include agent_run tasks"
    );
}
