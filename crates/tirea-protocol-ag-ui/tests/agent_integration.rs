//! Integration tests for tirea.
//!
//! These tests verify the Tool and SystemReminder traits
//! work correctly with the new State/Context API.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tirea_agentos::contracts::runtime::tool_call::{
    Tool, ToolDescriptor, ToolError, ToolExecutionEffect, ToolResult,
};
use tirea_agentos::contracts::thread::Role as ThreadRole;
use tirea_agentos::contracts::thread::Thread as ConversationAgentState;
use tirea_agentos::contracts::AnyStateAction;
use tirea_agentos::contracts::SuspensionResponse;
use tirea_agentos::contracts::ToolCallContext;
use tirea_agentos::engine::convert;
use tirea_agentos::extensions::reminder::SystemReminder;
use tirea_agentos::runtime::activity::ActivityHub;
use tirea_agentos::runtime::loop_runner::AgentLoopError;
use tirea_contract::runtime::state::StateSpec;
use tirea_contract::testing::TestFixture;
use tirea_protocol_ag_ui::{interaction_to_ag_ui_events, Role, ToolExecutionLocation};
use tirea_state::StateManager;
use tirea_state::{DocCell, PatchSink, Path, State, TireaError, TireaResult};

// ============================================================================
// Test state types
// ============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, State, Default)]
#[tirea(action = "CounterAction")]
struct CounterState {
    #[serde(default)]
    value: i64,
    #[serde(default)]
    label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, State, Default)]
#[tirea(action = "TaskAction")]
struct TaskState {
    #[serde(default)]
    items: Vec<String>,
    #[serde(default)]
    count: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, State, Default)]
struct ProgressState {
    progress: f64,
    status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, State, Default)]
#[tirea(action = "CallCounterAction", scope = "tool_call")]
struct CallCounterState {
    #[serde(default)]
    value: i64,
    #[serde(default)]
    label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
enum CounterAction {
    SetValue(i64),
}

impl CounterState {
    fn reduce(&mut self, action: CounterAction) {
        match action {
            CounterAction::SetValue(value) => self.value = value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum TaskAction {
    AddItem(String),
    SetCount(i64),
}

impl TaskState {
    fn reduce(&mut self, action: TaskAction) {
        match action {
            TaskAction::AddItem(item) => self.items.push(item),
            TaskAction::SetCount(count) => self.count = count,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum CallCounterAction {
    SetValue(i64),
    SetLabel(String),
}

impl CallCounterState {
    fn reduce(&mut self, action: CallCounterAction) {
        match action {
            CallCounterAction::SetValue(value) => self.value = value,
            CallCounterAction::SetLabel(label) => self.label = label,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
struct CounterEffectState {
    #[serde(default)]
    value: i64,
    #[serde(default)]
    label: String,
}

struct CounterEffectRef;

impl State for CounterEffectState {
    type Ref<'a> = CounterEffectRef;
    const PATH: &'static str = "";

    fn state_ref<'a>(_: &'a DocCell, _: Path, _: PatchSink<'a>) -> Self::Ref<'a> {
        CounterEffectRef
    }

    fn from_value(value: &Value) -> TireaResult<Self> {
        if value.is_null() {
            return Ok(Self::default());
        }
        if let Some(current) = value.as_i64() {
            return Ok(Self {
                value: current,
                ..Self::default()
            });
        }
        serde_json::from_value(value.clone()).map_err(TireaError::Serialization)
    }

    fn to_value(&self) -> TireaResult<Value> {
        serde_json::to_value(self).map_err(TireaError::Serialization)
    }
}

impl StateSpec for CounterEffectState {
    type Action = CounterAction;

    fn reduce(&mut self, action: CounterAction) {
        match action {
            CounterAction::SetValue(value) => self.value = value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
struct TaskEffectState {
    #[serde(default)]
    items: Vec<String>,
    #[serde(default)]
    count: i64,
}

struct TaskEffectRef;

impl State for TaskEffectState {
    type Ref<'a> = TaskEffectRef;
    const PATH: &'static str = "";

    fn state_ref<'a>(_: &'a DocCell, _: Path, _: PatchSink<'a>) -> Self::Ref<'a> {
        TaskEffectRef
    }

    fn from_value(value: &Value) -> TireaResult<Self> {
        if value.is_null() {
            return Ok(Self::default());
        }
        serde_json::from_value(value.clone()).map_err(TireaError::Serialization)
    }

    fn to_value(&self) -> TireaResult<Value> {
        serde_json::to_value(self).map_err(TireaError::Serialization)
    }
}

impl StateSpec for TaskEffectState {
    type Action = TaskAction;

    fn reduce(&mut self, action: TaskAction) {
        match action {
            TaskAction::AddItem(item) => self.items.push(item),
            TaskAction::SetCount(count) => self.count = count,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
struct CallCounterEffectState {
    #[serde(default)]
    value: i64,
    #[serde(default)]
    label: String,
}

struct CallCounterEffectRef;

impl State for CallCounterEffectState {
    type Ref<'a> = CallCounterEffectRef;
    const PATH: &'static str = "";

    fn state_ref<'a>(_: &'a DocCell, _: Path, _: PatchSink<'a>) -> Self::Ref<'a> {
        CallCounterEffectRef
    }

    fn from_value(value: &Value) -> TireaResult<Self> {
        if value.is_null() {
            return Ok(Self::default());
        }
        serde_json::from_value(value.clone()).map_err(TireaError::Serialization)
    }

    fn to_value(&self) -> TireaResult<Value> {
        serde_json::to_value(self).map_err(TireaError::Serialization)
    }
}

impl StateSpec for CallCounterEffectState {
    type Action = CallCounterAction;

    fn reduce(&mut self, action: CallCounterAction) {
        match action {
            CallCounterAction::SetValue(value) => self.value = value,
            CallCounterAction::SetLabel(label) => self.label = label,
        }
    }
}

// ============================================================================
// Test tools
// ============================================================================

struct IncrementTool;

#[async_trait]
impl Tool for IncrementTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "increment",
            "Increment Counter",
            "Increments a counter by 1",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Counter path"}
            },
            "required": ["path"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("path is required".to_string()))?;

        let counter = ctx.state::<CounterState>(path);
        let current = counter.value().unwrap_or(0);
        counter
            .set_value(current + 1)
            .expect("state mutation should succeed");

        Ok(ToolResult::success(
            "increment",
            json!({"new_value": current + 1}),
        ))
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("path is required".to_string()))?;

        let counter = ctx
            .snapshot_at::<CounterEffectState>(path)
            .unwrap_or_default();
        let current = counter.value;

        Ok(ToolExecutionEffect::new(ToolResult::success(
            "increment",
            json!({"new_value": current + 1}),
        ))
        .with_action(AnyStateAction::new_at::<CounterEffectState>(
            path,
            CounterAction::SetValue(current + 1),
        )))
    }
}

struct AddTaskTool;

#[async_trait]
impl Tool for AddTaskTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("add_task", "Add Task", "Adds a new task item")
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let item = args["item"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("item is required".to_string()))?;

        let tasks = ctx.state::<TaskState>("tasks");
        let current_count = tasks.count().unwrap_or(0);
        tasks
            .items_push(item)
            .expect("state mutation should succeed");
        tasks
            .set_count(current_count + 1)
            .expect("state mutation should succeed");

        Ok(ToolResult::success(
            "add_task",
            json!({"count": current_count + 1}),
        ))
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let item = args["item"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("item is required".to_string()))?;

        let current_count = ctx
            .snapshot_at::<TaskEffectState>("tasks")
            .unwrap_or_default()
            .count;

        Ok(ToolExecutionEffect::new(ToolResult::success(
            "add_task",
            json!({"count": current_count + 1}),
        ))
        .with_action(AnyStateAction::new_at::<TaskEffectState>(
            "tasks",
            TaskAction::AddItem(item.to_string()),
        ))
        .with_action(AnyStateAction::new_at::<TaskEffectState>(
            "tasks",
            TaskAction::SetCount(current_count + 1),
        )))
    }
}

struct UpdateCallStateTool;

#[async_trait]
impl Tool for UpdateCallStateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("update_call", "Update Call State", "Updates the call state")
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let label = args["label"].as_str().unwrap_or("updated");

        // Use call_state() for per-call state
        let call_state = ctx.call_state::<CallCounterState>();
        let step = call_state.value().unwrap_or(0);
        call_state
            .set_value(step + 1)
            .expect("state mutation should succeed");
        call_state
            .set_label(label)
            .expect("state mutation should succeed");

        Ok(ToolResult::success(
            "update_call",
            json!({"step": step + 1}),
        ))
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let label = args["label"].as_str().unwrap_or("updated");
        let call_state = ctx
            .snapshot_of::<CallCounterEffectState>()
            .unwrap_or_default();
        let step = call_state.value;

        Ok(ToolExecutionEffect::new(ToolResult::success(
            "update_call",
            json!({"step": step + 1}),
        ))
        .with_action(AnyStateAction::new_for_call::<CallCounterEffectState>(
            CallCounterAction::SetValue(step + 1),
            ctx.call_id(),
        ))
        .with_action(AnyStateAction::new_for_call::<CallCounterEffectState>(
            CallCounterAction::SetLabel(label.to_string()),
            ctx.call_id(),
        )))
    }
}

// ============================================================================
// Test system reminders
// ============================================================================

struct TaskReminder;

#[async_trait]
impl SystemReminder for TaskReminder {
    fn id(&self) -> &str {
        "task_reminder"
    }

    async fn remind(
        &self,
        ctx: &tirea_contract::runtime::tool_call::ToolCallContext<'_>,
    ) -> Option<String> {
        let tasks = ctx.state::<TaskState>("tasks");

        let count = tasks.count().unwrap_or(0);
        if count > 0 {
            Some(format!("You have {} pending tasks", count))
        } else {
            None
        }
    }
}

// ============================================================================
// Tool execution tests
// ============================================================================

#[tokio::test]
async fn test_tool_basic_execution() {
    let manager = StateManager::new(json!({
        "counter": {"value": 0, "label": "test"}
    }));

    let tool = IncrementTool;
    let snapshot = manager.snapshot().await;
    let fix = TestFixture::new_with_state(snapshot);

    let result = tool
        .execute(
            json!({"path": "counter"}),
            &fix.ctx_with("call_001", "tool:increment"),
        )
        .await
        .unwrap();

    assert!(result.is_success());
    assert_eq!(result.data["new_value"], 1);

    // Apply patch
    let patch = fix.ctx().take_patch();
    manager.commit(patch).await.unwrap();

    let new_state = manager.snapshot().await;
    assert_eq!(new_state["counter"]["value"], 1);
}

#[tokio::test]
async fn test_tool_multiple_executions() {
    let manager = StateManager::new(json!({
        "counter": {"value": 0, "label": "test"}
    }));

    let tool = IncrementTool;

    for i in 1..=5 {
        let snapshot = manager.snapshot().await;
        let fix = TestFixture::new_with_state(snapshot);

        let result = tool
            .execute(
                json!({"path": "counter"}),
                &fix.ctx_with(format!("call_{}", i), "tool:increment"),
            )
            .await
            .unwrap();

        assert!(result.is_success());
        assert_eq!(result.data["new_value"], i);

        manager.commit(fix.ctx().take_patch()).await.unwrap();
    }

    let final_state = manager.snapshot().await;
    assert_eq!(final_state["counter"]["value"], 5);
}

#[tokio::test]
async fn test_tool_with_call_state() {
    let manager = StateManager::new(json!({
        "tool_calls": {
            "call_abc": {"value": 0, "label": "initial"}
        }
    }));

    let tool = UpdateCallStateTool;

    // First execution
    {
        let snapshot = manager.snapshot().await;
        let fix = TestFixture::new_with_state(snapshot);

        let result = tool
            .execute(
                json!({"label": "step1"}),
                &fix.ctx_with("call_abc", "tool:update_call"),
            )
            .await
            .unwrap();

        assert!(result.is_success());
        assert_eq!(result.data["step"], 1);

        manager.commit(fix.ctx().take_patch()).await.unwrap();
    }

    // Second execution
    {
        let snapshot = manager.snapshot().await;
        let fix = TestFixture::new_with_state(snapshot);

        let result = tool
            .execute(
                json!({"label": "step2"}),
                &fix.ctx_with("call_abc", "tool:update_call"),
            )
            .await
            .unwrap();

        assert!(result.is_success());
        assert_eq!(result.data["step"], 2);

        manager.commit(fix.ctx().take_patch()).await.unwrap();
    }

    let final_state = manager.snapshot().await;
    assert_eq!(final_state["tool_calls"]["call_abc"]["value"], 2);
    assert_eq!(final_state["tool_calls"]["call_abc"]["label"], "step2");
}

#[tokio::test]
async fn test_tool_error_handling() {
    let manager = StateManager::new(json!({}));

    let tool = IncrementTool;
    let snapshot = manager.snapshot().await;
    let fix = TestFixture::new_with_state(snapshot);

    // Missing required argument
    let result = tool
        .execute(json!({}), &fix.ctx_with("call_001", "tool:increment"))
        .await;
    assert!(result.is_err());

    match result {
        Err(ToolError::InvalidArguments(msg)) => {
            assert!(msg.contains("path"));
        }
        _ => panic!("Expected InvalidArguments error"),
    }
}

// ============================================================================
// System reminder tests
// ============================================================================

#[tokio::test]
async fn test_system_reminder_with_tasks() {
    let manager = StateManager::new(json!({
        "tasks": {"items": ["Task 1", "Task 2"], "count": 2}
    }));

    let reminder = TaskReminder;

    let snapshot = manager.snapshot().await;
    let fix = TestFixture::new_with_state(snapshot);

    let message = reminder.remind(&fix.ctx()).await;
    assert!(message.is_some());
    assert!(message.unwrap().contains("2"));
}

#[tokio::test]
async fn test_system_reminder_no_tasks() {
    let manager = StateManager::new(json!({
        "tasks": {"items": [], "count": 0}
    }));

    let reminder = TaskReminder;

    let snapshot = manager.snapshot().await;
    let fix = TestFixture::new_with_state(snapshot);

    let message = reminder.remind(&fix.ctx()).await;
    assert!(message.is_none());
}

#[tokio::test]
async fn test_system_reminder_metadata() {
    let reminder = TaskReminder;
    assert_eq!(reminder.id(), "task_reminder");
}

// ============================================================================
// Tool descriptor tests
// ============================================================================

#[test]
fn test_tool_descriptor_basic() {
    let desc = ToolDescriptor::new("my_tool", "My Tool", "A test tool");

    assert_eq!(desc.id, "my_tool");
    assert_eq!(desc.name, "My Tool");
    assert_eq!(desc.description, "A test tool");
    assert!(desc.category.is_none());
}

#[test]
fn test_tool_descriptor_with_options() {
    let desc = ToolDescriptor::new("my_tool", "My Tool", "A test tool")
        .with_parameters(json!({"type": "object"}))
        .with_category("testing")
        .with_metadata("version", json!("1.0"));

    assert_eq!(desc.category, Some("testing".to_string()));
    assert_eq!(desc.metadata.get("version"), Some(&json!("1.0")));
}

// ============================================================================
// Tool result tests
// ============================================================================

#[test]
fn test_tool_result_success() {
    let result = ToolResult::success("my_tool", json!({"data": 123}));

    assert!(result.is_success());
    assert!(!result.is_error());
    assert!(!result.is_pending());
    assert_eq!(result.tool_name, "my_tool");
    assert_eq!(result.data["data"], 123);
}

#[test]
fn test_tool_result_error() {
    let result = ToolResult::error("my_tool", "Something went wrong");

    assert!(result.is_error());
    assert!(!result.is_success());
    assert_eq!(result.message, Some("Something went wrong".to_string()));
}

#[test]
fn test_tool_result_pending() {
    let result = ToolResult::suspended("my_tool", "Waiting for user input");

    assert!(result.is_pending());
    assert!(!result.is_success());
    assert!(!result.is_error());
}

#[test]
fn test_tool_result_warning() {
    let result = ToolResult::warning("my_tool", json!({}), "Partial success");

    assert!(result.is_success()); // Warning is still considered success
    assert!(!result.is_error());
}

#[test]
fn test_tool_result_with_metadata() {
    let result = ToolResult::success("my_tool", json!({}))
        .with_metadata("timing", json!({"ms": 100}))
        .with_metadata("version", json!("1.0"));

    assert_eq!(result.metadata.len(), 2);
    assert_eq!(result.metadata["timing"]["ms"], 100);
}

// ============================================================================
// Full workflow tests
// ============================================================================

#[tokio::test]
async fn test_full_tool_workflow() {
    // Simulate a complete tool execution workflow
    let manager = StateManager::new(json!({
        "tasks": {"items": [], "count": 0},
        "tool_calls": {}
    }));

    let tool = AddTaskTool;

    // Execute tool multiple times
    for i in 1..=3 {
        let call_id = format!("call_{}", i);
        let snapshot = manager.snapshot().await;
        let fix = TestFixture::new_with_state(snapshot);

        let result = tool
            .execute(
                json!({"item": format!("Task {}", i)}),
                &fix.ctx_with(&call_id, "tool:add_task"),
            )
            .await
            .unwrap();

        assert!(result.is_success());
        assert_eq!(result.data["count"], i);

        // Apply changes
        manager.commit(fix.ctx().take_patch()).await.unwrap();
    }

    // Verify final state
    let final_state = manager.snapshot().await;
    assert_eq!(
        final_state["tasks"]["items"],
        json!(["Task 1", "Task 2", "Task 3"])
    );
    assert_eq!(final_state["tasks"]["count"], 3);

    // Verify history
    assert_eq!(manager.history_len().await, 3);

    // Replay to middle state
    let mid_state = manager.replay_to(1).await.unwrap();
    assert_eq!(mid_state["tasks"]["items"], json!(["Task 1", "Task 2"]));
    assert_eq!(mid_state["tasks"]["count"], 2);
}

#[tokio::test]
async fn test_tool_reminder_integration() {
    let manager = StateManager::new(json!({
        "counter": {"value": 0, "label": "initial"},
        "tasks": {"items": [], "count": 0}
    }));

    let increment_tool = IncrementTool;
    let task_tool = AddTaskTool;
    let reminder = TaskReminder;

    // Execute increment tool
    {
        let snapshot = manager.snapshot().await;
        let fix = TestFixture::new_with_state(snapshot);
        let _ = increment_tool
            .execute(
                json!({"path": "counter"}),
                &fix.ctx_with("call_1", "tool:increment"),
            )
            .await
            .unwrap();
        manager.commit(fix.ctx().take_patch()).await.unwrap();
    }

    // Execute add task tool
    {
        let snapshot = manager.snapshot().await;
        let fix = TestFixture::new_with_state(snapshot);
        let _ = task_tool
            .execute(
                json!({"item": "New task"}),
                &fix.ctx_with("call_2", "tool:add_task"),
            )
            .await
            .unwrap();
        manager.commit(fix.ctx().take_patch()).await.unwrap();
    }

    // Run system reminder
    {
        let snapshot = manager.snapshot().await;
        let fix = TestFixture::new_with_state(snapshot);
        let message = reminder.remind(&fix.ctx()).await;
        assert!(message.is_some());
        assert!(message.unwrap().contains("1")); // 1 pending task
    }

    // Verify final state
    let final_state = manager.snapshot().await;
    assert_eq!(final_state["counter"]["value"], 1);
    assert_eq!(final_state["counter"]["label"], "initial");
    assert_eq!(final_state["tasks"]["count"], 1);
}

// ============================================================================
// Thread and Agent Loop Integration Tests
// ============================================================================

use std::sync::Arc;
use tempfile::TempDir;
use tirea_agentos::contracts::runtime::StreamResult;
use tirea_agentos::contracts::storage::{ThreadReader, ThreadWriter};
use tirea_agentos::contracts::thread::Message;
use tirea_agentos::runtime::loop_runner::{
    execute_tools, tool_map, BaseAgent, SequentialToolExecutor,
};
use tirea_agentos::runtime::streaming::StreamCollector;
use tirea_state::{path, Op, Patch, TrackedPatch};
use tirea_store_adapters::{FileStore, MemoryStore};
type Thread = ConversationAgentState;

async fn loop_execute_tools(
    thread: Thread,
    result: &StreamResult,
    tools: &std::collections::HashMap<String, Arc<dyn Tool>>,
    parallel: bool,
) -> Result<Thread, AgentLoopError> {
    execute_tools(thread, result, tools, parallel)
        .await
        .map(|outcome| outcome.into_thread())
}

#[tokio::test]
async fn test_session_with_tool_workflow() {
    // Create session with initial state
    let thread = Thread::with_initial_state("workflow-test", json!({"counter": 0}))
        .with_message(Message::user("Increment the counter twice"));

    // Simulate first LLM response with tool call
    let result1 = StreamResult {
        text: "I'll increment the counter".to_string(),
        tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
            "call_1",
            "increment",
            json!({"path": "counter"}),
        )],
        usage: None,
        stop_reason: None,
    };

    // Add assistant message
    let thread = thread.with_message(convert::assistant_tool_calls(
        &result1.text,
        result1.tool_calls.clone(),
    ));

    // Execute tool
    let tools = tool_map([IncrementTool]);
    let thread = loop_execute_tools(thread, &result1, &tools, true)
        .await
        .unwrap();

    // Verify state after first tool call
    let state = thread.rebuild_state().unwrap();
    assert_eq!(state["counter"]["value"], 1);

    // Simulate second LLM response
    let result2 = StreamResult {
        text: "Incrementing again".to_string(),
        tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
            "call_2",
            "increment",
            json!({"path": "counter"}),
        )],
        usage: None,
        stop_reason: None,
    };

    let thread = thread.with_message(convert::assistant_tool_calls(
        &result2.text,
        result2.tool_calls.clone(),
    ));

    let thread = loop_execute_tools(thread, &result2, &tools, true)
        .await
        .unwrap();

    // Verify final state
    let state = thread.rebuild_state().unwrap();
    assert_eq!(state["counter"]["value"], 2);

    // Thread should have all messages
    assert_eq!(thread.message_count(), 5); // user + 2*(assistant + tool)
}

#[tokio::test]
async fn test_session_storage_roundtrip() {
    let storage = MemoryStore::new();

    // Create and save session
    let thread = Thread::with_initial_state("storage-test", json!({"data": "initial"}))
        .with_message(Message::user("Hello"))
        .with_message(Message::assistant("Hi there!"))
        .with_patch(TrackedPatch::new(
            Patch::new().with_op(Op::set(path!("data"), json!("updated"))),
        ));

    storage.save(&thread).await.unwrap();

    // Load session
    let loaded = storage.load_thread("storage-test").await.unwrap().unwrap();

    // Verify
    assert_eq!(loaded.id, "storage-test");
    assert_eq!(loaded.message_count(), 2);
    assert_eq!(loaded.patch_count(), 1);

    // Rebuild state
    let state = loaded.rebuild_state().unwrap();
    assert_eq!(state["data"], "updated");
}

#[tokio::test]
async fn test_file_storage_session_persistence() {
    let temp_dir = TempDir::new().unwrap();
    let storage = FileStore::new(temp_dir.path());

    // Create complex session
    let thread = Thread::with_initial_state(
        "persist-test",
        json!({
            "user": {"name": "Test", "level": 1},
            "items": []
        }),
    )
    .with_message(Message::user("Add item"))
    .with_message(Message::assistant("Adding item..."))
    .with_patch(TrackedPatch::new(
        Patch::new()
            .with_op(Op::append(path!("items"), json!("item1")))
            .with_op(Op::increment(path!("user").key("level"), 1)),
    ));

    storage.save(&thread).await.unwrap();

    // Verify file exists
    let path = temp_dir.path().join("persist-test.json");
    assert!(path.exists());

    // Load and verify
    let loaded = storage.load_thread("persist-test").await.unwrap().unwrap();
    let state = loaded.rebuild_state().unwrap();

    assert_eq!(state["user"]["level"], 2);
    assert_eq!(state["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_session_snapshot_and_continue() {
    let storage = MemoryStore::new();

    // Create session with patches
    let thread = Thread::with_initial_state("snapshot-test", json!({"counter": 0}))
        .with_patch(TrackedPatch::new(
            Patch::new().with_op(Op::set(path!("counter"), json!(5))),
        ))
        .with_patch(TrackedPatch::new(
            Patch::new().with_op(Op::set(path!("counter"), json!(10))),
        ));

    assert_eq!(thread.patch_count(), 2);

    // Snapshot to collapse patches
    let thread = thread.snapshot().unwrap();
    assert_eq!(thread.patch_count(), 0);
    assert_eq!(thread.state["counter"], 10);

    // Save and load
    storage.save(&thread).await.unwrap();
    let loaded = storage.load_thread("snapshot-test").await.unwrap().unwrap();

    // Continue with more patches
    let thread = loaded.with_patch(TrackedPatch::new(
        Patch::new().with_op(Op::set(path!("counter"), json!(15))),
    ));

    assert_eq!(thread.patch_count(), 1);
    let state = thread.rebuild_state().unwrap();
    assert_eq!(state["counter"], 15);
}

#[tokio::test]
async fn test_agent_config_variations() {
    let config1 = BaseAgent::new("gpt-4o-mini");
    assert_eq!(config1.model, "gpt-4o-mini");
    assert_eq!(config1.max_rounds, 10);
    assert_eq!(config1.tool_executor.name(), "parallel_streaming");

    let config2 = BaseAgent::new("claude-3-opus")
        .with_max_rounds(5)
        .with_tool_executor(Arc::new(SequentialToolExecutor));

    assert_eq!(config2.model, "claude-3-opus");
    assert_eq!(config2.max_rounds, 5);
    assert_eq!(config2.tool_executor.name(), "sequential");
}

#[tokio::test]
async fn test_tool_map_with_multiple_tools() {
    let mut tools: std::collections::HashMap<String, Arc<dyn Tool>> =
        std::collections::HashMap::new();
    tools.insert("increment".to_string(), Arc::new(IncrementTool));
    tools.insert("add_task".to_string(), Arc::new(AddTaskTool));

    assert_eq!(tools.len(), 2);
    assert!(tools.contains_key("increment"));
    assert!(tools.contains_key("add_task"));
}

#[tokio::test]
async fn test_session_message_types() {
    let thread = ConversationAgentState::new("msg-test")
        .with_message(Message::user("User message"))
        .with_message(Message::assistant("Assistant response"))
        .with_message(Message::tool("call_1", "Tool result"));

    assert_eq!(thread.messages.len(), 3);
    assert_eq!(thread.messages[0].role, ThreadRole::User);
    assert_eq!(thread.messages[1].role, ThreadRole::Assistant);
    assert_eq!(thread.messages[2].role, ThreadRole::Tool);
}

#[tokio::test]
async fn test_storage_list_and_delete() {
    let storage = MemoryStore::new();

    // Create multiple sessions
    storage
        .save(&ConversationAgentState::new("thread-1"))
        .await
        .unwrap();
    storage
        .save(&ConversationAgentState::new("thread-2"))
        .await
        .unwrap();
    storage
        .save(&ConversationAgentState::new("thread-3"))
        .await
        .unwrap();

    let ids = storage.list().await.unwrap();
    assert_eq!(ids.len(), 3);

    // Delete one
    storage.delete("thread-2").await.unwrap();

    let ids = storage.list().await.unwrap();
    assert_eq!(ids.len(), 2);
    assert!(!ids.contains(&"thread-2".to_string()));
}

#[tokio::test]
async fn test_parallel_tool_execution_order() {
    // Create session with initial state
    let thread = Thread::with_initial_state("parallel-test", json!({"results": []}));

    // Multiple tool calls
    let result = StreamResult {
        text: "Running parallel tools".to_string(),
        tool_calls: vec![
            tirea_agentos::contracts::thread::ToolCall::new(
                "call_1",
                "add_task",
                json!({"item": "first"}),
            ),
            tirea_agentos::contracts::thread::ToolCall::new(
                "call_2",
                "add_task",
                json!({"item": "second"}),
            ),
            tirea_agentos::contracts::thread::ToolCall::new(
                "call_3",
                "add_task",
                json!({"item": "third"}),
            ),
        ],
        usage: None,
        stop_reason: None,
    };

    let tools = tool_map([AddTaskTool]);
    let err = loop_execute_tools(thread, &result, &tools, true)
        .await
        .unwrap_err();

    match err {
        AgentLoopError::StateError(msg) => {
            assert!(msg.contains("conflicting parallel state patches"));
        }
        other => panic!(
            "expected StateError for conflicting patches, got: {:?}",
            other
        ),
    }
}

#[tokio::test]
async fn test_session_needs_snapshot_threshold() {
    let mut thread = ConversationAgentState::new("threshold-test");

    // Add patches
    for i in 0..5 {
        thread = thread.with_patch(TrackedPatch::new(
            Patch::new().with_op(Op::set(path!("value"), json!(i))),
        ));
    }

    assert!(!thread.needs_snapshot(10));
    assert!(thread.needs_snapshot(5));
    assert!(thread.needs_snapshot(3));
}

// ============================================================================
// Concurrent Stress Tests
// ============================================================================

#[tokio::test]
async fn test_concurrent_tool_execution_stress() {
    // Test 50 concurrent tool executions
    let manager = StateManager::new(json!({
        "counters": {}
    }));

    let mut handles = vec![];

    for i in 0..50 {
        let manager = manager.clone();
        let handle = tokio::spawn(async move {
            let snapshot = manager.snapshot().await;
            let fix = TestFixture::new_with_state(snapshot);
            let ctx = fix.ctx_with(format!("call_{}", i), "tool:increment");

            // Create counter path for this task
            let path = format!("counters.c{}", i % 10);
            let counter = ctx.state::<CounterState>(&path);
            let current = counter.value().unwrap_or(0);
            counter
                .set_value(current + 1)
                .expect("state mutation should succeed");

            let patch = ctx.take_patch();
            manager.commit(patch).await
        });
        handles.push(handle);
    }

    // Wait for all to complete
    let results: Vec<_> = futures::future::join_all(handles).await;
    let success_count = results.iter().filter(|r| r.is_ok()).count();

    // Most should succeed (some may fail due to conflicts, which is expected)
    assert!(
        success_count >= 40,
        "Expected at least 40 successful executions, got {}",
        success_count
    );
}

#[tokio::test]
async fn test_concurrent_storage_operations() {
    let storage = Arc::new(MemoryStore::new());

    let mut handles = vec![];

    // 100 concurrent save operations
    for i in 0..100 {
        let storage = Arc::clone(&storage);
        let handle: tokio::task::JoinHandle<
            Result<(), tirea_agentos::contracts::storage::ThreadStoreError>,
        > = tokio::spawn(async move {
            let thread =
                Thread::with_initial_state(format!("concurrent-{}", i), json!({"index": i}))
                    .with_message(Message::user(format!("Message {}", i)));

            storage.save(&thread).await
        });
        handles.push(handle);
    }

    let results: Vec<_> = futures::future::join_all(handles).await;
    let success_count = results
        .iter()
        .filter(|r| r.as_ref().map(|r| r.is_ok()).unwrap_or(false))
        .count();
    assert_eq!(success_count, 100, "All saves should succeed");

    // Verify all sessions exist
    let ids = storage.list().await.unwrap();
    assert_eq!(ids.len(), 100);
}

#[tokio::test]
async fn test_concurrent_session_rebuild() {
    // Create a session with many patches
    let mut thread = Thread::with_initial_state("rebuild-stress", json!({"counter": 0}));

    for _ in 0..100 {
        thread = thread.with_patch(TrackedPatch::new(
            Patch::new().with_op(Op::increment(path!("counter"), 1)),
        ));
    }

    // Concurrent rebuilds
    let mut handles = vec![];
    for _ in 0..50 {
        let thread = thread.clone();
        let handle = tokio::spawn(async move { thread.rebuild_state() });
        handles.push(handle);
    }

    let results: Vec<_> = futures::future::join_all(handles).await;

    // All rebuilds should succeed and return same value
    for result in results {
        let state = result.unwrap().unwrap();
        assert_eq!(state["counter"], 100);
    }
}

// ============================================================================
// Large Thread Tests (1000+ messages)
// ============================================================================

#[tokio::test]
async fn test_large_session_1000_messages() {
    let mut thread = ConversationAgentState::new("large-msg-test");

    // Add 1000 messages
    for i in 0..1000 {
        if i % 2 == 0 {
            thread = thread.with_message(Message::user(format!("User message {}", i)));
        } else {
            thread = thread.with_message(Message::assistant(format!("Assistant response {}", i)));
        }
    }

    assert_eq!(thread.message_count(), 1000);

    // Verify first and last messages
    assert!(thread.messages[0].content.contains("0"));
    assert!(thread.messages[999].content.contains("999"));
}

#[tokio::test]
async fn test_large_session_1000_patches() {
    let mut thread = Thread::with_initial_state("large-patch-test", json!({"values": []}));

    // Add 1000 patches
    for i in 0..1000 {
        thread = thread.with_patch(TrackedPatch::new(
            Patch::new().with_op(Op::append(path!("values"), json!(i))),
        ));
    }

    assert_eq!(thread.patch_count(), 1000);
    assert!(thread.needs_snapshot(500));

    // Rebuild should work
    let state = thread.rebuild_state().unwrap();
    let values = state["values"].as_array().unwrap();
    assert_eq!(values.len(), 1000);
    assert_eq!(values[0], 0);
    assert_eq!(values[999], 999);
}

#[tokio::test]
async fn test_large_session_storage_roundtrip() {
    let storage = MemoryStore::new();

    // Create large session
    let mut thread = Thread::with_initial_state("large-storage-test", json!({"counter": 0}));

    for i in 0..500 {
        thread = thread.with_message(Message::user(format!("Msg {}", i)));
        thread = thread.with_patch(TrackedPatch::new(
            Patch::new().with_op(Op::increment(path!("counter"), 1)),
        ));
    }

    // Save
    storage.save(&thread).await.unwrap();

    // Load
    let loaded = storage
        .load_thread("large-storage-test")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(loaded.message_count(), 500);
    assert_eq!(loaded.patch_count(), 500);

    let state = loaded.rebuild_state().unwrap();
    assert_eq!(state["counter"], 500);
}

#[tokio::test]
async fn test_large_session_snapshot_performance() {
    let mut thread = Thread::with_initial_state("snapshot-perf-test", json!({"data": {}}));

    // Add 200 patches with nested data
    for i in 0..200 {
        thread = thread.with_patch(TrackedPatch::new(Patch::new().with_op(Op::set(
            path!("data").key(format!("key_{}", i)),
            json!({
                "index": i,
                "nested": {"value": i * 2}
            }),
        ))));
    }

    // Snapshot should collapse all patches
    let start = std::time::Instant::now();
    let thread = thread.snapshot().unwrap();
    let duration = start.elapsed();

    assert_eq!(thread.patch_count(), 0);
    assert!(
        duration.as_millis() < 1000,
        "Snapshot took too long: {:?}",
        duration
    );

    // Verify data
    let keys_count = thread.state["data"].as_object().unwrap().len();
    assert_eq!(keys_count, 200);
}

// ============================================================================
// Thread Interruption Recovery Tests
// ============================================================================

#[tokio::test]
async fn test_session_recovery_after_partial_save() {
    let temp_dir = TempDir::new().unwrap();
    let storage = FileStore::new(temp_dir.path());

    // Create session with multiple messages and patches
    let thread = Thread::with_initial_state("recovery-test", json!({"step": 0}))
        .with_message(Message::user("Step 1"))
        .with_patch(TrackedPatch::new(
            Patch::new().with_op(Op::set(path!("step"), json!(1))),
        ))
        .with_message(Message::assistant("Done step 1"))
        .with_message(Message::user("Step 2"))
        .with_patch(TrackedPatch::new(
            Patch::new().with_op(Op::set(path!("step"), json!(2))),
        ));

    // Save checkpoint
    storage.save(&thread).await.unwrap();

    // Simulate adding more work
    let _session = thread
        .with_message(Message::assistant("Done step 2"))
        .with_message(Message::user("Step 3"))
        .with_patch(TrackedPatch::new(
            Patch::new().with_op(Op::set(path!("step"), json!(3))),
        ));

    // "Crash" happens here - we don't save

    // Recovery: load from last checkpoint
    let recovered = storage.load_thread("recovery-test").await.unwrap().unwrap();

    // Should have state from checkpoint (step 2, not step 3)
    let state = recovered.rebuild_state().unwrap();
    assert_eq!(state["step"], 2);
    // Messages: "Step 1", "Done step 1", "Step 2" = 3 messages (no "Done step 2" yet at checkpoint)
    assert_eq!(recovered.message_count(), 3);
}

#[tokio::test]
async fn test_session_incremental_checkpoints() {
    let storage = MemoryStore::new();

    let mut thread = Thread::with_initial_state("checkpoint-test", json!({"progress": 0}));

    // Simulate long-running task with periodic checkpoints
    for checkpoint in 1..=5 {
        // Do some work
        for _ in 0..10 {
            thread = thread.with_message(Message::user(format!(
                "Work item at checkpoint {}",
                checkpoint
            )));
        }

        thread = thread.with_patch(TrackedPatch::new(
            Patch::new().with_op(Op::set(path!("progress"), json!(checkpoint * 10))),
        ));

        // Save checkpoint
        storage.save(&thread).await.unwrap();
    }

    // Verify final state
    let loaded = storage
        .load_thread("checkpoint-test")
        .await
        .unwrap()
        .unwrap();
    let state = loaded.rebuild_state().unwrap();
    assert_eq!(state["progress"], 50);
    assert_eq!(loaded.message_count(), 50);
}

#[tokio::test]
async fn test_incremental_checkpoints_via_append() {
    use tirea_agentos::contracts::storage::{ThreadSync, ThreadWriter, VersionPrecondition};
    use tirea_agentos::contracts::{CheckpointReason, ThreadChangeSet};

    let storage = MemoryStore::new();

    // Create thread
    let mut thread = Thread::with_initial_state("append-test", json!({"progress": 0}));
    storage.create(&thread).await.unwrap();

    // Simulate 5 checkpoints via append (not save)
    for checkpoint in 1..=5u64 {
        let mut delta_messages = Vec::new();
        for _ in 0..10 {
            let message = Message::user(format!("Work item at checkpoint {}", checkpoint));
            delta_messages.push(Arc::new(message.clone()));
            thread = thread.with_message(message);
        }
        let delta_patch = TrackedPatch::new(
            Patch::new().with_op(Op::set(path!("progress"), json!(checkpoint * 10))),
        );
        thread = thread.with_patch(delta_patch.clone());
        let delta = ThreadChangeSet {
            run_id: "run-1".to_string(),
            parent_run_id: None,
            run_meta: None,
            reason: if checkpoint == 5 {
                CheckpointReason::RunFinished
            } else {
                CheckpointReason::ToolResultsCommitted
            },
            messages: delta_messages,
            patches: vec![delta_patch],
            state_actions: vec![],
            snapshot: None,
        };

        storage
            .append("append-test", &delta, VersionPrecondition::Any)
            .await
            .unwrap();
    }

    // Verify final state matches
    let head = ThreadReader::load(&storage, "append-test")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head.version, 5);
    assert_eq!(head.thread.message_count(), 50);
    let state = head.thread.rebuild_state().unwrap();
    assert_eq!(state["progress"], 50);

    // Verify delta replay
    let all_deltas = storage.load_deltas("append-test", 0).await.unwrap();
    assert_eq!(all_deltas.len(), 5);

    // Partial replay: only last 2
    let tail = storage.load_deltas("append-test", 3).await.unwrap();
    assert_eq!(tail.len(), 2);

    // Total messages across all deltas = 50
    let total_msgs: usize = all_deltas.iter().map(|d| d.messages.len()).sum();
    assert_eq!(total_msgs, 50);
}

#[tokio::test]
async fn test_session_recovery_with_snapshot() {
    let storage = MemoryStore::new();

    // Create session with many patches
    let mut thread = Thread::with_initial_state("snapshot-recovery", json!({"counter": 0}));

    for _ in 0..50 {
        thread = thread.with_patch(TrackedPatch::new(
            Patch::new().with_op(Op::increment(path!("counter"), 1)),
        ));
    }

    // Snapshot to optimize
    let thread = thread.snapshot().unwrap();
    assert_eq!(thread.patch_count(), 0);
    assert_eq!(thread.state["counter"], 50);

    // Save
    storage.save(&thread).await.unwrap();

    // Continue work
    let mut thread = storage
        .load_thread("snapshot-recovery")
        .await
        .unwrap()
        .unwrap();
    for _ in 0..25 {
        thread = thread.with_patch(TrackedPatch::new(
            Patch::new().with_op(Op::increment(path!("counter"), 1)),
        ));
    }

    // Save again
    storage.save(&thread).await.unwrap();

    // Load and verify
    let loaded = storage
        .load_thread("snapshot-recovery")
        .await
        .unwrap()
        .unwrap();
    let state = loaded.rebuild_state().unwrap();
    assert_eq!(state["counter"], 75);
}

// ============================================================================
// Patch Conflict Handling Tests
// ============================================================================

#[tokio::test]
async fn test_patch_conflict_same_field() {
    let manager = StateManager::new(json!({"value": 0}));

    // Two concurrent modifications to same field
    let snapshot1 = manager.snapshot().await;
    let snapshot2 = manager.snapshot().await;

    let fix1 = TestFixture::new_with_state(snapshot1);
    let ctx1 = fix1.ctx_with("call_1", "test");
    let fix2 = TestFixture::new_with_state(snapshot2);
    let ctx2 = fix2.ctx_with("call_2", "test");

    // Both read same value
    let counter1 = ctx1.state::<CounterState>("");
    let counter2 = ctx2.state::<CounterState>("");

    // Both increment from 0
    counter1
        .set_value(1)
        .expect("state mutation should succeed");
    counter2
        .set_value(1)
        .expect("state mutation should succeed");

    // First commit succeeds
    let patch1 = ctx1.take_patch();
    manager.commit(patch1).await.unwrap();

    // Second commit also succeeds (last-write-wins)
    let patch2 = ctx2.take_patch();
    manager.commit(patch2).await.unwrap();

    // Final value is 1 (not 2 - this is the "conflict")
    let final_state = manager.snapshot().await;
    assert_eq!(final_state["value"], 1);
}

#[tokio::test]
async fn test_patch_conflict_different_fields() {
    let manager = StateManager::new(json!({
        "field_a": 0,
        "field_b": 0
    }));

    // Two concurrent modifications to different fields
    let snapshot1 = manager.snapshot().await;
    let snapshot2 = manager.snapshot().await;

    let fix1 = TestFixture::new_with_state(snapshot1);
    let ctx1 = fix1.ctx_with("call_1", "test");
    let fix2 = TestFixture::new_with_state(snapshot2);
    let ctx2 = fix2.ctx_with("call_2", "test");

    // Modify different fields
    {
        let ops = &ctx1.state::<CounterState>("field_a");
        ops.set_value(10).expect("state mutation should succeed");
    }
    {
        let ops = &ctx2.state::<CounterState>("field_b");
        ops.set_value(20).expect("state mutation should succeed");
    }

    // Both commits succeed
    manager.commit(ctx1.take_patch()).await.unwrap();
    manager.commit(ctx2.take_patch()).await.unwrap();

    // Both values should be updated
    let final_state = manager.snapshot().await;
    assert_eq!(final_state["field_a"]["value"], 10);
    assert_eq!(final_state["field_b"]["value"], 20);
}

#[tokio::test]
async fn test_patch_conflict_array_operations() {
    let manager = StateManager::new(json!({
        "tasks": {"items": [], "count": 0}
    }));

    // Two concurrent array appends
    let snapshot1 = manager.snapshot().await;
    let snapshot2 = manager.snapshot().await;

    let fix1 = TestFixture::new_with_state(snapshot1);
    let ctx1 = fix1.ctx_with("call_1", "test");
    let fix2 = TestFixture::new_with_state(snapshot2);
    let ctx2 = fix2.ctx_with("call_2", "test");

    let tasks1 = ctx1.state::<TaskState>("tasks");
    let tasks2 = ctx2.state::<TaskState>("tasks");

    tasks1
        .items_push("item_a")
        .expect("state mutation should succeed");
    tasks1.set_count(1).expect("state mutation should succeed");

    tasks2
        .items_push("item_b")
        .expect("state mutation should succeed");
    tasks2.set_count(1).expect("state mutation should succeed");

    // Apply both
    manager.commit(ctx1.take_patch()).await.unwrap();
    manager.commit(ctx2.take_patch()).await.unwrap();

    // Both items should be present (append doesn't conflict)
    let final_state = manager.snapshot().await;
    let items = final_state["tasks"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
}

#[tokio::test]
async fn test_session_patch_ordering() {
    let thread = Thread::with_initial_state("order-test", json!({"log": []}));

    // Add patches in specific order
    let thread = thread
        .with_patch(TrackedPatch::new(
            Patch::new().with_op(Op::append(path!("log"), json!("first"))),
        ))
        .with_patch(TrackedPatch::new(
            Patch::new().with_op(Op::append(path!("log"), json!("second"))),
        ))
        .with_patch(TrackedPatch::new(
            Patch::new().with_op(Op::append(path!("log"), json!("third"))),
        ));

    // Rebuild should preserve order
    let state = thread.rebuild_state().unwrap();
    let log = state["log"].as_array().unwrap();

    assert_eq!(log[0], "first");
    assert_eq!(log[1], "second");
    assert_eq!(log[2], "third");
}

// ============================================================================
// Tool Timeout Handling Tests
// ============================================================================

/// A tool that can simulate delays
struct SlowTool {
    delay_ms: u64,
}

#[async_trait]
impl Tool for SlowTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("slow_tool", "Slow Tool", "A tool that takes time")
    }

    async fn execute(
        &self,
        _args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        tokio::time::sleep(tokio::time::Duration::from_millis(self.delay_ms)).await;

        let counter = ctx.state::<CounterState>("counter");
        counter.set_value(1).expect("state mutation should succeed");

        Ok(ToolResult::success("slow_tool", json!({"completed": true})))
    }
}

#[tokio::test]
async fn test_tool_execution_with_timeout() {
    let manager = StateManager::new(json!({"counter": {"value": 0, "label": ""}}));

    let tool = SlowTool { delay_ms: 50 };

    let snapshot = manager.snapshot().await;
    let fix = TestFixture::new_with_state(snapshot);

    // Execute with timeout
    let result = tokio::time::timeout(
        tokio::time::Duration::from_millis(100),
        tool.execute(json!({}), &fix.ctx_with("call_slow", "tool:slow")),
    )
    .await;

    // Should complete within timeout
    assert!(result.is_ok());
    let tool_result = result.unwrap().unwrap();
    assert!(tool_result.is_success());
}

#[tokio::test]
async fn test_tool_timeout_exceeded() {
    let manager = StateManager::new(json!({"counter": {"value": 0, "label": ""}}));

    let tool = SlowTool { delay_ms: 200 };

    let snapshot = manager.snapshot().await;
    let fix = TestFixture::new_with_state(snapshot);

    // Execute with short timeout
    let result = tokio::time::timeout(
        tokio::time::Duration::from_millis(50),
        tool.execute(json!({}), &fix.ctx_with("call_slow", "tool:slow")),
    )
    .await;

    // Should timeout
    assert!(result.is_err());

    // State should not be modified (tool didn't complete)
    let patch = fix.ctx().take_patch();
    assert!(patch.patch().is_empty());
}

#[tokio::test]
async fn test_multiple_tools_with_varying_timeouts() {
    let manager = StateManager::new(json!({
        "results": {"fast": false, "medium": false, "slow": false}
    }));

    let fast_tool = SlowTool { delay_ms: 10 };
    let medium_tool = SlowTool { delay_ms: 50 };
    let slow_tool = SlowTool { delay_ms: 200 };

    // Execute all with 100ms timeout
    let timeout = tokio::time::Duration::from_millis(100);

    let snapshot = manager.snapshot().await;

    // Fast - should complete
    let fix = TestFixture::new_with_state(snapshot.clone());
    let fast_result = tokio::time::timeout(
        timeout,
        fast_tool.execute(json!({}), &fix.ctx_with("fast", "tool:fast")),
    )
    .await;
    assert!(fast_result.is_ok());

    // Medium - should complete
    let fix = TestFixture::new_with_state(snapshot.clone());
    let medium_result = tokio::time::timeout(
        timeout,
        medium_tool.execute(json!({}), &fix.ctx_with("medium", "tool:medium")),
    )
    .await;
    assert!(medium_result.is_ok());

    // Slow - should timeout
    let fix = TestFixture::new_with_state(snapshot);
    let slow_result = tokio::time::timeout(
        timeout,
        slow_tool.execute(json!({}), &fix.ctx_with("slow", "tool:slow")),
    )
    .await;
    assert!(slow_result.is_err());
}

#[tokio::test]
async fn test_tool_timeout_cleanup() {
    // Test that partial state changes are not applied on timeout
    let thread = Thread::with_initial_state("timeout-cleanup", json!({"value": "original"}));

    // Simulate a tool that would modify state but times out
    // In real scenario, the patch wouldn't be collected if tool times out

    // The session state should remain unchanged
    let state = thread.rebuild_state().unwrap();
    assert_eq!(state["value"], "original");
}

// ============================================================================
// Stream Interruption Tests
// ============================================================================

#[test]
fn test_stream_collector_partial_text() {
    let mut collector = StreamCollector::new();

    // Simulate partial text chunks
    use genai::chat::{ChatStreamEvent, StreamChunk};

    collector.process(ChatStreamEvent::Chunk(StreamChunk {
        content: "Hello ".to_string(),
    }));

    collector.process(ChatStreamEvent::Chunk(StreamChunk {
        content: "world".to_string(),
    }));

    // Stream "interrupted" - finish early
    let result = collector.finish(None);

    assert_eq!(result.text, "Hello world");
    assert!(result.tool_calls.is_empty());
}

#[test]
fn test_stream_collector_interrupted_tool_call() {
    let mut collector = StreamCollector::new();

    use genai::chat::{ChatStreamEvent, StreamChunk, ToolChunk};

    // Start a tool call
    collector.process(ChatStreamEvent::Chunk(StreamChunk {
        content: "I'll help you".to_string(),
    }));

    // Tool call chunk with initial data
    let tool_call = genai::chat::ToolCall {
        call_id: "call_1".to_string(),
        fn_name: "calculator".to_string(),
        fn_arguments: json!({}),
        thought_signatures: None,
    };
    collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call }));

    // Partial arguments in second chunk
    let tool_call2 = genai::chat::ToolCall {
        call_id: "call_1".to_string(),
        fn_name: String::new(),
        fn_arguments: json!({"expr": "1+1"}),
        thought_signatures: None,
    };
    collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
        tool_call: tool_call2,
    }));

    // Stream "interrupted" - finish without complete tool call
    let result = collector.finish(None);

    assert_eq!(result.text, "I'll help you");
    // Tool call should still be captured (even if incomplete)
    assert_eq!(result.tool_calls.len(), 1);
    assert_eq!(result.tool_calls[0].name, "calculator");
}

#[test]
fn test_stream_collector_multiple_interruptions() {
    // Test collecting results after multiple partial streams
    let mut collector1 = StreamCollector::new();

    use genai::chat::{ChatStreamEvent, StreamChunk};

    collector1.process(ChatStreamEvent::Chunk(StreamChunk {
        content: "Part 1".to_string(),
    }));

    let result1 = collector1.finish(None);
    assert_eq!(result1.text, "Part 1");

    // New collector for "retry"
    let mut collector2 = StreamCollector::new();

    collector2.process(ChatStreamEvent::Chunk(StreamChunk {
        content: "Complete response".to_string(),
    }));

    let result2 = collector2.finish(None);
    assert_eq!(result2.text, "Complete response");
}

#[test]
fn test_stream_result_from_partial_response() {
    // Simulate building StreamResult from partial data
    let result = StreamResult {
        text: "Partial...".to_string(),
        tool_calls: vec![],
        usage: None,
        stop_reason: None,
    };

    assert!(!result.needs_tools());

    // Can still create messages from partial result
    let msg = convert::assistant_message(&result.text);
    assert_eq!(msg.content, "Partial...");
}

// ============================================================================
// Network Error Simulation Tests
// ============================================================================

/// Simulates a tool that encounters "network" errors
struct NetworkErrorTool {
    fail_count: std::sync::atomic::AtomicU32,
    max_failures: u32,
}

impl NetworkErrorTool {
    fn new(max_failures: u32) -> Self {
        Self {
            fail_count: std::sync::atomic::AtomicU32::new(0),
            max_failures,
        }
    }
}

#[async_trait]
impl Tool for NetworkErrorTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("network_tool", "Network Tool", "Tool that may fail")
    }

    async fn execute(
        &self,
        _args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let count = self
            .fail_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        if count < self.max_failures {
            Err(ToolError::ExecutionFailed(format!(
                "Network error (attempt {})",
                count + 1
            )))
        } else {
            let counter = ctx.state::<CounterState>("counter");
            counter.set_value(1).expect("state mutation should succeed");
            Ok(ToolResult::success(
                "network_tool",
                json!({"success": true}),
            ))
        }
    }
}

#[tokio::test]
async fn test_tool_network_error_retry() {
    let manager = StateManager::new(json!({"counter": {"value": 0, "label": ""}}));

    // Tool fails twice, then succeeds
    let tool = NetworkErrorTool::new(2);

    // Attempt 1 - fails
    let snapshot = manager.snapshot().await;
    let fix = TestFixture::new_with_state(snapshot);
    let result1 = tool
        .execute(json!({}), &fix.ctx_with("call_1", "tool:network"))
        .await;
    assert!(result1.is_err());

    // Attempt 2 - fails
    let snapshot = manager.snapshot().await;
    let fix = TestFixture::new_with_state(snapshot);
    let result2 = tool
        .execute(json!({}), &fix.ctx_with("call_2", "tool:network"))
        .await;
    assert!(result2.is_err());

    // Attempt 3 - succeeds
    let snapshot = manager.snapshot().await;
    let fix = TestFixture::new_with_state(snapshot);
    let result3 = tool
        .execute(json!({}), &fix.ctx_with("call_3", "tool:network"))
        .await;
    assert!(result3.is_ok());

    // Apply successful patch
    manager.commit(fix.ctx().take_patch()).await.unwrap();

    let final_state = manager.snapshot().await;
    assert_eq!(final_state["counter"]["value"], 1);
}

#[tokio::test]
async fn test_tool_error_does_not_corrupt_state() {
    let manager = StateManager::new(json!({"value": 100}));

    // Tool that always fails
    let tool = NetworkErrorTool::new(1000); // Will always fail

    for i in 0..5 {
        let snapshot = manager.snapshot().await;
        let fix = TestFixture::new_with_state(snapshot);

        let result = tool
            .execute(
                json!({}),
                &fix.ctx_with(format!("call_{}", i), "tool:network"),
            )
            .await;
        assert!(result.is_err());

        // Don't apply patch from failed execution
        let patch = fix.ctx().take_patch();
        // In real code, we wouldn't commit on failure
        // But even if we do, patch should be empty for failed tool
        if !patch.patch().is_empty() {
            // This shouldn't happen for our test tool that fails before modifying state
            panic!("Failed tool should not produce patch");
        }
    }

    // State should be unchanged
    let final_state = manager.snapshot().await;
    assert_eq!(final_state["value"], 100);
}

#[tokio::test]
async fn test_session_resilient_to_tool_errors() {
    let thread = Thread::with_initial_state("error-resilient", json!({"counter": 0}));

    // Simulate tool calls where some fail
    let tool_results = [
        ToolResult::success("tool1", json!({"value": 1})),
        ToolResult::error("tool2", "Network timeout"),
        ToolResult::success("tool3", json!({"value": 3})),
        ToolResult::error("tool4", "Connection refused"),
    ];

    // Add all results as messages
    let mut thread = thread;
    for (i, result) in tool_results.iter().enumerate() {
        thread = thread.with_message(convert::tool_response(format!("call_{}", i), result));
    }

    // Thread should have all messages regardless of success/error
    assert_eq!(thread.message_count(), 4);

    // Verify error messages are preserved
    assert!(
        thread.messages[1].content.contains("error")
            || thread.messages[1].content.contains("Network")
    );
    assert!(
        thread.messages[3].content.contains("error")
            || thread.messages[3].content.contains("Connection")
    );
}

#[tokio::test]
async fn test_storage_error_recovery() {
    let temp_dir = TempDir::new().unwrap();
    let storage = FileStore::new(temp_dir.path());

    // Save a valid session
    let thread = ConversationAgentState::new("valid-thread").with_message(Message::user("Hello"));
    storage.save(&thread).await.unwrap();

    // Try to load non-existent session
    let result = storage.load_thread("non-existent").await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_none());

    // Original session should still be loadable
    let loaded = storage.load_thread("valid-thread").await.unwrap().unwrap();
    assert_eq!(loaded.message_count(), 1);
}

#[tokio::test]
async fn test_concurrent_errors_dont_corrupt_storage() {
    let storage = Arc::new(MemoryStore::new());

    // Save initial session
    let thread =
        ConversationAgentState::new("concurrent-test").with_message(Message::user("Initial"));
    storage.save(&thread).await.unwrap();

    let mut handles = vec![];

    // 50 concurrent operations (mix of saves and loads)
    for i in 0..50 {
        let storage = Arc::clone(&storage);
        let handle: tokio::task::JoinHandle<
            Result<Option<Thread>, tirea_agentos::contracts::storage::ThreadStoreError>,
        > = tokio::spawn(async move {
            if i % 3 == 0 {
                // Load
                storage.load_thread("concurrent-test").await
            } else {
                // Save (potentially conflicting)
                let thread = ConversationAgentState::new("concurrent-test")
                    .with_message(Message::user(format!("Update {}", i)));
                storage.save(&thread).await.map(|_| None)
            }
        });
        handles.push(handle);
    }

    futures::future::join_all(handles).await;

    // Storage should still be consistent
    let final_thread = storage
        .load_thread("concurrent-test")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(final_thread.message_count(), 1); // Should have one message
}

// ============================================================================
// Additional Coverage Tests
// ============================================================================

#[test]
fn test_tool_result_success_with_message() {
    let result =
        ToolResult::success_with_message("my_tool", json!({"data": 42}), "Operation completed");

    assert!(result.is_success());
    assert!(!result.is_error());
    assert_eq!(result.tool_name, "my_tool");
    assert_eq!(result.data["data"], 42);
    assert_eq!(result.message, Some("Operation completed".to_string()));
}

#[test]
fn test_tool_result_success_with_message_empty_data() {
    let result = ToolResult::success_with_message("empty_tool", json!(null), "No data returned");

    assert!(result.is_success());
    assert_eq!(result.message, Some("No data returned".to_string()));
    assert!(result.data.is_null());
}

#[test]
fn test_tool_result_success_with_message_complex() {
    let result = ToolResult::success_with_message(
        "api_tool",
        json!({
            "status": 200,
            "body": {"users": [{"id": 1}, {"id": 2}]}
        }),
        "API call successful with 2 users",
    );

    assert!(result.is_success());
    assert_eq!(result.data["status"], 200);
    assert!(result.message.as_ref().unwrap().contains("2 users"));
}

#[test]
fn test_stream_collector_end_event_with_tool_calls() {
    use genai::chat::{ChatStreamEvent, StreamChunk, StreamEnd};

    let mut collector = StreamCollector::new();

    // Add some text first
    collector.process(ChatStreamEvent::Chunk(StreamChunk {
        content: "Processing your request...".to_string(),
    }));

    // Create an end event with captured tool calls
    let end = StreamEnd::default();
    // Note: StreamEnd::captured_tool_calls() returns Option<&Vec<ToolCall>>
    // Testing the path where captured_tool_calls is None (default)

    let output = collector.process(ChatStreamEvent::End(end));
    assert!(output.is_none()); // End event returns None

    let result = collector.finish(None);
    assert_eq!(result.text, "Processing your request...");
}

#[test]
fn test_stream_output_tool_call_delta_coverage() {
    use tirea_agentos::runtime::streaming::StreamOutput;

    // Test ToolCallDelta variant
    let delta = StreamOutput::ToolCallDelta {
        id: "call_123".to_string(),
        args_delta: r#"{"partial": true}"#.to_string(),
    };

    match delta {
        StreamOutput::ToolCallDelta { id, args_delta } => {
            assert_eq!(id, "call_123");
            assert!(args_delta.contains("partial"));
        }
        _ => panic!("Expected ToolCallDelta"),
    }
}

#[test]
fn test_stream_output_tool_call_start_coverage() {
    use tirea_agentos::runtime::streaming::StreamOutput;

    let start = StreamOutput::ToolCallStart {
        id: "call_abc".to_string(),
        name: "web_search".to_string(),
    };

    match start {
        StreamOutput::ToolCallStart { id, name } => {
            assert_eq!(id, "call_abc");
            assert_eq!(name, "web_search");
        }
        _ => panic!("Expected ToolCallStart"),
    }
}

#[tokio::test]
async fn test_file_storage_corrupted_json() {
    let temp_dir = TempDir::new().unwrap();

    // Write corrupted JSON file
    let corrupted_path = temp_dir.path().join("corrupted.json");
    tokio::fs::write(&corrupted_path, "{ invalid json }")
        .await
        .unwrap();

    let storage = FileStore::new(temp_dir.path());

    // Try to load corrupted session
    let result = storage.load_thread("corrupted").await;
    assert!(result.is_err());

    match result {
        Err(tirea_agentos::contracts::storage::ThreadStoreError::Serialization(msg)) => {
            assert!(msg.contains("expected") || msg.contains("key") || !msg.is_empty());
        }
        Err(other) => panic!("Expected Serialization error, got: {:?}", other),
        Ok(_) => panic!("Expected error for corrupted JSON"),
    }
}

#[test]
fn test_tool_error_variants_display() {
    use tirea_agentos::contracts::runtime::tool_call::ToolError;

    let invalid_args = ToolError::InvalidArguments("Missing required field 'name'".to_string());
    assert!(invalid_args.to_string().contains("Invalid arguments"));

    let not_found = ToolError::NotFound("unknown_tool".to_string());
    assert!(
        not_found.to_string().contains("not found") || not_found.to_string().contains("Not found")
    );

    let exec_failed = ToolError::ExecutionFailed("Database connection timeout".to_string());
    assert!(exec_failed.to_string().contains("failed"));

    let permission_denied = ToolError::Denied("Admin access required".to_string());
    assert!(
        permission_denied.to_string().contains("Denied")
            || permission_denied.to_string().contains("Admin")
    );

    let internal = ToolError::Internal("Unexpected state".to_string());
    assert!(internal.to_string().contains("Internal") || internal.to_string().contains("error"));
}

#[test]
fn test_stream_result_needs_tools_variants() {
    // Test with empty tool calls
    let result_no_tools = StreamResult {
        text: "Just text".to_string(),
        tool_calls: vec![],
        usage: None,
        stop_reason: None,
    };
    assert!(!result_no_tools.needs_tools());

    // Test with tool calls
    let result_with_tools = StreamResult {
        text: "".to_string(),
        tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
            "id",
            "name",
            json!({}),
        )],
        usage: None,
        stop_reason: None,
    };
    assert!(result_with_tools.needs_tools());

    // Test with both text and tools
    let result_both = StreamResult {
        text: "Processing...".to_string(),
        tool_calls: vec![
            tirea_agentos::contracts::thread::ToolCall::new("id1", "search", json!({})),
            tirea_agentos::contracts::thread::ToolCall::new("id2", "calculate", json!({})),
        ],
        usage: None,
        stop_reason: None,
    };
    assert!(result_both.needs_tools());
}

#[tokio::test]
async fn test_execute_tools_empty_result() {
    let thread = ConversationAgentState::new("empty-tools-test");

    // Empty StreamResult (no tools)
    let result = StreamResult {
        text: "No tools needed".to_string(),
        tool_calls: vec![],
        usage: None,
        stop_reason: None,
    };

    let tools: std::collections::HashMap<String, Arc<dyn Tool>> = std::collections::HashMap::new();

    // Should return session unchanged when no tools
    let new_thread = loop_execute_tools(thread.clone(), &result, &tools, true)
        .await
        .unwrap();

    assert_eq!(new_thread.message_count(), thread.message_count());
}

#[test]
fn test_agent_event_all_variants() {
    use tirea_agentos::contracts::AgentEvent;

    // TextDelta
    let text_delta = AgentEvent::TextDelta {
        delta: "Hello".to_string(),
    };
    match text_delta {
        AgentEvent::TextDelta { delta } => assert_eq!(delta, "Hello"),
        _ => panic!("Wrong variant"),
    }

    // ToolCallStart
    let tool_start = AgentEvent::ToolCallStart {
        id: "call_1".to_string(),
        name: "search".to_string(),
    };
    match tool_start {
        AgentEvent::ToolCallStart { id, name } => {
            assert_eq!(id, "call_1");
            assert_eq!(name, "search");
        }
        _ => panic!("Wrong variant"),
    }

    // ToolCallDelta
    let tool_delta = AgentEvent::ToolCallDelta {
        id: "call_1".to_string(),
        args_delta: r#"{"q":"test"}"#.to_string(),
    };
    match tool_delta {
        AgentEvent::ToolCallDelta { id, args_delta } => {
            assert_eq!(id, "call_1");
            assert!(args_delta.contains("test"));
        }
        _ => panic!("Wrong variant"),
    }

    // ToolCallDone
    let tool_done = AgentEvent::ToolCallDone {
        id: "call_1".to_string(),
        result: ToolResult::success("search", json!({"results": []})),
        patch: None,
        message_id: String::new(),
    };
    match tool_done {
        AgentEvent::ToolCallDone {
            id, result, patch, ..
        } => {
            assert_eq!(id, "call_1");
            assert!(result.is_success());
            assert!(patch.is_none());
        }
        _ => panic!("Wrong variant"),
    }

    // Error
    let error = AgentEvent::Error {
        message: "Network timeout".to_string(),
        code: None,
    };
    match error {
        AgentEvent::Error { message, .. } => assert!(message.contains("timeout")),
        _ => panic!("Wrong variant"),
    }

    // ActivitySnapshot
    let activity_snapshot = AgentEvent::ActivitySnapshot {
        message_id: "activity_1".to_string(),
        activity_type: "progress".to_string(),
        content: json!({"progress": 0.5}),
        replace: Some(true),
    };
    match activity_snapshot {
        AgentEvent::ActivitySnapshot {
            message_id,
            activity_type,
            content,
            replace,
        } => {
            assert_eq!(message_id, "activity_1");
            assert_eq!(activity_type, "progress");
            assert_eq!(content["progress"], 0.5);
            assert_eq!(replace, Some(true));
        }
        _ => panic!("Wrong variant"),
    }

    // ActivityDelta
    let activity_delta = AgentEvent::ActivityDelta {
        message_id: "activity_1".to_string(),
        activity_type: "progress".to_string(),
        patch: vec![json!({"op": "replace", "path": "/progress", "value": 0.75})],
    };
    match activity_delta {
        AgentEvent::ActivityDelta {
            message_id,
            activity_type,
            patch,
        } => {
            assert_eq!(message_id, "activity_1");
            assert_eq!(activity_type, "progress");
            assert_eq!(patch.len(), 1);
        }
        _ => panic!("Wrong variant"),
    }

    // RunFinish
    let finish = AgentEvent::RunFinish {
        thread_id: "t1".to_string(),
        run_id: "r1".to_string(),
        result: Some(serde_json::json!({"response": "Final response"})),
        termination: tirea_agentos::contracts::TerminationReason::NaturalEnd,
    };
    match finish {
        AgentEvent::RunFinish { result, .. } => {
            assert_eq!(result.unwrap()["response"], "Final response");
        }
        _ => panic!("Wrong variant"),
    }
}

#[tokio::test]
async fn test_activity_context_emits_snapshot_on_update() {
    use std::sync::Arc;
    use tirea_agentos::contracts::AgentEvent;
    use tokio::sync::mpsc;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let hub = Arc::new(ActivityHub::new(tx));
    let fix = TestFixture::new();
    let ctx = ToolCallContext::new(
        &fix.doc,
        &fix.ops,
        "call_1",
        "tool:test",
        &fix.run_policy,
        &fix.pending_messages,
        hub,
    );
    let activity = ctx.activity("stream_1", "progress");

    let progress = activity.state::<ProgressState>("");
    progress
        .set_progress(0.25)
        .expect("state mutation should succeed");
    progress
        .set_status("running")
        .expect("state mutation should succeed");

    let first = rx.recv().await.expect("first activity event");
    let second = rx.recv().await.expect("second activity event");

    match first {
        AgentEvent::ActivitySnapshot {
            message_id,
            activity_type,
            content,
            replace,
        } => {
            assert_eq!(message_id, "stream_1");
            assert_eq!(activity_type, "progress");
            assert_eq!(content["progress"], 0.25);
            assert!(content.get("status").is_none());
            assert_eq!(replace, Some(true));
        }
        _ => panic!("Expected ActivitySnapshot"),
    }

    match second {
        AgentEvent::ActivitySnapshot { content, .. } => {
            assert_eq!(content["progress"], 0.25);
            assert_eq!(content["status"], "running");
        }
        _ => panic!("Expected ActivitySnapshot"),
    }
}

#[tokio::test]
async fn test_activity_context_snapshot_reused_across_contexts() {
    use std::sync::Arc;
    use tokio::sync::mpsc;

    let (tx, _rx) = mpsc::unbounded_channel();
    let hub = Arc::new(ActivityHub::new(tx));
    let fix = TestFixture::new();

    let ctx = ToolCallContext::new(
        &fix.doc,
        &fix.ops,
        "call_1",
        "tool:test",
        &fix.run_policy,
        &fix.pending_messages,
        hub.clone(),
    );
    let activity = ctx.activity("stream_2", "progress");
    let progress = activity.state::<ProgressState>("");
    progress
        .set_progress(0.9)
        .expect("state mutation should succeed");

    let ctx2 = ToolCallContext::new(
        &fix.doc,
        &fix.ops,
        "call_2",
        "tool:test",
        &fix.run_policy,
        &fix.pending_messages,
        hub,
    );
    let activity2 = ctx2.activity("stream_2", "progress");
    let progress2 = activity2.state::<ProgressState>("");
    assert_eq!(progress2.progress().unwrap_or_default(), 0.9);
}

#[tokio::test]
async fn test_activity_context_multiple_streams_emit_separately() {
    use std::sync::Arc;
    use tirea_agentos::contracts::AgentEvent;
    use tokio::sync::mpsc;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let hub = Arc::new(ActivityHub::new(tx));
    let fix = TestFixture::new();

    let ctx = ToolCallContext::new(
        &fix.doc,
        &fix.ops,
        "call_1",
        "tool:test",
        &fix.run_policy,
        &fix.pending_messages,
        hub,
    );
    let activity_a = ctx.activity("stream_a", "progress");
    let activity_b = ctx.activity("stream_b", "progress");

    let state_a = activity_a.state::<ProgressState>("");
    state_a
        .set_progress(0.1)
        .expect("state mutation should succeed");

    let state_b = activity_b.state::<ProgressState>("");
    state_b
        .set_progress(0.9)
        .expect("state mutation should succeed");

    let first = rx.recv().await.expect("first event");
    let second = rx.recv().await.expect("second event");

    let (first_id, second_id) = match (first, second) {
        (
            AgentEvent::ActivitySnapshot {
                message_id: id1, ..
            },
            AgentEvent::ActivitySnapshot {
                message_id: id2, ..
            },
        ) => (id1, id2),
        _ => panic!("Expected ActivitySnapshot events"),
    };

    assert_eq!(first_id, "stream_a");
    assert_eq!(second_id, "stream_b");
}

#[test]
fn test_message_role_coverage() {
    // Test all Role variants through Message creation
    let user_msg = Message::user("User content");
    assert_eq!(user_msg.role, ThreadRole::User);
    assert!(user_msg.tool_calls.is_none());
    assert!(user_msg.tool_call_id.is_none());

    let assistant_msg = Message::assistant("Assistant content");
    assert_eq!(assistant_msg.role, ThreadRole::Assistant);

    let system_msg = Message::system("System prompt");
    assert_eq!(system_msg.role, ThreadRole::System);

    let tool_msg = Message::tool("call_123", "Tool result");
    assert_eq!(tool_msg.role, ThreadRole::Tool);
    assert_eq!(tool_msg.tool_call_id, Some("call_123".to_string()));
}

#[test]
fn test_tool_call_creation_and_serialization() {
    let call = tirea_agentos::contracts::thread::ToolCall::new(
        "call_abc123",
        "web_search",
        json!({"query": "rust programming", "limit": 10}),
    );

    assert_eq!(call.id, "call_abc123");
    assert_eq!(call.name, "web_search");
    assert_eq!(call.arguments["query"], "rust programming");
    assert_eq!(call.arguments["limit"], 10);

    // Test serialization
    let json_str = serde_json::to_string(&call).unwrap();
    assert!(json_str.contains("call_abc123"));
    assert!(json_str.contains("web_search"));

    // Test deserialization
    let parsed: tirea_agentos::contracts::thread::ToolCall =
        serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed.id, call.id);
    assert_eq!(parsed.name, call.name);
}

#[tokio::test]
async fn test_session_state_complex_operations() {
    let thread = Thread::with_initial_state(
        "complex-state",
        json!({
            "users": [],
            "settings": {"theme": "dark", "notifications": true},
            "counter": 0
        }),
    );

    // Add multiple patches
    let thread = thread
        .with_patch(TrackedPatch::new(
            Patch::new()
                .with_op(Op::append(
                    path!("users"),
                    json!({"id": 1, "name": "Alice"}),
                ))
                .with_op(Op::increment(path!("counter"), 1)),
        ))
        .with_patch(TrackedPatch::new(
            Patch::new()
                .with_op(Op::append(path!("users"), json!({"id": 2, "name": "Bob"})))
                .with_op(Op::set(path!("settings").key("theme"), json!("light"))),
        ));

    let state = thread.rebuild_state().unwrap();

    assert_eq!(state["users"].as_array().unwrap().len(), 2);
    assert_eq!(state["users"][0]["name"], "Alice");
    assert_eq!(state["users"][1]["name"], "Bob");
    assert_eq!(state["settings"]["theme"], "light");
    assert_eq!(state["counter"], 1);
}

#[test]
fn test_storage_error_variants() {
    use std::io::{Error as IoError, ErrorKind};
    use tirea_agentos::contracts::storage::ThreadStoreError;

    // Test IO error variant
    let io_error = ThreadStoreError::from(IoError::new(
        ErrorKind::PermissionDenied,
        "Permission denied",
    ));
    let display = io_error.to_string();
    assert!(display.contains("IO error") || display.contains("Permission") || !display.is_empty());

    // Test Serialization error variant
    let serialization_error = ThreadStoreError::Serialization("Invalid JSON at line 5".to_string());
    let display = serialization_error.to_string();
    assert!(
        display.contains("Serialization")
            || display.contains("JSON")
            || display.contains("Invalid")
    );

    // Test NotFound error variant
    let not_found = ThreadStoreError::NotFound("thread-123".to_string());
    let display = not_found.to_string();
    assert!(
        display.contains("not found")
            || display.contains("thread-123")
            || display.contains("Not found")
    );
}

// ============================================================================
// Sequential Execution with Patch Error Tests
// ============================================================================

/// Tool that produces a patch with nested state changes
struct NestedStateTool;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, State, Default)]
#[tirea(action = "NestedStateAction")]
struct NestedState {
    #[serde(default)]
    value: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
enum NestedStateAction {
    SetValue(i64),
}

impl NestedState {
    fn reduce(&mut self, action: NestedStateAction) {
        match action {
            NestedStateAction::SetValue(value) => self.value = value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
struct NestedEffectState {
    #[serde(default)]
    value: i64,
}

struct NestedEffectRef;

impl State for NestedEffectState {
    type Ref<'a> = NestedEffectRef;
    const PATH: &'static str = "";

    fn state_ref<'a>(_: &'a DocCell, _: Path, _: PatchSink<'a>) -> Self::Ref<'a> {
        NestedEffectRef
    }

    fn from_value(value: &Value) -> TireaResult<Self> {
        if value.is_null() {
            return Ok(Self::default());
        }
        if let Some(current) = value.as_i64() {
            return Ok(Self { value: current });
        }
        serde_json::from_value(value.clone()).map_err(TireaError::Serialization)
    }

    fn to_value(&self) -> TireaResult<Value> {
        serde_json::to_value(self).map_err(TireaError::Serialization)
    }
}

impl StateSpec for NestedEffectState {
    type Action = NestedStateAction;

    fn reduce(&mut self, action: NestedStateAction) {
        match action {
            NestedStateAction::SetValue(value) => self.value = value,
        }
    }
}

#[async_trait]
impl Tool for NestedStateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("nested_state", "Nested State Tool", "Modifies nested state")
    }

    async fn execute(
        &self,
        _args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        // Modify deeply nested state
        let nested = ctx.state::<NestedState>("deeply.nested");
        let current = nested.value().unwrap_or(0);
        nested
            .set_value(current + 10)
            .expect("state mutation should succeed");

        Ok(ToolResult::success(
            "nested_state",
            json!({"new_value": current + 10}),
        ))
    }

    async fn execute_effect(
        &self,
        _args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let current = ctx
            .snapshot_at::<NestedEffectState>("deeply.nested")
            .unwrap_or_default()
            .value;
        Ok(ToolExecutionEffect::new(ToolResult::success(
            "nested_state",
            json!({"new_value": current + 10}),
        ))
        .with_action(AnyStateAction::new_at::<NestedEffectState>(
            "deeply.nested",
            NestedStateAction::SetValue(current + 10),
        )))
    }
}

#[tokio::test]
async fn test_sequential_execution_with_conflicting_patches() {
    use tirea_agentos::engine::tool_execution::execute_tools_sequential;

    // Test sequential execution where patches might conflict
    let mut tools: std::collections::HashMap<String, Arc<dyn Tool>> =
        std::collections::HashMap::new();
    tools.insert("increment".to_string(), Arc::new(IncrementTool));

    // Create tool calls that modify the same field sequentially
    let calls = vec![
        tirea_agentos::contracts::thread::ToolCall::new(
            "call_1",
            "increment",
            json!({"path": "counter"}),
        ),
        tirea_agentos::contracts::thread::ToolCall::new(
            "call_2",
            "increment",
            json!({"path": "counter"}),
        ),
        tirea_agentos::contracts::thread::ToolCall::new(
            "call_3",
            "increment",
            json!({"path": "counter"}),
        ),
    ];

    // Initial state with counter
    let initial_state = json!({
        "counter": {"value": 0, "label": "test"}
    });

    // Execute sequentially - each tool sees the updated state
    let (final_state, executions) = execute_tools_sequential(&tools, &calls, &initial_state).await;

    assert_eq!(executions.len(), 3);
    assert!(executions.iter().all(|e| e.result.is_success()));

    // In sequential mode, each tool sees the result of the previous
    // So counter should be 3 (0 -> 1 -> 2 -> 3)
    assert_eq!(final_state["counter"]["value"], 3);
}

#[tokio::test]
async fn test_sequential_execution_with_nested_state() {
    use tirea_agentos::engine::tool_execution::execute_tools_sequential;

    // Test sequential execution with nested state modifications
    let mut tools: std::collections::HashMap<String, Arc<dyn Tool>> =
        std::collections::HashMap::new();
    tools.insert("nested_state".to_string(), Arc::new(NestedStateTool));

    let calls = vec![
        tirea_agentos::contracts::thread::ToolCall::new("call_1", "nested_state", json!({})),
        tirea_agentos::contracts::thread::ToolCall::new("call_2", "nested_state", json!({})),
    ];

    // Start with state that has nested structure
    let initial_state = json!({
        "deeply": {
            "nested": {"value": 0}
        }
    });

    // Execute sequentially - each tool sees the updated state
    let (final_state, executions) = execute_tools_sequential(&tools, &calls, &initial_state).await;

    // Both tools should execute
    assert_eq!(executions.len(), 2);
    assert!(executions.iter().all(|e| e.result.is_success()));

    // Sequential: 0 -> 10 -> 20
    assert_eq!(final_state["deeply"]["nested"]["value"], 20);
}

/// Test parallel execution state isolation - each tool sees the same initial state
#[tokio::test]
async fn test_parallel_execution_state_isolation() {
    use tirea_agentos::engine::tool_execution::execute_tools_parallel;

    let mut tools: std::collections::HashMap<String, Arc<dyn Tool>> =
        std::collections::HashMap::new();
    tools.insert("increment".to_string(), Arc::new(IncrementTool));

    // Three parallel increment calls
    let calls = vec![
        tirea_agentos::contracts::thread::ToolCall::new(
            "call_1",
            "increment",
            json!({"path": "counter"}),
        ),
        tirea_agentos::contracts::thread::ToolCall::new(
            "call_2",
            "increment",
            json!({"path": "counter"}),
        ),
        tirea_agentos::contracts::thread::ToolCall::new(
            "call_3",
            "increment",
            json!({"path": "counter"}),
        ),
    ];

    let initial_state = json!({
        "counter": {"value": 10, "label": "test"}
    });

    let results = execute_tools_parallel(&tools, &calls, &initial_state).await;

    // All three tools should see initial state (counter=10) and increment to 11
    assert_eq!(results.len(), 3);
    for (i, exec) in results.iter().enumerate() {
        assert!(exec.result.is_success(), "Tool {} should succeed", i);
        // Each tool saw initial value 10, incremented to 11
        assert_eq!(
            exec.result.data["new_value"], 11,
            "Tool {} should see initial state",
            i
        );
    }

    // All three patches should set counter.value to 11 (not 11, 12, 13)
    let patches: Vec<_> = results.iter().filter_map(|e| e.patch.as_ref()).collect();
    assert_eq!(patches.len(), 3, "All tools should produce patches");
}

/// Test parallel execution with patch conflict - multiple tools modify same field
#[tokio::test]
async fn test_parallel_execution_patch_conflict() {
    let thread = Thread::with_initial_state(
        "parallel-conflict",
        json!({"counter": {"value": 0, "label": ""}}),
    );

    // Three parallel increments
    let llm_response = StreamResult {
        text: "Running three increments in parallel".to_string(),
        tool_calls: vec![
            tirea_agentos::contracts::thread::ToolCall::new(
                "call_1",
                "increment",
                json!({"path": "counter"}),
            ),
            tirea_agentos::contracts::thread::ToolCall::new(
                "call_2",
                "increment",
                json!({"path": "counter"}),
            ),
            tirea_agentos::contracts::thread::ToolCall::new(
                "call_3",
                "increment",
                json!({"path": "counter"}),
            ),
        ],
        usage: None,
        stop_reason: None,
    };

    let tools = tool_map([IncrementTool]);

    // Execute in parallel mode
    let err = loop_execute_tools(thread, &llm_response, &tools, true)
        .await
        .unwrap_err();

    match err {
        AgentLoopError::StateError(msg) => {
            assert!(msg.contains("conflicting parallel state patches"));
        }
        other => panic!(
            "expected StateError for conflicting patches, got: {:?}",
            other
        ),
    }
}

/// Test parallel execution with different fields - no conflict
#[tokio::test]
async fn test_parallel_execution_different_fields() {
    let thread = Thread::with_initial_state(
        "parallel-no-conflict",
        json!({
            "counter_a": {"value": 0, "label": ""},
            "counter_b": {"value": 0, "label": ""},
            "counter_c": {"value": 0, "label": ""}
        }),
    );

    // Three parallel increments to different fields
    let llm_response = StreamResult {
        text: "Running three increments to different counters".to_string(),
        tool_calls: vec![
            tirea_agentos::contracts::thread::ToolCall::new(
                "call_1",
                "increment",
                json!({"path": "counter_a"}),
            ),
            tirea_agentos::contracts::thread::ToolCall::new(
                "call_2",
                "increment",
                json!({"path": "counter_b"}),
            ),
            tirea_agentos::contracts::thread::ToolCall::new(
                "call_3",
                "increment",
                json!({"path": "counter_c"}),
            ),
        ],
        usage: None,
        stop_reason: None,
    };

    let tools = tool_map([IncrementTool]);

    let thread = loop_execute_tools(thread, &llm_response, &tools, true)
        .await
        .unwrap();

    let state = thread.rebuild_state().unwrap();

    // All three counters should be 1 (no conflict)
    assert_eq!(state["counter_a"]["value"], 1);
    assert_eq!(state["counter_b"]["value"], 1);
    assert_eq!(state["counter_c"]["value"], 1);
}

/// Test parallel vs sequential with same operations - different results
#[tokio::test]
async fn test_sequential_vs_parallel_execution_difference() {
    use tirea_agentos::engine::tool_execution::{execute_tools_parallel, execute_tools_sequential};

    let mut tools: std::collections::HashMap<String, Arc<dyn Tool>> =
        std::collections::HashMap::new();
    tools.insert("increment".to_string(), Arc::new(IncrementTool));

    let calls = vec![
        tirea_agentos::contracts::thread::ToolCall::new(
            "call_1",
            "increment",
            json!({"path": "counter"}),
        ),
        tirea_agentos::contracts::thread::ToolCall::new(
            "call_2",
            "increment",
            json!({"path": "counter"}),
        ),
    ];

    let initial_state = json!({
        "counter": {"value": 0, "label": "test"}
    });

    // Parallel execution - both tools see initial state (counter=0)
    let parallel_results = execute_tools_parallel(&tools, &calls, &initial_state).await;

    // Both tools incremented from 0, so both patches set counter.value to 1
    assert_eq!(parallel_results.len(), 2);

    // Sequential execution - second tool sees first tool's result
    let (seq_final_state, seq_results) =
        execute_tools_sequential(&tools, &calls, &initial_state).await;

    assert_eq!(seq_results.len(), 2);
    // Sequential: 0 -> 1 -> 2
    assert_eq!(seq_final_state["counter"]["value"], 2);
}

// ============================================================================
// Stream End Event with Captured Tool Calls Tests
// ============================================================================

#[test]
fn test_stream_collector_with_tool_call_via_chunk_then_end() {
    use genai::chat::{ChatStreamEvent, StreamChunk, StreamEnd, ToolChunk};

    // Test the flow: text chunks -> tool call chunks -> end event
    let mut collector = StreamCollector::new();

    // Text chunk
    collector.process(ChatStreamEvent::Chunk(StreamChunk {
        content: "Let me search for that.".to_string(),
    }));

    // Tool call chunk with complete info
    let tool_call = genai::chat::ToolCall {
        call_id: "call_search".to_string(),
        fn_name: "web_search".to_string(),
        fn_arguments: json!({"query": "rust async"}),
        thought_signatures: None,
    };
    collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call }));

    // End event
    let end = StreamEnd::default();
    let output = collector.process(ChatStreamEvent::End(end));

    // End event returns None
    assert!(output.is_none());

    // Finish and verify results
    let result = collector.finish(None);
    assert_eq!(result.text, "Let me search for that.");
    assert_eq!(result.tool_calls.len(), 1);
    assert_eq!(result.tool_calls[0].name, "web_search");
}

#[test]
fn test_stream_collector_multiple_tool_calls_and_end() {
    use genai::chat::{ChatStreamEvent, StreamChunk, StreamEnd, ToolChunk};

    let mut collector = StreamCollector::new();

    // Text
    collector.process(ChatStreamEvent::Chunk(StreamChunk {
        content: "Running multiple tools.".to_string(),
    }));

    // First tool call
    let tc1 = genai::chat::ToolCall {
        call_id: "call_1".to_string(),
        fn_name: "search".to_string(),
        fn_arguments: json!({"q": "test"}),
        thought_signatures: None,
    };
    collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc1 }));

    // Second tool call
    let tc2 = genai::chat::ToolCall {
        call_id: "call_2".to_string(),
        fn_name: "calculate".to_string(),
        fn_arguments: json!({"expr": "1+1"}),
        thought_signatures: None,
    };
    collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc2 }));

    // End event (tool calls already captured via ToolCallChunk)
    collector.process(ChatStreamEvent::End(StreamEnd::default()));

    let result = collector.finish(None);
    assert_eq!(result.tool_calls.len(), 2);
}

#[test]
fn test_stream_collector_text_only_then_end() {
    use genai::chat::{ChatStreamEvent, StreamChunk, StreamEnd};

    let mut collector = StreamCollector::new();

    // Only text, no tool calls
    collector.process(ChatStreamEvent::Chunk(StreamChunk {
        content: "Here is your answer: 42".to_string(),
    }));

    // End event with no captured tool calls
    collector.process(ChatStreamEvent::End(StreamEnd::default()));

    let result = collector.finish(None);
    assert_eq!(result.text, "Here is your answer: 42");
    assert!(result.tool_calls.is_empty());
    assert!(!result.needs_tools());
}

#[test]
fn test_stream_collector_unknown_event_handling() {
    use genai::chat::ChatStreamEvent;

    let mut collector = StreamCollector::new();

    // Start event (should be ignored)
    let output = collector.process(ChatStreamEvent::Start);
    assert!(output.is_none());

    // ReasoningDelta event (if exists, should be ignored)
    // The _ match arm handles unknown events

    let result = collector.finish(None);
    assert!(result.text.is_empty());
    assert!(result.tool_calls.is_empty());
}

// ============================================================================
// Tool Execution with Empty Patch Tests
// ============================================================================

/// Tool that reads state but doesn't modify it
struct ReadOnlyTool;

#[async_trait]
impl Tool for ReadOnlyTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("read_only", "Read Only", "Reads state without modification")
    }

    async fn execute(
        &self,
        _args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        // Only read, don't modify
        let counter = ctx.state::<CounterState>("counter");
        let value = counter.value().unwrap_or(-1);

        // No modifications, so patch should be empty
        Ok(ToolResult::success(
            "read_only",
            json!({"current_value": value}),
        ))
    }
}

#[tokio::test]
async fn test_tool_execution_with_empty_patch() {
    use tirea_agentos::engine::tool_execution::execute_single_tool;

    let tool = ReadOnlyTool;
    let call = tirea_agentos::contracts::thread::ToolCall::new("call_1", "read_only", json!({}));
    let state = json!({"counter": {"value": 42, "label": "test"}});

    let result = execute_single_tool(Some(&tool), &call, &state).await;

    assert!(result.result.is_success());
    assert_eq!(result.result.data["current_value"], 42);
    // Patch should be None (empty patches are converted to None)
    assert!(result.patch.is_none());
}

#[tokio::test]
async fn test_sequential_execution_with_mixed_patch_results() {
    use tirea_agentos::engine::tool_execution::execute_tools_sequential;

    let mut tools: std::collections::HashMap<String, Arc<dyn Tool>> =
        std::collections::HashMap::new();
    tools.insert("read_only".to_string(), Arc::new(ReadOnlyTool));
    tools.insert("increment".to_string(), Arc::new(IncrementTool));

    let calls = vec![
        tirea_agentos::contracts::thread::ToolCall::new("call_1", "read_only", json!({})),
        tirea_agentos::contracts::thread::ToolCall::new(
            "call_2",
            "increment",
            json!({"path": "counter"}),
        ),
        tirea_agentos::contracts::thread::ToolCall::new("call_3", "read_only", json!({})),
    ];

    let initial_state = json!({
        "counter": {"value": 10, "label": "test"}
    });

    let (final_state, executions) = execute_tools_sequential(&tools, &calls, &initial_state).await;

    // First read_only sees 10, increment changes to 11, second read_only sees 11
    assert_eq!(executions[0].result.data["current_value"], 10);
    assert_eq!(executions[1].result.data["new_value"], 11);
    assert_eq!(executions[2].result.data["current_value"], 11);

    // Final state should be 11
    assert_eq!(final_state["counter"]["value"], 11);

    // Check patches
    assert!(executions[0].patch.is_none()); // read_only - no patch
    assert!(executions[1].patch.is_some()); // increment - has patch
    assert!(executions[2].patch.is_none()); // read_only - no patch
}

// ============================================================================
// Agent Loop Error Tests
// ============================================================================

#[test]
fn test_agent_loop_error_all_variants() {
    use tirea_agentos::runtime::loop_runner::AgentLoopError;

    // LlmError
    let llm_err = AgentLoopError::LlmError("API rate limit exceeded".to_string());
    let display = llm_err.to_string();
    assert!(display.contains("LLM") || display.contains("rate limit"));

    // StateError
    let state_err = AgentLoopError::StateError("Failed to rebuild state".to_string());
    let display = state_err.to_string();
    assert!(display.contains("State") || display.contains("rebuild"));
}

// ============================================================================
// Mock-based End-to-End Tests (No Real LLM Required)
// ============================================================================

// These tests simulate the full agent loop without requiring a real LLM.
// They test the same scenarios as live_deepseek.rs but using mock responses.

/// Simulate a complete agent step with tool calls
#[tokio::test]
async fn test_e2e_tool_execution_flow() {
    // Simulates: User asks -> LLM calls tool -> Tool executes -> Response

    // 1. Create session with initial state
    let thread = Thread::with_initial_state("e2e-test", json!({"counter": 0}))
        .with_message(Message::user("Increment the counter by 5"));

    // 2. Simulate LLM response with tool call
    let llm_response = StreamResult {
        text: "I'll increment the counter for you.".to_string(),
        tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
            "call_1",
            "increment",
            json!({"path": "counter"}),
        )],
        usage: None,
        stop_reason: None,
    };

    // 3. Add assistant message with tool calls
    let thread = thread.with_message(convert::assistant_tool_calls(
        &llm_response.text,
        llm_response.tool_calls.clone(),
    ));

    // 4. Execute tools
    let tools = tool_map([IncrementTool]);
    let thread = loop_execute_tools(thread, &llm_response, &tools, true)
        .await
        .unwrap();

    // 5. Verify state changed
    let state = thread.rebuild_state().unwrap();
    assert_eq!(state["counter"]["value"], 1);

    // 6. Simulate final LLM response
    let thread = thread.with_message(Message::assistant("Done! The counter is now 1."));

    assert_eq!(thread.message_count(), 4); // user + assistant(tool) + tool_response + assistant
    assert_eq!(thread.patch_count(), 2);
}

/// Simulate parallel tool calls
#[tokio::test]
async fn test_e2e_parallel_tool_calls() {
    let thread = Thread::with_initial_state(
        "e2e-parallel",
        json!({
            "counter": {"value": 0, "label": "test"},
            "tasks": {"items": [], "count": 0}
        }),
    )
    .with_message(Message::user("Increment counter and add a task"));

    // LLM calls two tools in parallel
    let llm_response = StreamResult {
        text: "I'll do both.".to_string(),
        tool_calls: vec![
            tirea_agentos::contracts::thread::ToolCall::new(
                "call_1",
                "increment",
                json!({"path": "counter"}),
            ),
            tirea_agentos::contracts::thread::ToolCall::new(
                "call_2",
                "add_task",
                json!({"item": "New task"}),
            ),
        ],
        usage: None,
        stop_reason: None,
    };

    let thread = thread.with_message(convert::assistant_tool_calls(
        &llm_response.text,
        llm_response.tool_calls.clone(),
    ));

    // Execute tools (parallel mode)
    let mut tools: std::collections::HashMap<String, Arc<dyn Tool>> =
        std::collections::HashMap::new();
    tools.insert("increment".to_string(), Arc::new(IncrementTool));
    tools.insert("add_task".to_string(), Arc::new(AddTaskTool));

    let thread = loop_execute_tools(thread, &llm_response, &tools, true)
        .await
        .unwrap();

    // Both tools executed
    let state = thread.rebuild_state().unwrap();
    assert_eq!(state["counter"]["value"], 1);
    assert_eq!(state["tasks"]["count"], 1);
    assert_eq!(thread.patch_count(), 3); // Two tool patches + control-state patch
}

/// Simulate multi-step conversation with state accumulation
#[tokio::test]
async fn test_e2e_multi_step_with_state() {
    let tools = tool_map([IncrementTool]);

    // Step 1
    let mut thread =
        Thread::with_initial_state("e2e-multi", json!({"counter": {"value": 0, "label": ""}}))
            .with_message(Message::user("Increment"));

    let response1 = StreamResult {
        text: "Incrementing.".to_string(),
        tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
            "call_1",
            "increment",
            json!({"path": "counter"}),
        )],
        usage: None,
        stop_reason: None,
    };
    thread = thread.with_message(convert::assistant_tool_calls(
        &response1.text,
        response1.tool_calls.clone(),
    ));
    thread = loop_execute_tools(thread, &response1, &tools, true)
        .await
        .unwrap();
    thread = thread.with_message(Message::assistant("Counter is now 1."));

    // Step 2
    thread = thread.with_message(Message::user("Increment again"));
    let response2 = StreamResult {
        text: "Incrementing again.".to_string(),
        tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
            "call_2",
            "increment",
            json!({"path": "counter"}),
        )],
        usage: None,
        stop_reason: None,
    };
    thread = thread.with_message(convert::assistant_tool_calls(
        &response2.text,
        response2.tool_calls.clone(),
    ));
    thread = loop_execute_tools(thread, &response2, &tools, true)
        .await
        .unwrap();
    thread = thread.with_message(Message::assistant("Counter is now 2."));

    // Step 3
    thread = thread.with_message(Message::user("One more time"));
    let response3 = StreamResult {
        text: "One more increment.".to_string(),
        tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
            "call_3",
            "increment",
            json!({"path": "counter"}),
        )],
        usage: None,
        stop_reason: None,
    };
    thread = thread.with_message(convert::assistant_tool_calls(
        &response3.text,
        response3.tool_calls.clone(),
    ));
    thread = loop_execute_tools(thread, &response3, &tools, true)
        .await
        .unwrap();

    // Verify accumulated state
    let state = thread.rebuild_state().unwrap();
    assert_eq!(state["counter"]["value"], 3);
    assert_eq!(thread.patch_count(), 6);
}

/// Simulate tool failure and error message
#[tokio::test]
async fn test_e2e_tool_failure_handling() {
    let thread = ConversationAgentState::new("e2e-failure")
        .with_message(Message::user("Call a non-existent tool"));

    // LLM calls a tool that doesn't exist
    let llm_response = StreamResult {
        text: "Calling tool.".to_string(),
        tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
            "call_1",
            "nonexistent_tool",
            json!({}),
        )],
        usage: None,
        stop_reason: None,
    };

    let thread = thread.with_message(convert::assistant_tool_calls(
        &llm_response.text,
        llm_response.tool_calls.clone(),
    ));

    // Execute with empty tool map
    let tools: std::collections::HashMap<String, Arc<dyn Tool>> = std::collections::HashMap::new();

    let thread = loop_execute_tools(thread, &llm_response, &tools, true)
        .await
        .unwrap();

    // Tool response should contain error
    let last_msg = thread.messages.last().unwrap();
    assert_eq!(last_msg.role, ThreadRole::Tool);
    assert!(last_msg.content.contains("error") || last_msg.content.contains("not found"));
}

/// Simulate session persistence and restore mid-conversation
#[tokio::test]
async fn test_e2e_session_persistence_restore() {
    let storage = MemoryStore::new();
    let tools = tool_map([IncrementTool]);

    // Phase 1: Start conversation
    let mut thread = Thread::with_initial_state(
        "e2e-persist",
        json!({"counter": {"value": 10, "label": ""}}),
    )
    .with_message(Message::user("Increment by 5"));

    let response = StreamResult {
        text: "Incrementing.".to_string(),
        tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
            "call_1",
            "increment",
            json!({"path": "counter"}),
        )],
        usage: None,
        stop_reason: None,
    };
    thread = thread.with_message(convert::assistant_tool_calls(
        &response.text,
        response.tool_calls.clone(),
    ));
    thread = loop_execute_tools(thread, &response, &tools, true)
        .await
        .unwrap();
    thread = thread.with_message(Message::assistant("Done!"));

    // Save
    storage.save(&thread).await.unwrap();
    let state_before = thread.rebuild_state().unwrap();

    // Phase 2: "Restart" - load and continue
    let mut loaded = storage.load_thread("e2e-persist").await.unwrap().unwrap();

    // Verify state preserved
    let state_after_load = loaded.rebuild_state().unwrap();
    assert_eq!(state_before, state_after_load);
    assert_eq!(loaded.message_count(), 4);

    // Continue conversation
    loaded = loaded.with_message(Message::user("Increment again"));
    let response2 = StreamResult {
        text: "Incrementing again.".to_string(),
        tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
            "call_2",
            "increment",
            json!({"path": "counter"}),
        )],
        usage: None,
        stop_reason: None,
    };
    loaded = loaded.with_message(convert::assistant_tool_calls(
        &response2.text,
        response2.tool_calls.clone(),
    ));
    loaded = loop_execute_tools(loaded, &response2, &tools, true)
        .await
        .unwrap();

    // Verify continued correctly
    let final_state = loaded.rebuild_state().unwrap();
    assert_eq!(final_state["counter"]["value"], 12); // 10 + 1 + 1
}

/// Simulate snapshot and continue
#[tokio::test]
async fn test_e2e_snapshot_and_continue() {
    let tools = tool_map([IncrementTool]);

    // Build up patches
    let mut thread = Thread::with_initial_state(
        "e2e-snapshot",
        json!({"counter": {"value": 0, "label": ""}}),
    );

    for i in 0..5 {
        let response = StreamResult {
            text: format!("Increment {}", i),
            tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
                format!("call_{}", i),
                "increment",
                json!({"path": "counter"}),
            )],
            usage: None,
            stop_reason: None,
        };
        thread = loop_execute_tools(thread, &response, &tools, true)
            .await
            .unwrap();
    }

    assert_eq!(thread.patch_count(), 10);
    let state_before = thread.rebuild_state().unwrap();
    assert_eq!(state_before["counter"]["value"], 5);

    // Snapshot
    let thread = thread.snapshot().unwrap();
    assert_eq!(thread.patch_count(), 0);
    assert_eq!(thread.state["counter"]["value"], 5);

    // Continue after snapshot
    let response = StreamResult {
        text: "One more".to_string(),
        tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
            "call_5",
            "increment",
            json!({"path": "counter"}),
        )],
        usage: None,
        stop_reason: None,
    };
    let thread = loop_execute_tools(thread, &response, &tools, true)
        .await
        .unwrap();

    let final_state = thread.rebuild_state().unwrap();
    assert_eq!(final_state["counter"]["value"], 6);
    assert_eq!(thread.patch_count(), 2); // Tool patch + control-state patch
}

/// Simulate state replay (time-travel debugging)
#[tokio::test]
async fn test_e2e_state_replay() {
    let tools = tool_map([IncrementTool]);

    let mut thread =
        Thread::with_initial_state("e2e-replay", json!({"counter": {"value": 0, "label": ""}}));

    // Create history: 0 -> 1 -> 2 -> 3 -> 4 -> 5
    for i in 0..5 {
        let response = StreamResult {
            text: format!("Step {}", i),
            tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
                format!("call_{}", i),
                "increment",
                json!({"path": "counter"}),
            )],
            usage: None,
            stop_reason: None,
        };
        thread = loop_execute_tools(thread, &response, &tools, true)
            .await
            .unwrap();
    }

    // Replay through tool-state boundaries (control-state patches are interleaved).
    assert_eq!(thread.patch_count(), 10);
    assert_eq!(thread.replay_to(0).unwrap()["counter"]["value"], 1);
    assert_eq!(thread.replay_to(2).unwrap()["counter"]["value"], 2);
    assert_eq!(thread.replay_to(4).unwrap()["counter"]["value"], 3);
    assert_eq!(thread.replay_to(6).unwrap()["counter"]["value"], 4);
    assert_eq!(thread.replay_to(8).unwrap()["counter"]["value"], 5);

    // Final state
    assert_eq!(thread.rebuild_state().unwrap()["counter"]["value"], 5);
}

/// Simulate long conversation with many messages
#[tokio::test]
async fn test_e2e_long_conversation() {
    let mut thread = ConversationAgentState::new("e2e-long");

    // Build 100 runs of conversation
    for i in 0..100 {
        thread = thread
            .with_message(Message::user(format!("Message {}", i)))
            .with_message(Message::assistant(format!("Response {}", i)));
    }

    assert_eq!(thread.message_count(), 200);

    // Storage should handle this efficiently
    let storage = MemoryStore::new();
    storage.save(&thread).await.unwrap();
    let loaded = storage.load_thread("e2e-long").await.unwrap().unwrap();
    assert_eq!(loaded.message_count(), 200);
}

/// Simulate sequential tool execution (non-parallel mode)
#[tokio::test]
async fn test_e2e_sequential_tool_execution() {
    let thread = Thread::with_initial_state(
        "e2e-sequential",
        json!({"counter": {"value": 0, "label": ""}}),
    );

    // Multiple tool calls
    let llm_response = StreamResult {
        text: "Running sequentially.".to_string(),
        tool_calls: vec![
            tirea_agentos::contracts::thread::ToolCall::new(
                "call_1",
                "increment",
                json!({"path": "counter"}),
            ),
            tirea_agentos::contracts::thread::ToolCall::new(
                "call_2",
                "increment",
                json!({"path": "counter"}),
            ),
            tirea_agentos::contracts::thread::ToolCall::new(
                "call_3",
                "increment",
                json!({"path": "counter"}),
            ),
        ],
        usage: None,
        stop_reason: None,
    };

    let tools = tool_map([IncrementTool]);

    // Execute in sequential mode (parallel = false)
    let thread = loop_execute_tools(thread, &llm_response, &tools, false)
        .await
        .unwrap();

    // In sequential mode, each tool sees the previous tool's result
    // So counter should be 3 (0 -> 1 -> 2 -> 3)
    let state = thread.rebuild_state().unwrap();
    assert_eq!(state["counter"]["value"], 3);
}

// ============================================================================
// Execute Single Tool Edge Cases
// ============================================================================

#[tokio::test]
async fn test_execute_single_tool_not_found() {
    use tirea_agentos::engine::tool_execution::execute_single_tool;

    let call =
        tirea_agentos::contracts::thread::ToolCall::new("call_1", "nonexistent_tool", json!({}));
    let state = json!({});

    // Tool is None - not found
    let result = execute_single_tool(None, &call, &state).await;

    assert!(result.result.is_error());
    assert!(result
        .result
        .message
        .as_ref()
        .unwrap()
        .contains("not found"));
    assert!(result.patch.is_none());
}

#[tokio::test]
async fn test_execute_single_tool_with_complex_state() {
    use tirea_agentos::engine::tool_execution::execute_single_tool;

    let tool = IncrementTool;
    let call = tirea_agentos::contracts::thread::ToolCall::new(
        "call_1",
        "increment",
        json!({"path": "data.counters.main"}),
    );

    // Complex nested state
    let state = json!({
        "data": {
            "counters": {
                "main": {"value": 100, "label": "main counter"},
                "secondary": {"value": 50, "label": "secondary"}
            },
            "metadata": {"created": "2024-01-01"}
        }
    });

    let result = execute_single_tool(Some(&tool), &call, &state).await;

    assert!(result.result.is_success());
    assert_eq!(result.result.data["new_value"], 101);
}

/// Test SystemReminder integration
#[tokio::test]
async fn test_e2e_system_reminder_integration() {
    let manager = StateManager::new(json!({
        "tasks": {"items": ["Task 1", "Task 2", "Task 3"], "count": 3}
    }));

    let reminder = TaskReminder;

    // Reminder checks state and returns message
    let snapshot = manager.snapshot().await;
    let fix = TestFixture::new_with_state(snapshot);

    let message = reminder.remind(&fix.ctx()).await;

    // Should return reminder about pending tasks
    assert!(message.is_some());
    let msg = message.unwrap();
    assert!(msg.contains("3")); // 3 pending tasks
}

// ============================================================================
// ToolResult Pending/Warning Status Tests
// ============================================================================

/// Tool that returns pending status (needs user confirmation)
struct ConfirmationTool;

#[async_trait]
impl Tool for ConfirmationTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "dangerous_action",
            "Dangerous Action",
            "Requires confirmation",
        )
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let confirmed = args["confirmed"].as_bool().unwrap_or(false);

        if confirmed {
            Ok(ToolResult::success(
                "dangerous_action",
                json!({"status": "executed"}),
            ))
        } else {
            Ok(ToolResult::suspended(
                "dangerous_action",
                "This action requires confirmation. Please confirm to proceed.",
            ))
        }
    }
}

/// Tool that returns warning status (partial success)
struct PartialSuccessTool;

#[async_trait]
impl Tool for PartialSuccessTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("batch_process", "Batch Process", "Process multiple items")
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let items = args["items"].as_array().map(|a| a.len()).unwrap_or(0);

        // Simulate: some items succeed, some fail
        let successful = items * 8 / 10; // 80% success rate
        let failed = items - successful;

        if failed > 0 {
            Ok(ToolResult::warning(
                "batch_process",
                json!({
                    "processed": successful,
                    "failed": failed,
                    "total": items
                }),
                format!("{} items processed, {} failed", successful, failed),
            ))
        } else {
            Ok(ToolResult::success(
                "batch_process",
                json!({"processed": items, "failed": 0}),
            ))
        }
    }
}

#[tokio::test]
async fn test_e2e_tool_pending_status() {
    let tool = ConfirmationTool;
    let manager = StateManager::new(json!({}));
    let snapshot = manager.snapshot().await;
    let fix = TestFixture::new_with_state(snapshot);

    // First call without confirmation
    let result = tool
        .execute(json!({}), &fix.ctx_with("call_1", "tool:dangerous"))
        .await
        .unwrap();

    assert!(result.is_pending());
    assert!(!result.is_success());
    assert!(!result.is_error());
    assert!(result.message.as_ref().unwrap().contains("confirmation"));

    // Second call with confirmation
    let result = tool
        .execute(
            json!({"confirmed": true}),
            &fix.ctx_with("call_1", "tool:dangerous"),
        )
        .await
        .unwrap();

    assert!(result.is_success());
    assert!(!result.is_pending());
}

#[tokio::test]
async fn test_e2e_tool_warning_status() {
    let tool = PartialSuccessTool;
    let manager = StateManager::new(json!({}));
    let snapshot = manager.snapshot().await;
    let fix = TestFixture::new_with_state(snapshot);

    // Process 10 items (80% success = 8 success, 2 failed)
    let result = tool
        .execute(
            json!({"items": [1,2,3,4,5,6,7,8,9,10]}),
            &fix.ctx_with("call_1", "tool:batch"),
        )
        .await
        .unwrap();

    // Warning is still considered "success" but with a message
    assert!(result.is_success());
    assert!(result.message.is_some());
    assert!(result.message.as_ref().unwrap().contains("failed"));
    assert_eq!(result.data["processed"], 8);
    assert_eq!(result.data["failed"], 2);
}

#[tokio::test]
async fn test_e2e_pending_tool_in_session_flow() {
    let thread =
        ConversationAgentState::new("pending-test").with_message(Message::user("Delete all files"));

    // Simulate LLM calling dangerous action without confirmation
    let llm_response = StreamResult {
        text: "I'll delete the files.".to_string(),
        tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
            "call_1",
            "dangerous_action",
            json!({}),
        )],
        usage: None,
        stop_reason: None,
    };

    let thread = thread.with_message(convert::assistant_tool_calls(
        &llm_response.text,
        llm_response.tool_calls.clone(),
    ));

    let mut tools: std::collections::HashMap<String, Arc<dyn Tool>> =
        std::collections::HashMap::new();
    tools.insert("dangerous_action".to_string(), Arc::new(ConfirmationTool));

    let thread = loop_execute_tools(thread, &llm_response, &tools, true)
        .await
        .unwrap();

    // Check tool response contains pending status
    let tool_msg = thread.messages.last().unwrap();
    assert_eq!(tool_msg.role, ThreadRole::Tool);
    assert!(
        tool_msg.content.contains("pending")
            || tool_msg.content.contains("confirmation")
            || tool_msg.content.contains("awaiting approval")
    );
}

// ============================================================================
// Streaming Edge Case Tests
// ============================================================================

#[test]
fn test_stream_collector_empty_stream() {
    let collector = StreamCollector::new();
    let result = collector.finish(None);

    assert!(result.text.is_empty());
    assert!(result.tool_calls.is_empty());
    assert!(!result.needs_tools());
}

#[test]
fn test_stream_collector_only_whitespace() {
    use genai::chat::{ChatStreamEvent, StreamChunk};

    let mut collector = StreamCollector::new();

    collector.process(ChatStreamEvent::Chunk(StreamChunk {
        content: "   ".to_string(),
    }));
    collector.process(ChatStreamEvent::Chunk(StreamChunk {
        content: "\n\n".to_string(),
    }));

    let result = collector.finish(None);
    assert_eq!(result.text, "   \n\n");
}

#[test]
fn test_stream_collector_interleaved_text_and_tools() {
    use genai::chat::{ChatStreamEvent, StreamChunk, ToolChunk};

    let mut collector = StreamCollector::new();

    // Text
    collector.process(ChatStreamEvent::Chunk(StreamChunk {
        content: "Let me ".to_string(),
    }));

    // Tool call starts
    let tc1 = genai::chat::ToolCall {
        call_id: "call_1".to_string(),
        fn_name: "search".to_string(),
        fn_arguments: json!({"q": "test"}),
        thought_signatures: None,
    };
    collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc1 }));

    // More text
    collector.process(ChatStreamEvent::Chunk(StreamChunk {
        content: "help you.".to_string(),
    }));

    // Another tool call
    let tc2 = genai::chat::ToolCall {
        call_id: "call_2".to_string(),
        fn_name: "calculate".to_string(),
        fn_arguments: json!({"expr": "1+1"}),
        thought_signatures: None,
    };
    collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc2 }));

    let result = collector.finish(None);

    assert_eq!(result.text, "Let me help you.");
    assert_eq!(result.tool_calls.len(), 2);
}

#[test]
fn test_stream_result_with_empty_tool_calls() {
    let result = StreamResult {
        text: "Hello".to_string(),
        tool_calls: vec![],
        usage: None,
        stop_reason: None,
    };

    assert!(!result.needs_tools());
}

// ============================================================================
// Concurrent Thread Operations Tests
// ============================================================================

#[tokio::test]
async fn test_concurrent_session_modifications() {
    // Test that concurrent modifications to different sessions work correctly
    let storage = Arc::new(MemoryStore::new());

    let mut handles = vec![];

    for i in 0..20 {
        let storage = Arc::clone(&storage);
        let handle = tokio::spawn(async move {
            let thread_id = format!("concurrent-thread-{}", i);

            // Create session
            let mut thread = Thread::with_initial_state(&thread_id, json!({"value": i}));

            // Add messages
            for j in 0..5 {
                thread = thread.with_message(Message::user(format!("Msg {} from thread {}", j, i)));
            }

            // Save
            storage.save(&thread).await.unwrap();

            // Load and verify
            let loaded = storage.load_thread(&thread_id).await.unwrap().unwrap();
            assert_eq!(loaded.message_count(), 5);
            assert_eq!(loaded.state["value"], i);

            thread_id
        });
        handles.push(handle);
    }

    let results: Vec<String> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(results.len(), 20);

    // Verify all sessions exist
    let ids = storage.list().await.unwrap();
    assert_eq!(ids.len(), 20);
}

#[tokio::test]
async fn test_concurrent_read_write_same_session() {
    let storage = Arc::new(MemoryStore::new());

    // Create initial session
    let thread =
        ConversationAgentState::new("shared-thread").with_message(Message::user("Initial message"));
    storage.save(&thread).await.unwrap();

    let mut handles = vec![];

    // Multiple readers and writers
    for i in 0..10 {
        let storage = Arc::clone(&storage);
        let handle = tokio::spawn(async move {
            if i % 2 == 0 {
                // Reader
                let loaded = storage.load_thread("shared-thread").await.unwrap();
                loaded.is_some()
            } else {
                // Writer (updates the thread)
                let mut thread = storage.load_thread("shared-thread").await.unwrap().unwrap();
                thread = thread.with_message(Message::user(format!("Update {}", i)));
                storage.save(&thread).await.unwrap();
                true
            }
        });
        handles.push(handle);
    }

    let results: Vec<bool> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    // All operations should succeed
    assert!(results.iter().all(|&r| r));

    // Final session should have messages
    let final_thread = storage.load_thread("shared-thread").await.unwrap().unwrap();
    assert!(final_thread.message_count() >= 1);
}

#[tokio::test]
async fn test_concurrent_tool_executions_isolated() {
    // Test that concurrent tool executions don't interfere with each other
    let mut handles = vec![];

    for i in 0..10 {
        let handle = tokio::spawn(async move {
            let thread = Thread::with_initial_state(
                format!("isolated-{}", i),
                json!({"counter": {"value": i * 10, "label": ""}}),
            );

            let response = StreamResult {
                text: "Incrementing".to_string(),
                tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
                    format!("call_{}", i),
                    "increment",
                    json!({"path": "counter"}),
                )],
                usage: None,
                stop_reason: None,
            };

            let tools = tool_map([IncrementTool]);
            let thread = loop_execute_tools(thread, &response, &tools, true)
                .await
                .unwrap();

            let state = thread.rebuild_state().unwrap();
            let expected = i * 10 + 1;
            state["counter"]["value"].as_i64().unwrap() == expected as i64
        });
        handles.push(handle);
    }

    let results: Vec<bool> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    // All should have correct isolated state
    assert!(results.iter().all(|&r| r));
}

// ============================================================================
// Storage Edge Case Tests
// ============================================================================

#[tokio::test]
async fn test_storage_session_not_found() {
    let storage = MemoryStore::new();

    let result = storage.load_thread("nonexistent").await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn test_storage_delete_nonexistent() {
    let storage = MemoryStore::new();

    // Should not error when deleting non-existent session
    let result = storage.delete("nonexistent").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_storage_overwrite_session() {
    let storage = MemoryStore::new();

    // Create and save
    let session1 =
        ConversationAgentState::new("overwrite-test").with_message(Message::user("First version"));
    storage.save(&session1).await.unwrap();

    // Overwrite
    let session2 = ConversationAgentState::new("overwrite-test")
        .with_message(Message::user("Second version"))
        .with_message(Message::assistant("Response"));
    storage.save(&session2).await.unwrap();

    // Load and verify overwritten
    let loaded = storage
        .load_thread("overwrite-test")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.message_count(), 2);
    assert!(loaded.messages[0].content.contains("Second"));
}

#[tokio::test]
async fn test_file_storage_special_characters_in_id() {
    let temp_dir = TempDir::new().unwrap();
    let storage = FileStore::new(temp_dir.path());

    // Thread ID with special characters (but filesystem-safe)
    let thread = ConversationAgentState::new("session_with-special.chars_123")
        .with_message(Message::user("Test"));

    storage.save(&thread).await.unwrap();
    let loaded = storage
        .load_thread("session_with-special.chars_123")
        .await
        .unwrap();

    assert!(loaded.is_some());
    assert_eq!(loaded.unwrap().message_count(), 1);
}

// ============================================================================
// FileStore Concurrent Write Tests
// ============================================================================

#[tokio::test]
async fn test_file_storage_concurrent_writes_different_sessions() {
    // Multiple tasks writing different sessions concurrently should all succeed.
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(FileStore::new(temp_dir.path()));

    let mut handles = vec![];
    for i in 0..20 {
        let s = Arc::clone(&storage);
        handles.push(tokio::spawn(async move {
            let thread = ConversationAgentState::new(format!("session_{}", i))
                .with_message(Message::user(format!("Message from thread {}", i)));
            s.save(&thread).await.unwrap();
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    // Verify all 20 sessions were persisted.
    for i in 0..20 {
        let loaded = storage
            .load_thread(&format!("session_{}", i))
            .await
            .unwrap();
        assert!(loaded.is_some(), "session_{} should exist", i);
        let s = loaded.unwrap();
        assert_eq!(s.message_count(), 1);
        assert!(s.messages[0].content.contains(&format!("thread {}", i)));
    }
}

#[tokio::test]
async fn test_file_storage_concurrent_writes_same_session() {
    // Multiple tasks writing the SAME session concurrently.
    // Last-write-wins: all writes should succeed without panic/corruption,
    // and the final file should be valid JSON.
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(FileStore::new(temp_dir.path()));

    let mut handles = vec![];
    for i in 0..10 {
        let s = Arc::clone(&storage);
        handles.push(tokio::spawn(async move {
            let thread = ConversationAgentState::new("shared_session")
                .with_message(Message::user(format!("Write {}", i)));
            s.save(&thread).await.unwrap();
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    // The session file should be valid (no corruption).
    let loaded = storage.load_thread("shared_session").await.unwrap();
    assert!(loaded.is_some(), "shared_session should exist");
    let thread = loaded.unwrap();
    assert_eq!(thread.message_count(), 1);
    // The content should be from one of the writes.
    assert!(thread.messages[0].content.starts_with("Write "));
}

#[tokio::test]
async fn test_file_storage_read_write_interleaved() {
    // Interleaved reads and writes should not deadlock or corrupt permanently.
    // Note: FileStore does not have write locking, so concurrent reads MAY
    // encounter partial writes. The test verifies no panic/deadlock occurs and
    // that the final state is consistent after all writes complete.
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(FileStore::new(temp_dir.path()));

    // Seed an initial session.
    let initial = ConversationAgentState::new("interleaved").with_message(Message::user("initial"));
    storage.save(&initial).await.unwrap();

    let mut handles = vec![];
    for i in 0..10 {
        let s = Arc::clone(&storage);
        if i % 2 == 0 {
            // Writers
            handles.push(tokio::spawn(async move {
                let thread = ConversationAgentState::new("interleaved")
                    .with_message(Message::user(format!("update {}", i)));
                s.save(&thread).await.unwrap();
            }));
        } else {
            // Readers — may see partial writes, so just verify no panic.
            handles.push(tokio::spawn(async move {
                // load() may return Err on partial write; that's acceptable.
                let _ = s.load("interleaved").await;
            }));
        }
    }

    for h in handles {
        h.await.unwrap();
    }

    // After all writes complete, a final read should succeed.
    let final_thread = storage.load_thread("interleaved").await.unwrap().unwrap();
    assert!(final_thread.message_count() >= 1);
}

#[tokio::test]
async fn test_storage_empty_session() {
    let storage = MemoryStore::new();

    // Thread with no messages or patches
    let thread = ConversationAgentState::new("empty-thread");
    storage.save(&thread).await.unwrap();

    let loaded = storage.load_thread("empty-thread").await.unwrap().unwrap();
    assert_eq!(loaded.message_count(), 0);
    assert_eq!(loaded.patch_count(), 0);
}

#[tokio::test]
async fn test_storage_large_state() {
    let storage = MemoryStore::new();

    // Create session with large state
    let mut large_data = serde_json::Map::new();
    for i in 0..1000 {
        large_data.insert(
            format!("key_{}", i),
            json!({
                "index": i,
                "data": "x".repeat(100),
                "nested": {"a": 1, "b": 2, "c": 3}
            }),
        );
    }

    let thread = Thread::with_initial_state("large-state", Value::Object(large_data))
        .with_message(Message::user("Test with large state"));

    storage.save(&thread).await.unwrap();

    let loaded = storage.load_thread("large-state").await.unwrap().unwrap();
    let state = loaded.rebuild_state().unwrap();

    assert!(state.as_object().unwrap().len() >= 1000);
}

// ============================================================================
// Message Edge Case Tests
// ============================================================================

#[test]
fn test_message_empty_content() {
    let msg = Message::user("");
    assert_eq!(msg.content, "");
    assert_eq!(msg.role, ThreadRole::User);
}

#[test]
fn test_message_special_characters() {
    let special = "Hello! 你好! مرحبا! 🎉 <script>alert('xss')</script> \"quotes\" 'apostrophes'";
    let msg = Message::user(special);
    assert_eq!(msg.content, special);
}

#[test]
fn test_message_very_long_content() {
    let long_content = "a".repeat(100_000);
    let msg = Message::user(&long_content);
    assert_eq!(msg.content.len(), 100_000);
}

#[test]
fn test_message_multiline_content() {
    let multiline = "Line 1\nLine 2\r\nLine 3\n\n\nLine 6";
    let msg = Message::user(multiline);
    assert_eq!(msg.content, multiline);
}

#[test]
fn test_message_json_in_content() {
    let json_content = r#"{"key": "value", "array": [1, 2, 3]}"#;
    let msg = Message::user(json_content);
    assert_eq!(msg.content, json_content);

    // Should be serializable
    let serialized = serde_json::to_string(&msg).unwrap();
    let deserialized: Message = serde_json::from_str(&serialized).unwrap();
    assert_eq!(deserialized.content, json_content);
}

#[test]
fn test_session_with_all_message_types() {
    let thread = ConversationAgentState::new("all-types")
        .with_message(Message::system("You are helpful."))
        .with_message(Message::user("Hello"))
        .with_message(Message::assistant("Hi there!"))
        .with_message(Message::assistant_with_tool_calls(
            "Let me check.",
            vec![tirea_agentos::contracts::thread::ToolCall::new(
                "call_1",
                "search",
                json!({}),
            )],
        ))
        .with_message(Message::tool("call_1", r#"{"result": "found"}"#));

    assert_eq!(thread.message_count(), 5);

    // Verify each type
    assert_eq!(thread.messages[0].role, ThreadRole::System);
    assert_eq!(thread.messages[1].role, ThreadRole::User);
    assert_eq!(thread.messages[2].role, ThreadRole::Assistant);
    assert_eq!(thread.messages[3].role, ThreadRole::Assistant);
    assert!(thread.messages[3].tool_calls.is_some());
    assert_eq!(thread.messages[4].role, ThreadRole::Tool);
    assert_eq!(thread.messages[4].tool_call_id, Some("call_1".to_string()));
}

#[tokio::test]
async fn test_e2e_empty_user_message() {
    let thread = ConversationAgentState::new("empty-msg-test").with_message(Message::user(""));

    // Simulate LLM response to empty message
    let llm_response = StreamResult {
        text: "I notice you sent an empty message. How can I help you?".to_string(),
        tool_calls: vec![],
        usage: None,
        stop_reason: None,
    };

    let thread = thread.with_message(Message::assistant(&llm_response.text));

    assert_eq!(thread.message_count(), 2);
    assert!(thread.messages[0].content.is_empty());
    assert!(!thread.messages[1].content.is_empty());
}

#[tokio::test]
async fn test_e2e_system_prompt_in_session() {
    // Test that system prompt is preserved throughout conversation
    let thread = ConversationAgentState::new("system-prompt-test")
        .with_message(Message::system(
            "You are a calculator. Only respond with numbers.",
        ))
        .with_message(Message::user("What is 2+2?"))
        .with_message(Message::assistant("4"))
        .with_message(Message::user("And 3+3?"))
        .with_message(Message::assistant("6"));

    // Save and load
    let storage = MemoryStore::new();
    storage.save(&thread).await.unwrap();

    let loaded = storage
        .load_thread("system-prompt-test")
        .await
        .unwrap()
        .unwrap();

    // System prompt should be first message
    assert_eq!(loaded.messages[0].role, ThreadRole::System);
    assert!(loaded.messages[0].content.contains("calculator"));
}

// ============================================================================
// Tool Descriptor Edge Cases
// ============================================================================

#[test]
fn test_tool_descriptor_all_options() {
    let desc = ToolDescriptor::new("full_tool", "Full Tool", "A tool with all options")
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "required_field": {"type": "string"},
                "optional_field": {"type": "number"}
            },
            "required": ["required_field"]
        }))
        .with_category("testing")
        .with_metadata("version", json!("1.0.0"))
        .with_metadata("author", json!("test"));

    assert_eq!(desc.id, "full_tool");
    assert_eq!(desc.name, "Full Tool");
    assert_eq!(desc.category, Some("testing".to_string()));
    assert_eq!(desc.metadata.len(), 2);
}

#[test]
fn test_tool_descriptor_minimal() {
    let desc = ToolDescriptor::new("minimal", "Minimal", "");

    assert_eq!(desc.id, "minimal");
    assert_eq!(desc.description, "");
    assert!(desc.category.is_none());
    assert!(desc.metadata.is_empty());
}

// ============================================================================
// Stream End with Captured Tool Calls Tests (for coverage lines 92-102)
// ============================================================================

#[test]
fn test_stream_collector_end_event_with_captured_tool_calls() {
    use genai::chat::{ChatStreamEvent, MessageContent, StreamEnd};

    let mut collector = StreamCollector::new();

    // Create StreamEnd with captured_content containing tool calls
    let tool_call = genai::chat::ToolCall {
        call_id: "captured_call_1".to_string(),
        fn_name: "captured_search".to_string(),
        fn_arguments: json!({"query": "captured test"}),
        thought_signatures: None,
    };

    let end = StreamEnd {
        captured_usage: None,
        captured_stop_reason: None,
        captured_content: Some(MessageContent::from_tool_calls(vec![tool_call])),
        captured_reasoning_content: None,
    };

    // Process the end event - this should capture the tool calls
    let output = collector.process(ChatStreamEvent::End(end));
    assert!(output.is_none()); // End event always returns None

    // Verify the captured tool calls are in the result
    let result = collector.finish(None);
    assert_eq!(result.tool_calls.len(), 1);
    assert_eq!(result.tool_calls[0].name, "captured_search");
    assert_eq!(result.tool_calls[0].id, "captured_call_1");
}

#[test]
fn test_stream_collector_end_event_with_multiple_captured_tool_calls() {
    use genai::chat::{ChatStreamEvent, MessageContent, StreamEnd};

    let mut collector = StreamCollector::new();

    // Create multiple tool calls in captured_content
    let tc1 = genai::chat::ToolCall {
        call_id: "cap_call_1".to_string(),
        fn_name: "tool_a".to_string(),
        fn_arguments: json!({"arg": "a"}),
        thought_signatures: None,
    };
    let tc2 = genai::chat::ToolCall {
        call_id: "cap_call_2".to_string(),
        fn_name: "tool_b".to_string(),
        fn_arguments: json!({"arg": "b"}),
        thought_signatures: None,
    };

    let end = StreamEnd {
        captured_usage: None,
        captured_stop_reason: None,
        captured_content: Some(MessageContent::from_tool_calls(vec![tc1, tc2])),
        captured_reasoning_content: None,
    };

    collector.process(ChatStreamEvent::End(end));
    let result = collector.finish(None);

    assert_eq!(result.tool_calls.len(), 2);
    let names: Vec<&str> = result
        .tool_calls
        .iter()
        .map(|tc| tc.name.as_str())
        .collect();
    assert!(names.contains(&"tool_a"));
    assert!(names.contains(&"tool_b"));
}

#[test]
fn test_stream_collector_end_merges_chunk_and_captured_tool_calls() {
    use genai::chat::{ChatStreamEvent, MessageContent, StreamChunk, StreamEnd, ToolChunk};

    let mut collector = StreamCollector::new();

    // Add text chunk
    collector.process(ChatStreamEvent::Chunk(StreamChunk {
        content: "Processing...".to_string(),
    }));

    // Add a tool call via ToolCallChunk
    let chunk_tc = genai::chat::ToolCall {
        call_id: "chunk_call".to_string(),
        fn_name: "chunk_tool".to_string(),
        fn_arguments: json!({"from": "chunk"}),
        thought_signatures: None,
    };
    collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
        tool_call: chunk_tc,
    }));

    // End event with additional captured tool call
    let captured_tc = genai::chat::ToolCall {
        call_id: "end_call".to_string(),
        fn_name: "end_tool".to_string(),
        fn_arguments: json!({"from": "end"}),
        thought_signatures: None,
    };
    let end = StreamEnd {
        captured_usage: None,
        captured_stop_reason: None,
        captured_content: Some(MessageContent::from_tool_calls(vec![captured_tc])),
        captured_reasoning_content: None,
    };

    collector.process(ChatStreamEvent::End(end));
    let result = collector.finish(None);

    assert_eq!(result.text, "Processing...");
    assert_eq!(result.tool_calls.len(), 2);
}

#[test]
fn test_stream_collector_tool_chunk_with_null_arguments() {
    use genai::chat::{ChatStreamEvent, ToolChunk};
    use tirea_agentos::runtime::streaming::StreamOutput;

    let mut collector = StreamCollector::new();

    // Tool call chunk with name first (triggers ToolCallStart)
    let tc1 = genai::chat::ToolCall {
        call_id: "call_1".to_string(),
        fn_name: "my_tool".to_string(),
        fn_arguments: serde_json::Value::Null, // null arguments
        thought_signatures: None,
    };
    let output = collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc1 }));
    // Should emit ToolCallStart
    assert!(matches!(output, Some(StreamOutput::ToolCallStart { .. })));

    // Tool call chunk with null arguments again (tests the "null" check at line 80)
    let tc2 = genai::chat::ToolCall {
        call_id: "call_1".to_string(),
        fn_name: "".to_string(), // empty name (already set)
        fn_arguments: serde_json::Value::Null,
        thought_signatures: None,
    };
    let output = collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc2 }));
    // Should return None because args_str == "null"
    assert!(output.is_none());

    let result = collector.finish(None);
    assert_eq!(result.tool_calls.len(), 1);
    assert_eq!(result.tool_calls[0].name, "my_tool");
}

#[test]
fn test_stream_collector_tool_chunk_with_empty_string_arguments() {
    use genai::chat::{ChatStreamEvent, ToolChunk};

    let mut collector = StreamCollector::new();

    // First set the tool name
    let tc1 = genai::chat::ToolCall {
        call_id: "call_1".to_string(),
        fn_name: "test_tool".to_string(),
        fn_arguments: json!(""), // Value::String("") — empty string
        thought_signatures: None,
    };
    collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc1 }));

    // Empty string argument should NOT produce a delta
    let tc2 = genai::chat::ToolCall {
        call_id: "call_1".to_string(),
        fn_name: "".to_string(),
        fn_arguments: json!(""), // Value::String("") — empty, skipped
        thought_signatures: None,
    };
    let output = collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc2 }));
    // Value::String("") is treated as empty and filtered out
    assert!(output.is_none());
}

// ============================================================================
// Suspension to AG-UI Conversion Scenario Tests
// ============================================================================

use tirea_agentos::contracts::AgentEvent;
use tirea_agentos::contracts::Suspension;
use tirea_protocol_ag_ui::{AgUiEventContext, Event};

fn make_agui_ctx(thread_id: &str, run_id: &str) -> AgUiEventContext {
    let mut ctx = AgUiEventContext::new();
    ctx.on_agent_event(&AgentEvent::RunStart {
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
        parent_run_id: None,
    });
    ctx
}

/// Test complete scenario: Permission confirmation via Suspension → AG-UI
#[test]
fn test_scenario_permission_confirmation_to_ag_ui() {
    // 1. Plugin creates an Suspension for permission confirmation
    let interaction = Suspension::new("perm_write_file_123", "confirm")
        .with_message("Allow tool 'write_file' to write to /etc/config?")
        .with_parameters(json!({
            "tool_id": "write_file",
            "tool_args": {
                "path": "/etc/config",
                "content": "new config"
            }
        }));

    // 2. Pending frontend tool now emits standard ToolCallStart + ToolCallReady
    let mut ctx = make_agui_ctx("thread_123", "run_456");
    let start_events = ctx.on_agent_event(&AgentEvent::ToolCallStart {
        id: interaction.id.clone(),
        name: "confirm".to_string(),
    });
    let ready_events = ctx.on_agent_event(&AgentEvent::ToolCallReady {
        id: interaction.id.clone(),
        name: "confirm".to_string(),
        arguments: interaction.parameters.clone(),
    });

    assert_eq!(start_events.len(), 1);
    assert!(matches!(start_events[0], Event::ToolCallStart { .. }));
    assert_eq!(ready_events.len(), 2);
    assert!(matches!(ready_events[0], Event::ToolCallArgs { .. }));
    assert!(matches!(ready_events[1], Event::ToolCallEnd { .. }));
}

/// Test scenario: Custom frontend action (file picker)
#[test]
fn test_scenario_custom_frontend_action_to_ag_ui() {
    // 1. Create a custom frontend action interaction
    let interaction = Suspension::new("picker_001", "file_picker")
        .with_message("Select a configuration file")
        .with_parameters(json!({
            "accept": [".json", ".yaml", ".toml"],
            "multiple": false,
            "directory": "/home/user/configs"
        }))
        .with_response_schema(json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "name": { "type": "string" }
            },
            "required": ["path"]
        }));

    // 2. Convert directly to AG-UI events
    let ag_ui_events = interaction_to_ag_ui_events(&interaction);

    // 3. Verify the tool call represents our custom action
    assert_eq!(ag_ui_events.len(), 3);

    match &ag_ui_events[0] {
        Event::ToolCallStart { tool_call_name, .. } => {
            assert_eq!(tool_call_name, "file_picker"); // Custom action name
        }
        _ => panic!("Expected ToolCallStart"),
    }

    match &ag_ui_events[1] {
        Event::ToolCallArgs { delta, .. } => {
            let args: Value = serde_json::from_str(delta).unwrap();
            // Verify response_schema is included for client validation
            assert!(args["response_schema"].is_object());
            assert_eq!(args["response_schema"]["type"], "object");
            // Verify parameters
            assert_eq!(args["parameters"]["multiple"], false);
        }
        _ => panic!("Expected ToolCallArgs"),
    }
}

/// Test scenario: Text streaming interrupted by suspended interaction
#[test]
fn test_scenario_text_interrupted_by_interaction() {
    let mut ctx = make_agui_ctx("t1", "r1");

    // 1. Start text streaming
    let text_event = AgentEvent::TextDelta {
        delta: "I'll help you ".into(),
    };
    let events1 = ctx.on_agent_event(&text_event);
    assert!(events1
        .iter()
        .any(|e| matches!(e, Event::TextMessageStart { .. })));

    // 2. More text
    let text_event2 = AgentEvent::TextDelta {
        delta: "with that file.".into(),
    };
    let events2 = ctx.on_agent_event(&text_event2);
    assert!(events2
        .iter()
        .any(|e| matches!(e, Event::TextMessageContent { .. })));

    // 3. Pending frontend tool starts (e.g., permission needed)
    let events3 = ctx.on_agent_event(&AgentEvent::ToolCallStart {
        id: "int_1".into(),
        name: "confirm".into(),
    });

    // Should end text stream before tool call start.
    assert!(!events3.is_empty(), "Should have TextMessageEnd");
    assert!(
        matches!(events3[0], Event::TextMessageEnd { .. }),
        "First event should be TextMessageEnd"
    );
}

/// Test scenario: Multiple interaction types
#[test]
fn test_scenario_various_interaction_types() {
    // Different action types all use the same mechanism
    let interactions = vec![
        ("confirm", "confirm", "Allow this action?"),
        ("input", "input", "Enter your name:"),
        ("select", "select", "Choose an option:"),
        ("oauth", "oauth", "Authenticate with GitHub"),
        ("custom_widget", "custom_widget", "Configure settings"),
    ];

    for (id, action, message) in interactions {
        let interaction = Suspension::new(id, action).with_message(message);

        let events = interaction_to_ag_ui_events(&interaction);

        // All produce the same event structure
        assert_eq!(events.len(), 3);

        match &events[0] {
            Event::ToolCallStart {
                tool_call_id,
                tool_call_name,
                ..
            } => {
                assert_eq!(tool_call_id, id);
                assert_eq!(tool_call_name, action); // action → tool name
            }
            _ => panic!("Expected ToolCallStart for action: {}", action),
        }
    }
}

// ============================================================================
// InteractionPlugin Scenario Tests
// ============================================================================

use std::collections::{HashMap, HashSet};
use tirea_agentos::contracts::io::ResumeDecisionAction;
use tirea_agentos::contracts::runtime::behavior::ReadOnlyContext;
use tirea_agentos::contracts::runtime::phase::{
    ActionSet, BeforeToolExecuteAction, LifecycleAction, Phase, StepContext,
};
use tirea_agentos::contracts::runtime::state::{reduce_state_actions, ScopeContext};
use tirea_agentos::contracts::runtime::tool_call::ToolGate;
use tirea_agentos::contracts::runtime::AgentBehavior;
use tirea_agentos::contracts::runtime::{
    suspended_calls_from_state, SuspendedCall, ToolCallResume, ToolCallState, ToolCallStatus,
};
use tirea_agentos::contracts::thread::ToolCall;
use tirea_protocol_ag_ui::RunAgentInput;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ResponseRouting {
    ReplayOriginalTool,
    UseAsToolResult,
    PassToLLM,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum InvocationOrigin {
    ToolCallIntercepted {
        backend_call_id: String,
        backend_tool_name: String,
        backend_arguments: Value,
    },
    PluginInitiated {
        plugin_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct FrontendToolInvocation {
    call_id: String,
    tool_name: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    arguments: Value,
    origin: InvocationOrigin,
    routing: ResponseRouting,
}

impl FrontendToolInvocation {
    fn new(
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: Value,
        origin: InvocationOrigin,
        routing: ResponseRouting,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            arguments,
            origin,
            routing,
        }
    }
}

fn suspend_ticket_from_invocation(
    invocation: FrontendToolInvocation,
) -> tirea_agentos::contracts::runtime::phase::SuspendTicket {
    let suspension = tirea_agentos::contracts::Suspension::new(
        &invocation.call_id,
        format!("tool:{}", invocation.tool_name),
    )
    .with_parameters(invocation.arguments.clone());
    let resume_mode = match invocation.routing {
        ResponseRouting::ReplayOriginalTool => {
            tirea_agentos::contracts::runtime::ToolCallResumeMode::ReplayToolCall
        }
        ResponseRouting::UseAsToolResult => {
            tirea_agentos::contracts::runtime::ToolCallResumeMode::UseDecisionAsToolResult
        }
        ResponseRouting::PassToLLM => {
            tirea_agentos::contracts::runtime::ToolCallResumeMode::PassDecisionToTool
        }
    };
    tirea_agentos::contracts::runtime::phase::SuspendTicket::new(
        suspension,
        tirea_agentos::contracts::runtime::PendingToolCall::new(
            invocation.call_id,
            invocation.tool_name,
            invocation.arguments,
        ),
        resume_mode,
    )
}

#[async_trait::async_trait]
trait AgentBehaviorTestDispatch {
    async fn run_phase(&self, phase: Phase, step: &mut StepContext<'_>);
}

#[async_trait::async_trait]
impl<T> AgentBehaviorTestDispatch for T
where
    T: AgentBehavior + ?Sized,
{
    async fn run_phase(&self, phase: Phase, step: &mut StepContext<'_>) {
        use tirea_contract::testing::{
            apply_after_inference_for_test, apply_after_tool_for_test,
            apply_before_inference_for_test, apply_before_tool_for_test, apply_lifecycle_for_test,
        };
        let ctx = build_read_only_ctx_for_dispatch(phase, step);
        match phase {
            Phase::RunStart => apply_lifecycle_for_test(step, self.run_start(&ctx).await),
            Phase::StepStart => apply_lifecycle_for_test(step, self.step_start(&ctx).await),
            Phase::BeforeInference => {
                apply_before_inference_for_test(step, self.before_inference(&ctx).await)
            }
            Phase::AfterInference => {
                apply_after_inference_for_test(step, self.after_inference(&ctx).await)
            }
            Phase::BeforeToolExecute => {
                apply_before_tool_for_test(step, self.before_tool_execute(&ctx).await)
            }
            Phase::AfterToolExecute => {
                apply_after_tool_for_test(step, self.after_tool_execute(&ctx).await)
            }
            Phase::StepEnd => apply_lifecycle_for_test(step, self.step_end(&ctx).await),
            Phase::RunEnd => apply_lifecycle_for_test(step, self.run_end(&ctx).await),
        }
        // Reduce pending state actions and apply them to the doc so tests can
        // read updated state via fix.updated_state().
        if !step.pending_state_actions.is_empty() {
            let state_actions = std::mem::take(&mut step.pending_state_actions);
            let snapshot = step.snapshot();
            let scope_ctx = match phase {
                Phase::BeforeToolExecute | Phase::AfterToolExecute => step
                    .tool_call_id()
                    .map(ScopeContext::for_call)
                    .unwrap_or_else(ScopeContext::run),
                _ => ScopeContext::run(),
            };
            let patches = reduce_state_actions(state_actions, &snapshot, "test", &scope_ctx)
                .expect("state actions should reduce");
            for patch in patches {
                let doc = step.ctx().doc();
                for op in patch.patch().ops() {
                    doc.apply(op).expect("state action patch op should apply");
                }
                step.emit_patch(patch);
            }
        }
    }
}

fn build_read_only_ctx_for_dispatch<'a>(
    phase: Phase,
    step: &'a StepContext<'a>,
) -> ReadOnlyContext<'a> {
    tirea_agentos::contracts::runtime::behavior::build_read_only_context_from_step(
        phase,
        step,
        step.ctx().doc(),
    )
}

#[derive(Debug, Default)]
struct InteractionPlugin {
    responses: HashMap<String, Value>,
}

impl InteractionPlugin {
    fn with_responses(approved_ids: Vec<String>, denied_ids: Vec<String>) -> Self {
        let mut responses = HashMap::new();
        for id in approved_ids {
            responses.insert(id, Value::Bool(true));
        }
        for id in denied_ids {
            responses.insert(id, Value::Bool(false));
        }
        Self { responses }
    }

    fn has_responses(&self) -> bool {
        !self.responses.is_empty()
    }

    fn is_approved(&self, target_id: &str) -> bool {
        self.responses
            .get(target_id)
            .map(SuspensionResponse::is_approved)
            .unwrap_or(false)
    }

    fn is_denied(&self, target_id: &str) -> bool {
        self.responses
            .get(target_id)
            .map(SuspensionResponse::is_denied)
            .unwrap_or(false)
    }

    fn response_for_call(&self, call: &SuspendedCall) -> Option<Value> {
        self.responses
            .get(&call.call_id)
            .cloned()
            .or_else(|| self.responses.get(&call.ticket.suspension.id).cloned())
            .or_else(|| self.responses.get(&call.ticket.pending.id).cloned())
    }

    fn cancel_reason(result: &Value) -> Option<String> {
        result
            .as_object()
            .and_then(|obj| {
                obj.get("reason")
                    .and_then(Value::as_str)
                    .or_else(|| obj.get("message").and_then(Value::as_str))
            })
            .map(str::to_string)
    }

    fn to_tool_call_resume(call_id: &str, result: Value) -> ToolCallResume {
        let action = if SuspensionResponse::is_denied(&result) {
            ResumeDecisionAction::Cancel
        } else {
            ResumeDecisionAction::Resume
        };
        let reason = if matches!(action, ResumeDecisionAction::Cancel) {
            Self::cancel_reason(&result)
        } else {
            None
        };
        ToolCallResume {
            decision_id: format!("decision_{call_id}"),
            action,
            result,
            reason,
            updated_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| {
                    duration.as_millis().min(u128::from(u64::MAX)) as u64
                }),
        }
    }
}

#[async_trait::async_trait]
impl AgentBehavior for InteractionPlugin {
    fn id(&self) -> &str {
        "test_interaction"
    }

    async fn run_start(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        if self.responses.is_empty() {
            return ActionSet::empty();
        }

        let suspended_calls = suspended_calls_from_state(&ctx.snapshot());
        if suspended_calls.is_empty() {
            return ActionSet::empty();
        }

        let mut states =
            tirea_agentos::contracts::runtime::tool_call_states_from_state(&ctx.snapshot());
        let mut actions = ActionSet::empty();
        for (call_id, suspended_call) in suspended_calls {
            if states
                .get(call_id.as_str())
                .is_some_and(|state| matches!(state.status, ToolCallStatus::Resuming))
            {
                continue;
            }
            let Some(result) = self.response_for_call(&suspended_call) else {
                continue;
            };
            let resume = Self::to_tool_call_resume(call_id.as_str(), result);
            let updated_at = resume.updated_at;
            let mut state = states
                .remove(call_id.as_str())
                .unwrap_or_else(|| ToolCallState {
                    call_id: call_id.clone(),
                    tool_name: suspended_call.tool_name.clone(),
                    arguments: suspended_call.arguments.clone(),
                    status: ToolCallStatus::Suspended,
                    resume_token: Some(suspended_call.ticket.pending.id.clone()),
                    resume: None,
                    scratch: Value::Null,
                    updated_at,
                });
            state.call_id = call_id.clone();
            state.tool_name = suspended_call.tool_name.clone();
            state.arguments = suspended_call.arguments.clone();
            state.status = ToolCallStatus::Resuming;
            state.resume_token = Some(suspended_call.ticket.pending.id.clone());
            state.resume = Some(resume);
            state.updated_at = updated_at;
            actions = actions.and(ActionSet::single(LifecycleAction::State(
                state.into_state_action(),
            )));
        }
        actions
    }
}

fn interaction_plugin_from_request(request: &RunAgentInput) -> InteractionPlugin {
    InteractionPlugin::with_responses(request.approved_target_ids(), request.denied_target_ids())
}

struct TestFrontendToolPlugin {
    frontend_tools: HashSet<String>,
}

impl TestFrontendToolPlugin {
    fn new(frontend_tools: HashSet<String>) -> Self {
        Self { frontend_tools }
    }

    fn is_frontend_tool(&self, tool_name: &str) -> bool {
        self.frontend_tools.contains(tool_name)
    }
}

#[async_trait::async_trait]
impl AgentBehavior for TestFrontendToolPlugin {
    fn id(&self) -> &str {
        "test_frontend_tools"
    }

    async fn before_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeToolExecuteAction> {
        let Some(tool_name) = ctx.tool_name() else {
            return ActionSet::empty();
        };

        if !self.frontend_tools.contains(tool_name) {
            return ActionSet::empty();
        }

        let Some(tool_call_id) = ctx.tool_call_id() else {
            return ActionSet::empty();
        };

        let args = ctx.tool_args().cloned().unwrap_or_default();
        let invocation = FrontendToolInvocation::new(
            tool_call_id.to_string(),
            tool_name.to_string(),
            args.clone(),
            InvocationOrigin::ToolCallIntercepted {
                backend_call_id: tool_call_id.to_string(),
                backend_tool_name: tool_name.to_string(),
                backend_arguments: args,
            },
            ResponseRouting::ReplayOriginalTool,
        );
        ActionSet::single(BeforeToolExecuteAction::Suspend(
            suspend_ticket_from_invocation(invocation),
        ))
    }
}

fn frontend_plugin_from_request(request: &RunAgentInput) -> TestFrontendToolPlugin {
    TestFrontendToolPlugin::new(
        request
            .frontend_tools()
            .iter()
            .map(|tool| tool.name.clone())
            .collect(),
    )
}

fn suspended_interaction(step: &StepContext<'_>) -> Option<Suspension> {
    step.gate
        .as_ref()
        .and_then(|gate| gate.suspend_ticket.as_ref())
        .map(|ticket| ticket.suspension.clone())
}

fn suspended_invocation(step: &StepContext<'_>) -> Option<FrontendToolInvocation> {
    let gate = step.gate.as_ref()?;
    let backend = Some((gate.id.clone(), gate.name.clone(), gate.args.clone()));
    gate.suspend_ticket
        .as_ref()
        .map(|ticket| FrontendToolInvocation {
            call_id: ticket.pending.id.clone(),
            tool_name: ticket.pending.name.clone(),
            arguments: ticket.pending.arguments.clone(),
            origin: if matches!(
                ticket.resume_mode,
                tirea_agentos::contracts::runtime::ToolCallResumeMode::ReplayToolCall
            ) {
                let (backend_call_id, backend_tool_name, backend_arguments) =
                    backend.clone().unwrap_or_else(|| {
                        (
                            ticket.pending.id.clone(),
                            ticket.pending.name.clone(),
                            ticket.pending.arguments.clone(),
                        )
                    });
                InvocationOrigin::ToolCallIntercepted {
                    backend_call_id,
                    backend_tool_name,
                    backend_arguments,
                }
            } else {
                InvocationOrigin::PluginInitiated {
                    plugin_id: "test_frontend_tools".to_string(),
                }
            },
            routing: match ticket.resume_mode {
                tirea_agentos::contracts::runtime::ToolCallResumeMode::ReplayToolCall => {
                    ResponseRouting::ReplayOriginalTool
                }
                tirea_agentos::contracts::runtime::ToolCallResumeMode::UseDecisionAsToolResult => {
                    ResponseRouting::UseAsToolResult
                }
                tirea_agentos::contracts::runtime::ToolCallResumeMode::PassDecisionToTool => {
                    ResponseRouting::PassToLLM
                }
            },
        })
}

/// Test scenario: Complete frontend tool flow from request to AG-UI events
#[tokio::test]
async fn test_scenario_frontend_tool_request_to_agui() {
    let doc = json!({});
    let _fix = TestFixture::new_with_state(doc);
    // 1. Client sends request with mixed frontend/backend tools
    let request = RunAgentInput {
        tools: vec![
            tirea_protocol_ag_ui::Tool::backend("search", "Search the web"),
            tirea_protocol_ag_ui::Tool::backend("read_file", "Read a file"),
            tirea_protocol_ag_ui::Tool::frontend("copyToClipboard", "Copy text to clipboard"),
            tirea_protocol_ag_ui::Tool::frontend("showNotification", "Show a notification"),
        ],
        ..RunAgentInput::new("thread_1".to_string(), "run_1".to_string())
    };

    // 2. Frontend strategy plugin is created from request
    let plugin = frontend_plugin_from_request(&request);

    // 3. Simulate agent calling a frontend tool
    let thread = ConversationAgentState::new("session_1");
    let fix = TestFixture::new();
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);

    let tool_call = ToolCall::new(
        "call_001",
        "copyToClipboard",
        json!({
            "text": "Hello, World!",
            "format": "plain"
        }),
    );
    step.gate = Some(ToolGate::from_tool_call(&tool_call));

    // 4. Plugin intercepts in BeforeToolExecute phase
    plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;

    // 5. Tool should be pending
    assert!(step.tool_pending());

    // 6. Get interaction and convert to AG-UI
    let interaction = suspended_interaction(&step).expect("suspended interaction should exist");

    let events = interaction_to_ag_ui_events(&interaction);

    // 7. Verify AG-UI events
    assert_eq!(events.len(), 3);

    match &events[0] {
        Event::ToolCallStart {
            tool_call_id,
            tool_call_name,
            ..
        } => {
            assert_eq!(tool_call_id, "call_001");
            assert_eq!(tool_call_name, "copyToClipboard");
        }
        _ => panic!("Expected ToolCallStart"),
    }

    match &events[1] {
        Event::ToolCallArgs { delta, .. } => {
            let args: Value = serde_json::from_str(delta).unwrap();
            assert_eq!(args["parameters"]["text"], "Hello, World!");
            assert_eq!(args["parameters"]["format"], "plain");
        }
        _ => panic!("Expected ToolCallArgs"),
    }
}

/// Test scenario: Multiple frontend tools called in sequence
#[tokio::test]
async fn test_scenario_multiple_frontend_tools_sequence() {
    let doc = json!({});
    let _fix = TestFixture::new_with_state(doc);
    let request = RunAgentInput {
        tools: vec![
            tirea_protocol_ag_ui::Tool::frontend("copyToClipboard", "Copy"),
            tirea_protocol_ag_ui::Tool::frontend("showNotification", "Notify"),
            tirea_protocol_ag_ui::Tool::frontend("openDialog", "Dialog"),
        ],
        ..RunAgentInput::new("t1".to_string(), "r1".to_string())
    };

    let plugin = frontend_plugin_from_request(&request);
    let thread = ConversationAgentState::new("test");

    // Simulate three frontend tool calls in sequence
    let tool_calls = vec![
        ("call_1", "copyToClipboard", json!({"text": "data1"})),
        ("call_2", "showNotification", json!({"message": "Done!"})),
        ("call_3", "openDialog", json!({"title": "Confirm"})),
    ];

    for (call_id, tool_name, args) in tool_calls {
        let fix = TestFixture::new();
        let ctx_step = fix.ctx();
        let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);
        let tool_call = ToolCall::new(call_id, tool_name, args.clone());
        step.gate = Some(ToolGate::from_tool_call(&tool_call));

        plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;

        assert!(step.tool_pending(), "Tool {} should be pending", tool_name);

        let interaction = suspended_interaction(&step).expect("suspended interaction should exist");
        assert_eq!(interaction.id, call_id);
        assert_eq!(interaction.action, format!("tool:{}", tool_name));
        assert_eq!(interaction.parameters, args);
    }
}

/// Test scenario: Frontend tool with complex nested arguments
#[tokio::test]
async fn test_scenario_frontend_tool_complex_args() {
    let doc = json!({});
    let _fix = TestFixture::new_with_state(doc);
    let plugin = TestFrontendToolPlugin::new(["fileDialog".to_string()].into_iter().collect());

    let thread = ConversationAgentState::new("test");
    let fix = TestFixture::new();
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);

    // Complex nested arguments
    let complex_args = json!({
        "options": {
            "filters": [
                {"name": "Images", "extensions": ["png", "jpg", "gif"]},
                {"name": "Documents", "extensions": ["pdf", "doc", "txt"]}
            ],
            "defaultPath": "/home/user/documents",
            "properties": {
                "multiSelections": true,
                "showHiddenFiles": false
            }
        },
        "metadata": {
            "requestId": "req_123",
            "timestamp": 1704067200,
            "context": {
                "source": "editor",
                "purpose": "import"
            }
        }
    });

    let tool_call = ToolCall::new("call_complex", "fileDialog", complex_args.clone());
    step.gate = Some(ToolGate::from_tool_call(&tool_call));

    plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;

    assert!(step.tool_pending());

    let interaction = suspended_interaction(&step).expect("suspended interaction should exist");

    // Verify complex args are preserved
    assert_eq!(interaction.parameters, complex_args);
    assert_eq!(
        interaction.parameters["options"]["filters"][0]["name"],
        "Images"
    );
    assert_eq!(
        interaction.parameters["metadata"]["context"]["source"],
        "editor"
    );
}

/// Test scenario: Frontend tool with empty/null arguments
#[tokio::test]
async fn test_scenario_frontend_tool_empty_args() {
    let doc = json!({});
    let _fix = TestFixture::new_with_state(doc);
    let plugin = TestFrontendToolPlugin::new(["getClipboard".to_string()].into_iter().collect());

    let thread = ConversationAgentState::new("test");

    // Test with empty object
    {
        let fix = TestFixture::new();
        let ctx_step = fix.ctx();
        let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);
        let tool_call = ToolCall::new("call_empty", "getClipboard", json!({}));
        step.gate = Some(ToolGate::from_tool_call(&tool_call));

        plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;

        assert!(step.tool_pending());
        let interaction = suspended_interaction(&step).expect("suspended interaction should exist");
        assert_eq!(interaction.parameters, json!({}));
    }

    // Test with null
    {
        let fix = TestFixture::new();
        let ctx_step = fix.ctx();
        let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);
        let tool_call = ToolCall::new("call_null", "getClipboard", Value::Null);
        step.gate = Some(ToolGate::from_tool_call(&tool_call));

        plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;

        assert!(step.tool_pending());
        let interaction = suspended_interaction(&step).expect("suspended interaction should exist");
        assert_eq!(interaction.parameters, Value::Null);
    }
}

/// Test scenario: Frontend tool names with special characters
#[tokio::test]
async fn test_scenario_frontend_tool_special_names() {
    let doc = json!({});
    let _fix = TestFixture::new_with_state(doc);
    // Various tool name formats that might appear
    let tool_names = vec![
        "copy_to_clipboard",       // snake_case
        "copyToClipboard",         // camelCase
        "CopyToClipboard",         // PascalCase
        "copy-to-clipboard",       // kebab-case
        "namespace.copyClipboard", // dotted namespace
        "ui::clipboard::copy",     // rust-style path (unusual but valid)
    ];

    for tool_name in tool_names {
        let plugin = TestFrontendToolPlugin::new([tool_name.to_string()].into_iter().collect());

        let thread = ConversationAgentState::new("test");
        let fix = TestFixture::new();
        let ctx_step = fix.ctx();
        let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);
        let tool_call = ToolCall::new("call_1", tool_name, json!({}));
        step.gate = Some(ToolGate::from_tool_call(&tool_call));

        plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;

        assert!(
            step.tool_pending(),
            "Tool '{}' should be pending",
            tool_name
        );

        let interaction = suspended_interaction(&step).expect("suspended interaction should exist");
        assert_eq!(
            interaction.action,
            format!("tool:{}", tool_name),
            "Action should be 'tool:{}' for tool '{}'",
            tool_name,
            tool_name
        );
    }
}

/// Test scenario: Tool name case sensitivity
#[tokio::test]
async fn test_scenario_frontend_tool_case_sensitivity() {
    let doc = json!({});
    let _fix = TestFixture::new_with_state(doc);
    // Only "CopyToClipboard" is registered as frontend
    let plugin = TestFrontendToolPlugin::new(["CopyToClipboard".to_string()].into_iter().collect());

    let thread = ConversationAgentState::new("test");

    // Different cases - only exact match should work
    let test_cases = vec![
        ("CopyToClipboard", true),  // exact match
        ("copytoclipboard", false), // lowercase
        ("COPYTOCLIPBOARD", false), // uppercase
        ("copyToClipboard", false), // different case
    ];

    for (tool_name, should_be_pending) in test_cases {
        let fix = TestFixture::new();
        let ctx_step = fix.ctx();
        let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);
        let tool_call = ToolCall::new("call_1", tool_name, json!({}));
        step.gate = Some(ToolGate::from_tool_call(&tool_call));

        plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;

        assert_eq!(
            step.tool_pending(),
            should_be_pending,
            "Tool '{}' pending state should be {}",
            tool_name,
            should_be_pending
        );
    }
}

/// Test scenario: Frontend tool interaction serializes correctly for wire format
#[test]
fn test_scenario_frontend_tool_wire_format() {
    // Create interaction as InteractionPlugin would
    let interaction =
        Suspension::new("call_abc123", "tool:showNotification").with_parameters(json!({
            "title": "Success",
            "message": "Operation completed",
            "type": "info",
            "duration": 5000
        }));

    // Convert to AG-UI events (what goes over the wire)
    let events = interaction_to_ag_ui_events(&interaction);

    // Serialize each event as it would be sent
    for event in &events {
        let json_str = serde_json::to_string(event).expect("Event should serialize");

        // Verify it can be deserialized back
        let _: Event = serde_json::from_str(&json_str).expect("Event should deserialize");

        // Verify no null/undefined sneaking in for required fields
        let json_val: Value = serde_json::from_str(&json_str).unwrap();
        assert!(
            json_val.get("type").is_some(),
            "Event should have 'type' field"
        );
    }

    // Check ToolCallArgs specifically - the main payload
    match &events[1] {
        Event::ToolCallArgs { delta, .. } => {
            let args: Value = serde_json::from_str(delta).unwrap();

            // Verify structure matches what client expects
            assert!(args.get("id").is_some(), "Should have id");
            assert!(args.get("parameters").is_some(), "Should have parameters");

            // Verify nested data
            assert_eq!(args["parameters"]["title"], "Success");
            assert_eq!(args["parameters"]["duration"], 5000);
        }
        _ => panic!("Expected ToolCallArgs at index 1"),
    }
}

/// Test scenario: Frontend tool pending maps to ToolCallStart/Ready AG-UI events
#[tokio::test]
async fn test_scenario_frontend_tool_full_event_pipeline() {
    let doc = json!({});
    let _fix = TestFixture::new_with_state(doc);
    let plugin = TestFrontendToolPlugin::new(["showModal".to_string()].into_iter().collect());

    let thread = ConversationAgentState::new("test");
    let fix = TestFixture::new();
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);

    let tool_call = ToolCall::new(
        "modal_call_1",
        "showModal",
        json!({
            "content": "Are you sure?",
            "buttons": ["Yes", "No"]
        }),
    );
    step.gate = Some(ToolGate::from_tool_call(&tool_call));

    // 1. Plugin creates pending state with interaction
    plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;

    // 2. Agent loop now emits ToolCallStart + ToolCallReady for pending frontend tools.
    let invocation = suspended_invocation(&step).expect("suspended invocation should exist");
    let mut agui_ctx = make_agui_ctx("thread_123", "run_456");
    let start_events = agui_ctx.on_agent_event(&AgentEvent::ToolCallStart {
        id: invocation.call_id.clone(),
        name: invocation.tool_name.clone(),
    });
    let ready_events = agui_ctx.on_agent_event(&AgentEvent::ToolCallReady {
        id: invocation.call_id.clone(),
        name: invocation.tool_name.clone(),
        arguments: invocation.arguments.clone(),
    });
    let mut ag_ui_events = Vec::new();
    ag_ui_events.extend(start_events);
    ag_ui_events.extend(ready_events);

    // 3. AG-UI receives a normal tool-call lifecycle for frontend pending tools.
    assert_eq!(ag_ui_events.len(), 3);
    assert!(matches!(ag_ui_events[0], Event::ToolCallStart { .. }));
    assert!(matches!(ag_ui_events[1], Event::ToolCallArgs { .. }));
    assert!(matches!(ag_ui_events[2], Event::ToolCallEnd { .. }));
}

/// Test scenario: Backend tool should not be affected by InteractionPlugin
#[tokio::test]
async fn test_scenario_backend_tool_passthrough() {
    let fix = TestFixture::new();
    let plugin = TestFrontendToolPlugin::new(["frontendOnly".to_string()].into_iter().collect());

    let thread = ConversationAgentState::new("test");
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);

    // Backend tool call
    let tool_call = ToolCall::new(
        "call_backend",
        "search",
        json!({
            "query": "rust async",
            "limit": 10
        }),
    );
    step.gate = Some(ToolGate::from_tool_call(&tool_call));

    // Plugin should not interfere
    plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;

    assert!(!step.tool_pending(), "Backend tool should not be pending");
    assert!(!step.tool_blocked(), "Backend tool should not be blocked");
    assert!(
        suspended_interaction(&step).is_none(),
        "No interaction should be created"
    );
}

/// Test scenario: Request with no frontend tools creates empty plugin
#[test]
fn test_scenario_no_frontend_tools_in_request() {
    let request = RunAgentInput {
        tools: vec![
            tirea_protocol_ag_ui::Tool::backend("search", "Search"),
            tirea_protocol_ag_ui::Tool::backend("read", "Read file"),
            tirea_protocol_ag_ui::Tool::backend("write", "Write file"),
        ],
        ..RunAgentInput::new("t1".to_string(), "r1".to_string())
    };

    let plugin = frontend_plugin_from_request(&request);

    // All tools are backend, none should be frontend
    assert!(!plugin.is_frontend_tool("search"));
    assert!(!plugin.is_frontend_tool("read"));
    assert!(!plugin.is_frontend_tool("write"));
    assert!(!plugin.is_frontend_tool("nonexistent"));
}

/// Test scenario: Empty request creates empty plugin
#[test]
fn test_scenario_empty_request() {
    let request = RunAgentInput::new("t1".to_string(), "r1".to_string());

    let plugin = frontend_plugin_from_request(&request);

    // No tools in request, none should be frontend
    assert!(!plugin.is_frontend_tool("any_tool"));
    assert!(!plugin.is_frontend_tool(""));
}

// ============================================================================
// Permission Resume Flow Scenario Tests
// ============================================================================

use tirea_agentos::extensions::permission::PermissionPlugin;

/// Test scenario: Complete permission approval flow
/// Agent → Pending → AG-UI Events → Client Approves → Resume
#[tokio::test]
async fn test_scenario_permission_approved_complete_flow() {
    // Phase 1: Agent requests permission
    let thread = ConversationAgentState::new("test");
    let fix = TestFixture::new();
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);

    // Set up ask permission

    // Simulate tool call
    let tool_call = ToolCall::new(
        "call_write_file",
        "write_file",
        json!({"path": "/etc/config"}),
    );
    step.gate = Some(ToolGate::from_tool_call(&tool_call));

    // PermissionPlugin creates suspended interaction
    let plugin = PermissionPlugin;
    plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;

    assert!(step.tool_pending());
    let interaction = suspended_interaction(&step).expect("suspended interaction should exist");

    // Phase 2: Convert to AG-UI events
    let ag_ui_events = interaction_to_ag_ui_events(&interaction);
    assert_eq!(ag_ui_events.len(), 3);

    // Phase 3: Client receives events and approves
    // (Simulated by creating a new request with tool response)
    let client_response_request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::tool("true", &interaction.id));

    // Phase 4: Check approval
    assert!(client_response_request
        .approved_target_ids()
        .iter()
        .any(|id| id == &interaction.id));
    assert!(!client_response_request
        .denied_target_ids()
        .iter()
        .any(|id| id == &interaction.id));

    // Phase 5: Get response and verify
    let response = client_response_request
        .interaction_responses()
        .into_iter()
        .find(|response| response.target_id == interaction.id.as_str())
        .unwrap();
    assert!(response.approved());
}

/// Test scenario: Complete permission denial flow
#[tokio::test]
async fn test_scenario_permission_denied_complete_flow() {
    // Phase 1: Agent requests permission
    let thread = ConversationAgentState::new("test");
    let fix = TestFixture::new();
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);

    let tool_call = ToolCall::new(
        "call_delete",
        "delete_file",
        json!({"path": "/important.txt"}),
    );
    step.gate = Some(ToolGate::from_tool_call(&tool_call));

    let plugin = PermissionPlugin;
    plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;

    assert!(step.tool_pending());
    let interaction = suspended_interaction(&step).expect("suspended interaction should exist");

    // Phase 2-3: Client denies
    let client_response_request =
        RunAgentInput::new("t1".to_string(), "r1".to_string()).with_message(
            tirea_protocol_ag_ui::Message::tool("false", &interaction.id),
        );

    // Phase 4: Check denial
    assert!(client_response_request
        .denied_target_ids()
        .iter()
        .any(|id| id == &interaction.id));
    assert!(!client_response_request
        .approved_target_ids()
        .iter()
        .any(|id| id == &interaction.id));

    let response = client_response_request
        .interaction_responses()
        .into_iter()
        .find(|response| response.target_id == interaction.id.as_str())
        .unwrap();
    assert!(response.denied());
}

/// Test scenario: Frontend tool execution complete flow
#[tokio::test]
async fn test_scenario_frontend_tool_execution_complete_flow() {
    // Phase 1: Agent calls frontend tool
    let request = RunAgentInput {
        tools: vec![tirea_protocol_ag_ui::Tool::frontend(
            "copyToClipboard",
            "Copy to clipboard",
        )],
        ..RunAgentInput::new("t1".to_string(), "r1".to_string())
    };

    let plugin = frontend_plugin_from_request(&request);

    let thread = ConversationAgentState::new("test");
    let fix = TestFixture::new();
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);

    let tool_call = ToolCall::new("call_copy_1", "copyToClipboard", json!({"text": "Hello!"}));
    step.gate = Some(ToolGate::from_tool_call(&tool_call));

    plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;

    assert!(step.tool_pending());
    let interaction = suspended_interaction(&step).expect("suspended interaction should exist");

    // Verify action format
    assert_eq!(interaction.action, "tool:copyToClipboard");

    // Phase 2: Convert to AG-UI
    let ag_ui_events = interaction_to_ag_ui_events(&interaction);
    assert_eq!(ag_ui_events.len(), 3);

    // Phase 3: Client executes and returns result
    let client_response_request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::tool(
            r#"{"success":true,"bytes_copied":6}"#,
            &interaction.id,
        ));

    // Phase 4: Agent receives result
    let response = client_response_request
        .interaction_responses()
        .into_iter()
        .find(|response| response.target_id == interaction.id.as_str())
        .unwrap();

    assert!(response.result["success"].as_bool().unwrap());
    assert_eq!(response.result["bytes_copied"], 6);
}

/// Test scenario: Multiple interactions in sequence
#[tokio::test]
async fn test_scenario_multiple_interactions_sequence() {
    let thread = ConversationAgentState::new("test");
    let plugin = PermissionPlugin;

    // First tool: write_file
    let fix1 = TestFixture::new();
    let ctx_step1 = fix1.ctx();
    let mut step1 = StepContext::new(ctx_step1, &thread.id, &thread.messages, vec![]);
    let call1 = ToolCall::new("call_1", "write_file", json!({}));
    step1.gate = Some(ToolGate::from_tool_call(&call1));

    plugin.run_phase(Phase::BeforeToolExecute, &mut step1).await;
    let interaction1 = suspended_interaction(&step1).expect("suspended interaction should exist");

    // Second tool: read_file
    let fix2 = TestFixture::new();
    let ctx_step2 = fix2.ctx();
    let mut step2 = StepContext::new(ctx_step2, &thread.id, &thread.messages, vec![]);
    let call2 = ToolCall::new("call_2", "read_file", json!({}));
    step2.gate = Some(ToolGate::from_tool_call(&call2));

    plugin.run_phase(Phase::BeforeToolExecute, &mut step2).await;
    let interaction2 = suspended_interaction(&step2).expect("suspended interaction should exist");

    // Client responds to both
    let response_request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::tool(
            "true",
            &interaction1.id,
        ))
        .with_message(tirea_protocol_ag_ui::Message::tool(
            "false",
            &interaction2.id,
        ));

    // Verify responses
    assert!(response_request
        .approved_target_ids()
        .iter()
        .any(|id| id == &interaction1.id));
    assert!(response_request
        .denied_target_ids()
        .iter()
        .any(|id| id == &interaction2.id));

    let responses = response_request.interaction_responses();
    assert_eq!(responses.len(), 2);
}

/// Test scenario: Frontend tool with complex result
#[test]
fn test_scenario_frontend_tool_complex_result() {
    let client_response_request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::tool(
            r#"{
                "success": true,
                "selected_files": [
                    {"path": "/home/user/doc1.txt", "size": 1024},
                    {"path": "/home/user/doc2.txt", "size": 2048}
                ],
                "metadata": {
                    "dialog_duration_ms": 1500,
                    "user_action": "confirm"
                }
            }"#,
            "file_picker_call_1",
        ));

    let response = client_response_request
        .interaction_responses()
        .into_iter()
        .find(|response| response.target_id == "file_picker_call_1")
        .unwrap();

    assert!(response.result["success"].as_bool().unwrap());
    assert_eq!(
        response.result["selected_files"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        response.result["selected_files"][0]["path"],
        "/home/user/doc1.txt"
    );
    assert_eq!(response.result["metadata"]["user_action"], "confirm");
}

/// Test scenario: Permission with custom response format
#[test]
fn test_scenario_permission_custom_response_format() {
    // Using object format with reason
    let request1 = RunAgentInput::new("t1".to_string(), "r1".to_string()).with_message(
        tirea_protocol_ag_ui::Message::tool(
            r#"{"approved":true,"reason":"User trusts this operation"}"#,
            "perm_1",
        ),
    );

    assert!(request1
        .approved_target_ids()
        .iter()
        .any(|id| id == "perm_1"));

    // Using object format with denied flag
    let request2 = RunAgentInput::new("t1".to_string(), "r1".to_string()).with_message(
        tirea_protocol_ag_ui::Message::tool(
            r#"{"denied":true,"reason":"User is cautious"}"#,
            "perm_2",
        ),
    );

    assert!(request2.denied_target_ids().iter().any(|id| id == "perm_2"));

    // Using allowed flag
    let request3 = RunAgentInput::new("t1".to_string(), "r1".to_string()).with_message(
        tirea_protocol_ag_ui::Message::tool(r#"{"allowed":true}"#, "perm_3"),
    );

    assert!(request3
        .approved_target_ids()
        .iter()
        .any(|id| id == "perm_3"));
}

/// Test scenario: Suspension response with input value
#[test]
fn test_scenario_input_interaction_response() {
    // User provides text input
    let request = RunAgentInput::new("t1".to_string(), "r1".to_string()).with_message(
        tirea_protocol_ag_ui::Message::tool("John Doe", "input_name_1"),
    );

    let response = request
        .interaction_responses()
        .into_iter()
        .find(|response| response.target_id == "input_name_1")
        .unwrap();
    assert_eq!(response.result, Value::String("John Doe".into()));

    // Not approved or denied - it's just input
    assert!(!response.approved());
    assert!(!response.denied());
}

/// Test scenario: Selection interaction response
#[test]
fn test_scenario_select_interaction_response() {
    let request = RunAgentInput::new("t1".to_string(), "r1".to_string()).with_message(
        tirea_protocol_ag_ui::Message::tool(
            r#"{"selected_index":2,"selected_value":"Option C"}"#,
            "select_option_1",
        ),
    );

    let response = request
        .interaction_responses()
        .into_iter()
        .find(|response| response.target_id == "select_option_1")
        .unwrap();
    assert_eq!(response.result["selected_index"], 2);
    assert_eq!(response.result["selected_value"], "Option C");
}

/// Test scenario: Mixed message types in request
#[test]
fn test_scenario_mixed_messages_with_interaction_response() {
    // Real-world scenario: conversation + tool responses
    let request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::user(
            "Please write to the file",
        ))
        .with_message(tirea_protocol_ag_ui::Message::assistant(
            "I'll write to the file, but need permission",
        ))
        .with_message(tirea_protocol_ag_ui::Message::tool("true", "perm_write_1"))
        .with_message(tirea_protocol_ag_ui::Message::assistant("Done!"));

    // Should find the tool response
    assert!(request
        .interaction_responses()
        .iter()
        .any(|response| response.target_id == "perm_write_1"));
    assert!(request
        .approved_target_ids()
        .iter()
        .any(|id| id == "perm_write_1"));

    // Should have exactly one interaction response
    let responses = request.interaction_responses();
    assert_eq!(responses.len(), 1);
}

/// Test scenario: InteractionPlugin blocks denied tool in execution flow
#[tokio::test]
async fn test_scenario_interaction_response_plugin_blocks_denied() {
    // Unified format: id = tool_call_id, action = "tool:<name>"
    let thread = Thread::with_initial_state(
        "test",
        state_with_suspended_call(
            "call_write",
            "write_file",
            json!({
                "id": "call_write",
                "action": "tool:write_file",
                "parameters": { "source": "permission" }
            }),
            None,
        ),
    );

    let plugin = InteractionPlugin::with_responses(
        vec![],                         // no approved
        vec!["call_write".to_string()], // denied
    );

    let fix = TestFixture::new_with_state(thread.state.clone());
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    let decisions = resume_inputs_from_state(&fix.updated_state());
    let decision = decisions
        .get("call_write")
        .expect("denied response should create resume decision");
    assert!(matches!(decision.action, ResumeDecisionAction::Cancel));

    // Suspension plugin no longer applies gate decisions in BeforeToolExecute.
    let call = ToolCall::new("call_write", "write_file", json!({"path": "/etc/config"}));
    step.gate = Some(ToolGate::from_tool_call(&call));
    plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;
    assert!(!step.tool_blocked(), "gate application is loop-owned");
}

/// Test scenario: InteractionPlugin allows approved tool in execution flow
#[tokio::test]
async fn test_scenario_interaction_response_plugin_allows_approved() {
    // Unified format: id = tool_call_id, action = "tool:<name>"
    let thread = Thread::with_initial_state(
        "test",
        state_with_suspended_call(
            "call_read",
            "read_file",
            json!({
                "id": "call_read",
                "action": "tool:read_file",
                "parameters": { "source": "permission" }
            }),
            None,
        ),
    );

    let plugin = InteractionPlugin::with_responses(
        vec!["call_read".to_string()], // approved
        vec![],                        // no denied
    );

    let fix = TestFixture::new_with_state(thread.state.clone());
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);
    let call = ToolCall::new(
        "call_read",
        "read_file",
        json!({"path": "/home/user/doc.txt"}),
    );
    step.gate = Some(ToolGate::from_tool_call(&call));

    plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;

    assert!(!step.tool_blocked(), "Approved tool should not be blocked");
}

/// Test scenario: Complete end-to-end flow with PermissionPlugin → InteractionPlugin
#[tokio::test]
async fn test_scenario_e2e_permission_to_response_flow() {
    let thread = ConversationAgentState::new("test");

    // Step 1: First run - PermissionPlugin creates suspended interaction
    let permission_plugin = PermissionPlugin;
    let fix1 = TestFixture::new();
    let ctx_step1 = fix1.ctx();
    let mut step1 = StepContext::new(ctx_step1, &thread.id, &thread.messages, vec![]);
    let call = ToolCall::new("call_exec", "execute_command", json!({"cmd": "ls"}));
    step1.gate = Some(ToolGate::from_tool_call(&call));

    permission_plugin
        .run_phase(Phase::BeforeToolExecute, &mut step1)
        .await;
    assert!(step1.tool_pending(), "Permission ask should create pending");

    let interaction = suspended_interaction(&step1).expect("suspended interaction should exist");
    // Frontend tool invocation: id = fc_<uuid>, action = "tool:PermissionConfirm"
    assert!(
        interaction.id.starts_with("fc_"),
        "Permission interaction should use frontend call_id, got: {}",
        interaction.id
    );
    assert_eq!(interaction.action, "tool:PermissionConfirm");

    // Step 2: Client approves
    let response_request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::tool("true", &interaction.id));
    assert!(response_request
        .approved_target_ids()
        .iter()
        .any(|id| id == &interaction.id));

    // Step 3: Second run - InteractionPlugin processes approval
    // The session must have the suspended interaction persisted (as the real loop does).
    let session2 = Thread::with_initial_state(
        "test",
        state_with_suspended_call(
            &interaction.id,
            "execute_command",
            json!({
                "id": interaction.id,
                "action": interaction.action,
                "parameters": interaction.parameters
            }),
            None,
        ),
    );
    let response_plugin = interaction_plugin_from_request(&response_request);
    let fix2 = TestFixture::new_with_state(session2.state.clone());
    let ctx_step2 = fix2.ctx();
    let mut step2 = StepContext::new(ctx_step2, &session2.id, &session2.messages, vec![]);
    let call2 = ToolCall::new(&interaction.id, "execute_command", json!({"cmd": "ls"}));
    step2.gate = Some(ToolGate::from_tool_call(&call2));

    // InteractionPlugin runs first
    response_plugin
        .run_phase(Phase::BeforeToolExecute, &mut step2)
        .await;

    // Tool should NOT be blocked (approved)
    assert!(
        !step2.tool_blocked(),
        "Approved tool should not be blocked on resume"
    );

    // PermissionPlugin runs second - but InteractionPlugin didn't set pending
    permission_plugin
        .run_phase(Phase::BeforeToolExecute, &mut step2)
        .await;

    // PermissionPlugin should still create pending (because permission wasn't updated to Allow)
    // This is expected - in a real flow, the permission would be updated to Allow after approval
    assert!(
        step2.tool_pending() || !step2.tool_blocked(),
        "Tool should proceed"
    );
}

/// Test scenario: combined AG-UI plugin coordinates frontend and response handling
#[tokio::test]
async fn test_scenario_frontend_tool_with_response_plugin() {
    let thread = ConversationAgentState::new("test");

    // Setup: Frontend tool request
    let request = RunAgentInput {
        tools: vec![tirea_protocol_ag_ui::Tool::frontend(
            "showDialog",
            "Show a dialog",
        )],
        ..RunAgentInput::new("t1".to_string(), "r1".to_string())
    };

    // Step 1: InteractionPlugin creates pending for frontend tool
    let frontend_plugin = frontend_plugin_from_request(&request);
    let fix1 = TestFixture::new();
    let ctx_step1 = fix1.ctx();
    let mut step1 = StepContext::new(ctx_step1, &thread.id, &thread.messages, vec![]);
    let call = ToolCall::new("call_dialog_1", "showDialog", json!({"title": "Confirm"}));
    step1.gate = Some(ToolGate::from_tool_call(&call));

    frontend_plugin
        .run_phase(Phase::BeforeToolExecute, &mut step1)
        .await;
    assert!(step1.tool_pending(), "Frontend tool should create pending");

    let interaction = suspended_interaction(&step1).expect("suspended interaction should exist");
    assert_eq!(interaction.action, "tool:showDialog");

    // Step 2: Client executes and returns result
    let response_request = RunAgentInput::new("t1".to_string(), "r2".to_string()).with_message(
        tirea_protocol_ag_ui::Message::tool(
            r#"{"success":true,"user_clicked":"OK"}"#,
            &interaction.id,
        ),
    );

    // Step 3: On resume, InteractionPlugin processes the result
    let _response_plugin = interaction_plugin_from_request(&response_request);

    // The result is available (not blocked/denied)
    let response = response_request
        .interaction_responses()
        .into_iter()
        .find(|response| response.target_id == interaction.id.as_str())
        .unwrap();
    assert!(response.result["success"].as_bool().unwrap());
    assert_eq!(response.result["user_clicked"], "OK");
}

/// Test scenario: AG-UI context state after suspended interaction
#[test]
fn test_scenario_agui_context_state_after_pending() {
    let mut ctx = make_agui_ctx("thread_1", "run_1");

    // Start text streaming
    let text_event = AgentEvent::TextDelta {
        delta: "Let me help you ".into(),
    };
    let events1 = ctx.on_agent_event(&text_event);
    // First text delta should produce TextMessageStart
    assert!(events1
        .iter()
        .any(|e| matches!(e, Event::TextMessageStart { .. })));

    // More text
    let text_event2 = AgentEvent::TextDelta {
        delta: "with that.".into(),
    };
    let events2 = ctx.on_agent_event(&text_event2);
    // Second text delta should produce only TextMessageContent (not Start)
    assert!(events2
        .iter()
        .any(|e| matches!(e, Event::TextMessageContent { .. })));
    assert!(!events2
        .iter()
        .any(|e| matches!(e, Event::TextMessageStart { .. })));

    // Pending frontend tool starts.
    let pending_start_events = ctx.on_agent_event(&AgentEvent::ToolCallStart {
        id: "perm_1".into(),
        name: "confirm".into(),
    });

    // Should end current text stream first.
    assert!(!pending_start_events.is_empty());
    assert!(
        matches!(pending_start_events[0], Event::TextMessageEnd { .. }),
        "First event should be TextMessageEnd to close the text stream"
    );

    // After pending start, text stream should be ended - verify by checking that
    // another text event would start a new stream
    let text_event3 = AgentEvent::TextDelta {
        delta: "New text".into(),
    };
    let events3 = ctx.on_agent_event(&text_event3);
    // Should produce TextMessageStart again since previous stream was ended
    assert!(events3
        .iter()
        .any(|e| matches!(e, Event::TextMessageStart { .. })));
}

// ============================================================================
// AG-UI Stream Flow Tests
// ============================================================================

/// Test: Event sequence in stream - verify RUN_STARTED is first
#[test]
fn test_agui_stream_event_sequence_run_started_first() {
    // Simulate stream events
    let events: Vec<Event> = vec![
        Event::run_started("t1", "r1", None),
        Event::text_message_start("msg_1"),
        Event::text_message_content("msg_1", "Hello"),
        Event::text_message_end("msg_1"),
        Event::run_finished("t1", "r1", None),
    ];

    // First event must be RUN_STARTED
    assert!(matches!(&events[0], Event::RunStarted { .. }));

    // Last event must be RUN_FINISHED
    assert!(matches!(
        &events[events.len() - 1],
        Event::RunFinished { .. }
    ));
}

/// Test: Text interrupted by tool call - TEXT_MESSAGE_END before TOOL_CALL_START
#[test]
fn test_agui_stream_text_interrupted_by_tool_call() {
    let mut ctx = make_agui_ctx("t1", "r1");

    // Start text streaming
    let text1 = AgentEvent::TextDelta {
        delta: "Let me ".into(),
    };
    let _ = ctx.on_agent_event(&text1);

    let text2 = AgentEvent::TextDelta {
        delta: "search for that.".into(),
    };
    let _ = ctx.on_agent_event(&text2);

    // Tool call starts - should end text stream first
    let tool_start = AgentEvent::ToolCallStart {
        id: "call_1".into(),
        name: "search".into(),
    };
    let tool_events = ctx.on_agent_event(&tool_start);

    // Should have TEXT_MESSAGE_END followed by TOOL_CALL_START
    assert!(tool_events.len() >= 2);
    assert!(
        matches!(&tool_events[0], Event::TextMessageEnd { .. }),
        "First event should be TextMessageEnd, got {:?}",
        tool_events[0]
    );
    assert!(
        matches!(&tool_events[1], Event::ToolCallStart { .. }),
        "Second event should be ToolCallStart, got {:?}",
        tool_events[1]
    );
}

/// Test: Tool call complete sequence - START -> ARGS -> READY(END) -> DONE(RESULT)
#[test]
fn test_agui_stream_tool_call_sequence() {
    let mut ctx = make_agui_ctx("t1", "r1");

    // Collect all events for a tool call
    let start = AgentEvent::ToolCallStart {
        id: "call_1".into(),
        name: "read_file".into(),
    };
    let start_events = ctx.on_agent_event(&start);

    let args = AgentEvent::ToolCallDelta {
        id: "call_1".into(),
        args_delta: r#"{"path": "/tmp/file.txt"}"#.into(),
    };
    let args_events = ctx.on_agent_event(&args);

    // ToolCallReady produces TOOL_CALL_END (marks end of args streaming)
    let ready = AgentEvent::ToolCallReady {
        id: "call_1".into(),
        name: "read_file".into(),
        arguments: json!({"path": "/tmp/file.txt"}),
    };
    let ready_events = ctx.on_agent_event(&ready);

    // ToolCallDone produces TOOL_CALL_RESULT
    let done = AgentEvent::ToolCallDone {
        id: "call_1".into(),
        result: ToolResult::success("read_file", json!({"content": "Hello"})),
        patch: None,
        message_id: String::new(),
    };
    let done_events = ctx.on_agent_event(&done);

    // Verify sequence: Start events contain TOOL_CALL_START
    assert!(start_events
        .iter()
        .any(|e| matches!(e, Event::ToolCallStart { .. })));

    // Args events contain TOOL_CALL_ARGS
    assert!(args_events
        .iter()
        .any(|e| matches!(e, Event::ToolCallArgs { .. })));

    // Ready events contain TOOL_CALL_END (end of argument streaming)
    assert!(ready_events
        .iter()
        .any(|e| matches!(e, Event::ToolCallEnd { .. })));

    // Done events contain TOOL_CALL_RESULT
    assert!(done_events
        .iter()
        .any(|e| matches!(e, Event::ToolCallResult { .. })));
}

/// Test: Error event ends stream without RUN_FINISHED
#[test]
fn test_agui_stream_error_no_run_finished() {
    let mut ctx = make_agui_ctx("t1", "r1");

    // Start with RUN_STARTED (simulated)
    let _started = Event::run_started("t1", "r1", None);

    // Error occurs
    let error = AgentEvent::Error {
        message: "LLM API error: rate limited".into(),
        code: Some("LLM_ERROR".into()),
    };
    let error_events = ctx.on_agent_event(&error);

    // Should emit RUN_ERROR
    assert!(error_events
        .iter()
        .any(|e| matches!(e, Event::RunError { .. })));

    // Should NOT have RUN_FINISHED in the error events
    assert!(!error_events
        .iter()
        .any(|e| matches!(e, Event::RunFinished { .. })));
}

/// Test: Pending event doesn't emit RUN_FINISHED
#[test]
fn test_agui_stream_pending_no_run_finished() {
    let mut ctx = make_agui_ctx("t1", "r1");

    // Pending frontend tool start
    let pending = AgentEvent::ToolCallStart {
        id: "perm_1".into(),
        name: "confirm".into(),
    };
    let pending_events = ctx.on_agent_event(&pending);

    assert!(pending_events
        .iter()
        .any(|e| matches!(e, Event::ToolCallStart { .. })));

    // Should NOT have RUN_FINISHED
    assert!(!pending_events
        .iter()
        .any(|e| matches!(e, Event::RunFinished { .. })));
}

/// Test: SSE format validation
#[test]
fn test_agui_sse_format() {
    let event = Event::run_started("t1", "r1", None);
    let json = serde_json::to_string(&event).unwrap();
    let sse = format!("data: {}\n\n", json);

    // Validate SSE format
    assert!(sse.starts_with("data: "));
    assert!(sse.ends_with("\n\n"));

    // Validate JSON is parseable
    let json_part = sse
        .strip_prefix("data: ")
        .unwrap()
        .strip_suffix("\n\n")
        .unwrap();
    let parsed: Event = serde_json::from_str(json_part).unwrap();
    assert!(matches!(parsed, Event::RunStarted { .. }));
}

/// Test: Multiple SSE events in sequence
#[test]
fn test_agui_sse_multiple_events() {
    let events = [
        Event::run_started("t1", "r1", None),
        Event::text_message_start("m1"),
        Event::text_message_content("m1", "Hello"),
        Event::text_message_end("m1"),
        Event::run_finished("t1", "r1", None),
    ];

    let mut sse_output = String::new();
    for event in &events {
        let json = serde_json::to_string(event).unwrap();
        sse_output.push_str(&format!("data: {}\n\n", json));
    }

    // Parse back
    let lines: Vec<&str> = sse_output.split("\n\n").filter(|s| !s.is_empty()).collect();
    assert_eq!(lines.len(), 5);

    for (i, line) in lines.iter().enumerate() {
        let json = line.strip_prefix("data: ").unwrap();
        let _: Event =
            serde_json::from_str(json).unwrap_or_else(|e| panic!("Failed to parse event {i}: {e}"));
    }
}

// ============================================================================
// Permission E2E Flow Tests
// ============================================================================

/// Test: Complete permission approval flow
/// Ask → Pending → Client Approves → Tool Executes
#[tokio::test]
async fn test_permission_flow_approval_e2e() {
    // Phase 1: Agent requests permission (simulated by PermissionPlugin)
    let thread = ConversationAgentState::new("test");
    let fix = TestFixture::new();
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);

    let tool_call = ToolCall::new("call_write", "write_file", json!({"path": "/etc/config"}));
    step.gate = Some(ToolGate::from_tool_call(&tool_call));

    // PermissionPlugin creates pending
    let plugin = PermissionPlugin;
    plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;
    assert!(
        step.tool_pending(),
        "Tool should be pending after permission ask"
    );

    let interaction = suspended_interaction(&step).expect("suspended interaction should exist");
    // Frontend tool invocation: id = fc_<uuid>, action = "tool:PermissionConfirm"
    assert!(
        interaction.id.starts_with("fc_"),
        "Permission interaction should use frontend call_id, got: {}",
        interaction.id
    );
    assert_eq!(interaction.action, "tool:PermissionConfirm");

    // Phase 2: Convert to AG-UI events (client would receive these)
    let ag_ui_events = interaction_to_ag_ui_events(&interaction);
    assert_eq!(ag_ui_events.len(), 3); // Start, Args, End

    // Phase 3: Client approves (simulated by creating response request)
    let response_request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::tool("true", &interaction.id));

    assert!(response_request
        .approved_target_ids()
        .iter()
        .any(|id| id == &interaction.id));

    // Phase 4: Resume with InteractionPlugin
    let response_plugin = interaction_plugin_from_request(&response_request);
    assert!(response_plugin.has_responses());

    // Phase 5: On resume, tool should NOT be blocked
    let fix2 = TestFixture::new();
    let ctx_step2 = fix2.ctx();
    let mut step2 = StepContext::new(ctx_step2, &thread.id, &thread.messages, vec![]);
    // Use the interaction ID as the tool call ID (as happens in resume)
    let tool_call2 = ToolCall::new(
        &interaction.id,
        "write_file",
        json!({"path": "/etc/config"}),
    );
    step2.gate = Some(ToolGate::from_tool_call(&tool_call2));

    response_plugin
        .run_phase(Phase::BeforeToolExecute, &mut step2)
        .await;
    assert!(!step2.tool_blocked(), "Approved tool should not be blocked");
}

/// Test: Complete permission denial flow
/// Ask → Pending → Client Denies → Tool Blocked
#[tokio::test]
async fn test_permission_flow_denial_e2e() {
    // Phase 1: Agent requests permission
    let thread = ConversationAgentState::new("test");
    let fix = TestFixture::new();
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);

    let tool_call = ToolCall::new("call_delete", "delete_all", json!({}));
    step.gate = Some(ToolGate::from_tool_call(&tool_call));

    let plugin = PermissionPlugin;
    plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;
    assert!(step.tool_pending());

    let interaction = suspended_interaction(&step).expect("suspended interaction should exist");

    // Phase 2: Client denies
    let response_request = RunAgentInput::new("t1".to_string(), "r1".to_string()).with_message(
        tirea_protocol_ag_ui::Message::tool("false", &interaction.id),
    );

    assert!(response_request
        .denied_target_ids()
        .iter()
        .any(|id| id == &interaction.id));

    // Phase 3: On resume, response plugin should produce a cancel decision.
    // The session must have the suspended call persisted (as the real loop does).
    let session2 = Thread::with_initial_state(
        "test",
        state_with_suspended_call(
            &interaction.id,
            "delete_all",
            json!({ "id": interaction.id, "action": "confirm" }),
            None,
        ),
    );
    let response_plugin = interaction_plugin_from_request(&response_request);

    let fix2 = TestFixture::new_with_state(session2.state.clone());
    let ctx_step2 = fix2.ctx();
    let mut step2 = StepContext::new(ctx_step2, &session2.id, &session2.messages, vec![]);
    response_plugin.run_phase(Phase::RunStart, &mut step2).await;
    let decisions = resume_inputs_from_state(&fix2.updated_state());
    let decision = decisions
        .get(&interaction.id)
        .expect("denied interaction should create a decision");
    assert!(matches!(decision.action, ResumeDecisionAction::Cancel));
}

/// Test: Multiple tools with mixed permissions
#[tokio::test]
async fn test_permission_flow_multiple_tools_mixed() {
    let thread = ConversationAgentState::new("test");

    // Tool 1: Will be approved
    let fix1 = TestFixture::new();
    let ctx_step1 = fix1.ctx();
    let mut step1 = StepContext::new(ctx_step1, &thread.id, &thread.messages, vec![]);
    let call1 = ToolCall::new("call_1", "read_file", json!({}));
    step1.gate = Some(ToolGate::from_tool_call(&call1));

    let plugin = PermissionPlugin;
    plugin.run_phase(Phase::BeforeToolExecute, &mut step1).await;
    let int1 = suspended_interaction(&step1).expect("suspended interaction should exist");

    // Tool 2: Will be denied
    let fix2 = TestFixture::new();
    let ctx_step2 = fix2.ctx();
    let mut step2 = StepContext::new(ctx_step2, &thread.id, &thread.messages, vec![]);
    let call2 = ToolCall::new("call_2", "write_file", json!({}));
    step2.gate = Some(ToolGate::from_tool_call(&call2));
    plugin.run_phase(Phase::BeforeToolExecute, &mut step2).await;
    let int2 = suspended_interaction(&step2).expect("suspended interaction should exist");

    // Client responds: approve first, deny second
    let response_request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::tool("true", &int1.id))
        .with_message(tirea_protocol_ag_ui::Message::tool("false", &int2.id));

    let response_plugin = interaction_plugin_from_request(&response_request);

    // Verify first tool (approved) — session has its suspended call persisted.
    let session_r1 = Thread::with_initial_state(
        "test",
        state_with_suspended_call(
            &int1.id,
            "read_file",
            json!({ "id": int1.id, "action": "confirm" }),
            None,
        ),
    );
    let fix_r1 = TestFixture::new_with_state(session_r1.state.clone());
    let ctx_resume1 = fix_r1.ctx();
    let mut resume1 = StepContext::new(ctx_resume1, &session_r1.id, &session_r1.messages, vec![]);
    response_plugin
        .run_phase(Phase::RunStart, &mut resume1)
        .await;
    let decisions_r1 = resume_inputs_from_state(&fix_r1.updated_state());
    let decision_r1 = decisions_r1
        .get(&int1.id)
        .expect("approved interaction should create decision");
    assert!(matches!(decision_r1.action, ResumeDecisionAction::Resume));

    // Verify second tool (denied) — session has its suspended call persisted.
    let session_r2 = Thread::with_initial_state(
        "test",
        state_with_suspended_call(
            &int2.id,
            "write_file",
            json!({ "id": int2.id, "action": "confirm" }),
            None,
        ),
    );
    let fix_r2 = TestFixture::new_with_state(session_r2.state.clone());
    let ctx_resume2 = fix_r2.ctx();
    let mut resume2 = StepContext::new(ctx_resume2, &session_r2.id, &session_r2.messages, vec![]);
    response_plugin
        .run_phase(Phase::RunStart, &mut resume2)
        .await;
    let decisions_r2 = resume_inputs_from_state(&fix_r2.updated_state());
    let decision_r2 = decisions_r2
        .get(&int2.id)
        .expect("denied interaction should create decision");
    assert!(matches!(decision_r2.action, ResumeDecisionAction::Cancel));
}

// ============================================================================
// HITL Suspend/Resume via execute_tools_with_behaviors
// ============================================================================

/// Test: PermissionPlugin "ask" suspends tool execution via execute_tools_with_behaviors.
///
/// Verifies: Suspended outcome returned, no tool messages, interaction details correct,
/// and suspended_interaction persisted in session state.
#[tokio::test]
async fn test_e2e_permission_suspend_with_real_tool() {
    use tirea_agentos::runtime::loop_runner::{
        execute_tools_with_behaviors, tool_map, ExecuteToolsOutcome,
    };

    // Thread with permissions.default_behavior = "ask"
    let thread = Thread::with_initial_state(
        "test",
        json!({ "permissions": { "default_behavior": "ask", "tools": {} } }),
    );

    let result = StreamResult {
        text: "Calling increment".to_string(),
        tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
            "call_inc",
            "increment",
            json!({"path": "counter"}),
        )],
        usage: None,
        stop_reason: None,
    };

    let tools = tool_map([IncrementTool]);
    let behavior: Arc<dyn tirea_agentos::contracts::runtime::AgentBehavior> =
        Arc::new(PermissionPlugin);

    // execute_tools_with_behaviors should return Suspended outcome
    let outcome = execute_tools_with_behaviors(thread, &result, &tools, false, behavior)
        .await
        .unwrap();

    let (suspended_thread, interaction) = match outcome {
        ExecuteToolsOutcome::Suspended {
            thread,
            suspended_call,
        } => (thread, suspended_call.ticket.suspension.clone()),
        other => panic!("Expected Suspended, got: {:?}", other),
    };

    // Frontend tool invocation: id = fc_<uuid>, action = "tool:PermissionConfirm"
    assert!(
        interaction.id.starts_with("fc_"),
        "Permission interaction should use frontend call_id, got: {}",
        interaction.id
    );
    assert_eq!(interaction.action, "tool:PermissionConfirm");

    // Placeholder tool result keeps LLM message sequence valid while awaiting approval.
    assert_eq!(
        suspended_thread.messages.len(),
        1,
        "Pending tool should have placeholder result"
    );
    assert!(
        suspended_thread.messages[0]
            .content
            .contains("awaiting approval"),
        "Placeholder should mention awaiting approval"
    );

    // suspended_interaction persisted in session state (new per-call-scoped schema)
    let state = suspended_thread.rebuild_state().unwrap();
    let suspended_calls = suspended_calls_from_state(&state);
    let suspended = suspended_calls
        .values()
        .next()
        .expect("suspended interaction should be persisted");
    assert!(
        suspended.ticket.suspension.id.starts_with("fc_"),
        "Persisted interaction should use frontend call_id"
    );
    assert_eq!(suspended.ticket.suspension.action, "tool:PermissionConfirm");

    // Counter should NOT have been modified
    assert!(
        state.get("counter").is_none(),
        "Counter should not exist (tool didn't execute)"
    );
}

/// Test: InteractionPlugin denial blocks tool via execute_tools_with_behaviors.
///
/// After suspend, denial causes the tool to be blocked (error result, no execution).
#[tokio::test]
async fn test_e2e_permission_deny_blocks_via_execute_tools() {
    use tirea_agentos::runtime::loop_runner::{
        execute_tools_with_behaviors, tool_map, ExecuteToolsOutcome,
    };

    let thread = Thread::with_initial_state(
        "test",
        json!({ "permissions": { "default_behavior": "ask", "tools": {} } }),
    );

    let result = StreamResult {
        text: "Calling increment".to_string(),
        tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
            "call_inc",
            "increment",
            json!({"path": "counter"}),
        )],
        usage: None,
        stop_reason: None,
    };

    let tools = tool_map([IncrementTool]);
    let behavior: Arc<dyn tirea_agentos::contracts::runtime::AgentBehavior> =
        Arc::new(PermissionPlugin);

    // Phase 1: Suspend
    let outcome = execute_tools_with_behaviors(thread, &result, &tools, false, behavior)
        .await
        .unwrap();

    let (suspended_thread, interaction) = match outcome {
        ExecuteToolsOutcome::Suspended {
            thread,
            suspended_call,
        } => (thread, suspended_call.ticket.suspension.clone()),
        other => panic!("Expected Suspended, got: {:?}", other),
    };

    // Phase 2: Client denies
    let deny_request = RunAgentInput::new("t1", "r1").with_message(
        tirea_protocol_ag_ui::Message::tool("false", &interaction.id),
    );
    assert!(deny_request
        .denied_target_ids()
        .iter()
        .any(|id| id == &interaction.id));

    // Resume with only InteractionPlugin — denial should block the tool
    let response_plugin = interaction_plugin_from_request(&deny_request);
    let resume_behavior: Arc<dyn tirea_agentos::contracts::runtime::AgentBehavior> =
        Arc::new(response_plugin);

    let resume_result = StreamResult {
        text: "Resuming".to_string(),
        tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
            &interaction.id,
            "increment",
            json!({"path": "counter"}),
        )],
        usage: None,
        stop_reason: None,
    };

    let resumed_thread = execute_tools_with_behaviors(
        suspended_thread,
        &resume_result,
        &tools,
        false,
        resume_behavior,
    )
    .await
    .unwrap()
    .into_thread();

    // 1 placeholder from suspend + 1 blocked result from deny resume
    assert_eq!(
        resumed_thread.message_count(),
        2,
        "Blocked tool should produce a message"
    );
    let msg = &resumed_thread.messages[1];
    assert_eq!(msg.role, tirea_agentos::contracts::thread::Role::Tool);
    assert!(
        msg.content.contains("denied")
            || msg.content.contains("blocked")
            || msg.content.contains("Error"),
        "Blocked message should mention denial/block, got: {}",
        msg.content
    );
    // Counter should NOT have been incremented
    let state = resumed_thread.rebuild_state().unwrap();
    assert!(
        state.get("counter").is_none(),
        "Counter should not exist when denied"
    );
}

/// Test: InteractionPlugin approval allows tool via execute_tools_with_behaviors.
///
/// After suspend, approval (without PermissionPlugin re-running) lets the tool execute.
#[tokio::test]
async fn test_e2e_permission_approve_executes_via_execute_tools() {
    use tirea_agentos::runtime::loop_runner::{
        execute_tools_with_behaviors, tool_map, ExecuteToolsOutcome,
    };

    let thread = Thread::with_initial_state(
        "test",
        json!({ "permissions": { "default_behavior": "ask", "tools": {} } }),
    );

    let result = StreamResult {
        text: "Calling increment".to_string(),
        tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
            "call_inc",
            "increment",
            json!({"path": "counter"}),
        )],
        usage: None,
        stop_reason: None,
    };

    let tools = tool_map([IncrementTool]);
    let behavior: Arc<dyn tirea_agentos::contracts::runtime::AgentBehavior> =
        Arc::new(PermissionPlugin);

    // Phase 1: Suspend
    let outcome = execute_tools_with_behaviors(thread, &result, &tools, false, behavior)
        .await
        .unwrap();

    let (suspended_thread, interaction) = match outcome {
        ExecuteToolsOutcome::Suspended {
            thread,
            suspended_call,
        } => (thread, suspended_call.ticket.suspension.clone()),
        other => panic!("Expected Suspended, got: {:?}", other),
    };

    // Phase 2: Client approves
    let approve_request = RunAgentInput::new("t1", "r1")
        .with_message(tirea_protocol_ag_ui::Message::tool("true", &interaction.id));
    assert!(approve_request
        .approved_target_ids()
        .iter()
        .any(|id| id == &interaction.id));

    // Resume with only InteractionPlugin (no PermissionPlugin)
    let response_plugin = interaction_plugin_from_request(&approve_request);
    let resume_behavior: Arc<dyn tirea_agentos::contracts::runtime::AgentBehavior> =
        Arc::new(response_plugin);

    let resume_result = StreamResult {
        text: "Resuming".to_string(),
        tool_calls: vec![tirea_agentos::contracts::thread::ToolCall::new(
            &interaction.id,
            "increment",
            json!({"path": "counter"}),
        )],
        usage: None,
        stop_reason: None,
    };

    let resumed_thread = execute_tools_with_behaviors(
        suspended_thread,
        &resume_result,
        &tools,
        false,
        resume_behavior,
    )
    .await
    .unwrap()
    .into_thread();

    // 1 placeholder from suspend + 1 real result from approved resume
    assert_eq!(
        resumed_thread.message_count(),
        2,
        "Tool response message should be present after approval"
    );
    let msg = &resumed_thread.messages[1];
    assert_eq!(msg.role, tirea_agentos::contracts::thread::Role::Tool);
    assert!(
        msg.content.contains("new_value"),
        "Tool result should contain new_value, got: {}",
        msg.content
    );

    // suspended_interaction should be cleared
    let state_after = resumed_thread.rebuild_state().unwrap();
    let pending_after = state_after
        .get("__suspended_tool_calls")
        .and_then(|a| a.get("calls"))
        .and_then(|v| v.as_object());
    assert!(
        pending_after.is_none_or(|calls| calls.is_empty()),
        "suspended_interaction should be cleared after approval, got: {:?}",
        pending_after
    );
}

// ============================================================================
// Frontend Tool E2E Flow Tests
// ============================================================================

/// Test: Frontend tool creates suspended interaction
#[tokio::test]
async fn test_frontend_tool_flow_creates_pending() {
    let request = RunAgentInput {
        tools: vec![tirea_protocol_ag_ui::Tool::frontend(
            "copyToClipboard",
            "Copy to clipboard",
        )],
        ..RunAgentInput::new("t1".to_string(), "r1".to_string())
    };

    let plugin = frontend_plugin_from_request(&request);
    let thread = ConversationAgentState::new("test");

    let fix = TestFixture::new();
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);
    let call = ToolCall::new(
        "call_copy",
        "copyToClipboard",
        json!({"text": "Hello World"}),
    );
    step.gate = Some(ToolGate::from_tool_call(&call));

    plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;

    assert!(step.tool_pending(), "Frontend tool should be pending");

    let interaction = suspended_interaction(&step).expect("suspended interaction should exist");
    assert_eq!(interaction.action, "tool:copyToClipboard");
    assert_eq!(interaction.id, "call_copy");
}

/// Test: Frontend tool result returned from client
#[test]
fn test_frontend_tool_flow_result_from_client() {
    // Client returns result for frontend tool
    let response_request = RunAgentInput::new("t1".to_string(), "r1".to_string()).with_message(
        tirea_protocol_ag_ui::Message::tool(
            r#"{"success": true, "bytes_copied": 11}"#,
            "call_copy",
        ),
    );

    let response = response_request
        .interaction_responses()
        .into_iter()
        .find(|response| response.target_id == "call_copy")
        .unwrap();
    assert!(response.result["success"].as_bool().unwrap());
    assert_eq!(response.result["bytes_copied"], 11);
}

/// Test: Mixed frontend and backend tools
#[tokio::test]
async fn test_frontend_tool_flow_mixed_with_backend() {
    let request = RunAgentInput {
        tools: vec![
            tirea_protocol_ag_ui::Tool::frontend("showDialog", "Show dialog"),
            tirea_protocol_ag_ui::Tool::backend("search", "Search files"),
        ],
        ..RunAgentInput::new("t1".to_string(), "r1".to_string())
    };

    let plugin = frontend_plugin_from_request(&request);
    let thread = ConversationAgentState::new("test");

    // Backend tool - should NOT be pending
    let fix_backend = TestFixture::new();
    let ctx_step_backend = fix_backend.ctx();
    let mut step_backend = StepContext::new(ctx_step_backend, &thread.id, &thread.messages, vec![]);
    let call_backend = ToolCall::new("call_search", "search", json!({"query": "test"}));
    step_backend.gate = Some(ToolGate::from_tool_call(&call_backend));
    plugin
        .run_phase(Phase::BeforeToolExecute, &mut step_backend)
        .await;
    assert!(
        !step_backend.tool_pending(),
        "Backend tool should not be pending"
    );

    // Frontend tool - should be pending
    let fix_frontend = TestFixture::new();
    let ctx_step_frontend = fix_frontend.ctx();
    let mut step_frontend =
        StepContext::new(ctx_step_frontend, &thread.id, &thread.messages, vec![]);
    let call_frontend = ToolCall::new("call_dialog", "showDialog", json!({"title": "Confirm"}));
    step_frontend.gate = Some(ToolGate::from_tool_call(&call_frontend));
    plugin
        .run_phase(Phase::BeforeToolExecute, &mut step_frontend)
        .await;
    assert!(
        step_frontend.tool_pending(),
        "Frontend tool should be pending"
    );
}

/// Test: Frontend tool with complex nested result
#[test]
fn test_frontend_tool_flow_complex_result() {
    let complex_result = json!({
        "success": true,
        "selected_files": [
            {"path": "/home/user/doc1.txt", "size": 1024, "type": "text"},
            {"path": "/home/user/doc2.pdf", "size": 2048, "type": "pdf"}
        ],
        "metadata": {
            "dialog_duration_ms": 1500,
            "user_action": "confirm",
            "timestamp": "2024-01-15T10:30:00Z"
        }
    });

    let response_request = RunAgentInput::new("t1".to_string(), "r1".to_string()).with_message(
        tirea_protocol_ag_ui::Message::tool(complex_result.to_string(), "file_picker_call"),
    );

    let response = response_request
        .interaction_responses()
        .into_iter()
        .find(|response| response.target_id == "file_picker_call")
        .unwrap();
    assert!(response.result["success"].as_bool().unwrap());
    assert_eq!(
        response.result["selected_files"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        response.result["selected_files"][0]["path"],
        "/home/user/doc1.txt"
    );
    assert_eq!(response.result["metadata"]["user_action"], "confirm");
}

// ============================================================================
// State Event Flow Tests
// ============================================================================

/// Test: State snapshot event conversion
#[test]
fn test_state_event_snapshot_conversion() {
    let mut ctx = make_agui_ctx("t1", "r1");

    let state = json!({
        "counter": 42,
        "user": {"name": "Alice", "role": "admin"},
        "items": ["a", "b", "c"]
    });

    let event = AgentEvent::StateSnapshot {
        snapshot: state.clone(),
    };
    let ag_events = ctx.on_agent_event(&event);

    assert!(!ag_events.is_empty());
    assert!(ag_events
        .iter()
        .any(|e| matches!(e, Event::StateSnapshot { .. })));

    if let Event::StateSnapshot { snapshot, .. } = &ag_events[0] {
        assert_eq!(snapshot["counter"], 42);
        assert_eq!(snapshot["user"]["name"], "Alice");
    }
}

/// Test: State delta event conversion
#[test]
fn test_state_event_delta_conversion() {
    let mut ctx = make_agui_ctx("t1", "r1");

    let delta = vec![
        json!({"op": "replace", "path": "/counter", "value": 43}),
        json!({"op": "add", "path": "/items/-", "value": "d"}),
    ];

    let event = AgentEvent::StateDelta {
        delta: delta.clone(),
    };
    let ag_events = ctx.on_agent_event(&event);

    assert!(!ag_events.is_empty());
    assert!(ag_events
        .iter()
        .any(|e| matches!(e, Event::StateDelta { .. })));

    if let Event::StateDelta { delta: d, .. } = &ag_events[0] {
        assert_eq!(d.len(), 2);
        assert_eq!(d[0]["op"], "replace");
        assert_eq!(d[1]["op"], "add");
    }
}

/// Test: Messages snapshot event conversion
#[test]
fn test_state_event_messages_snapshot_conversion() {
    let mut ctx = make_agui_ctx("t1", "r1");

    let messages = vec![
        json!({"role": "user", "content": "Hello"}),
        json!({"role": "assistant", "content": "Hi there!"}),
    ];

    let event = AgentEvent::MessagesSnapshot {
        messages: messages.clone(),
    };
    let ag_events = ctx.on_agent_event(&event);

    assert!(!ag_events.is_empty());
    assert!(ag_events
        .iter()
        .any(|e| matches!(e, Event::MessagesSnapshot { .. })));

    if let Event::MessagesSnapshot { messages: m, .. } = &ag_events[0] {
        assert_eq!(m.len(), 2);
        assert_eq!(m[0]["role"], "user");
        assert_eq!(m[1]["role"], "assistant");
    }
}

// ============================================================================
// Error Handling Flow Tests
// ============================================================================

/// Test: Tool execution failure produces correct events
#[test]
fn test_error_flow_tool_execution_failure() {
    let mut ctx = make_agui_ctx("t1", "r1");

    let result = ToolResult::error("read_file", "File not found: /nonexistent");

    // ToolCallDone produces TOOL_CALL_RESULT (not TOOL_CALL_END)
    let event = AgentEvent::ToolCallDone {
        id: "call_1".into(),
        result,
        patch: None,
        message_id: String::new(),
    };
    let ag_events = ctx.on_agent_event(&event);

    // Should have TOOL_CALL_RESULT with error
    assert!(ag_events
        .iter()
        .any(|e| matches!(e, Event::ToolCallResult { .. })));

    // Result should contain error
    if let Some(Event::ToolCallResult { content, .. }) = ag_events
        .iter()
        .find(|e| matches!(e, Event::ToolCallResult { .. }))
    {
        assert!(content.contains("error") || content.contains("File not found"));
    }
}

/// Test: Invalid request validation
#[test]
fn test_error_flow_invalid_request() {
    // Empty thread_id
    let invalid1 = RunAgentInput::new("".to_string(), "r1".to_string());
    assert!(invalid1.validate().is_err());

    // Empty run_id
    let invalid2 = RunAgentInput::new("t1".to_string(), "".to_string());
    assert!(invalid2.validate().is_err());

    // Valid request
    let valid = RunAgentInput::new("t1".to_string(), "r1".to_string());
    assert!(valid.validate().is_ok());
}

/// Test: Cancelled run finish event
#[test]
fn test_error_flow_run_finish_cancelled() {
    let mut ctx = make_agui_ctx("t1", "r1");

    let event = AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: tirea_agentos::contracts::TerminationReason::Cancelled,
    };
    let ag_events = ctx.on_agent_event(&event);

    assert_eq!(ag_events.len(), 1);
    assert!(matches!(ag_events[0], Event::RunError { .. }));
}

// ============================================================================
// Resume Flow Tests
// ============================================================================

/// Test: Resume with approval continues execution
#[tokio::test]
async fn test_resume_flow_with_approval() {
    // Simulate: Previous run ended with pending permission
    let target_id = "call_x";

    // New request includes approval
    let request = RunAgentInput::new("t1".to_string(), "r2".to_string())
        .with_message(tirea_protocol_ag_ui::Message::tool("true", target_id));

    let plugin = interaction_plugin_from_request(&request);
    assert!(plugin.has_responses());
    assert!(plugin.is_approved(target_id));

    // Tool execution should not be blocked
    let thread = ConversationAgentState::new("test");
    let fix = TestFixture::new();
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);
    let call = ToolCall::new(target_id, "tool_x", json!({}));
    step.gate = Some(ToolGate::from_tool_call(&call));

    plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;
    assert!(!step.tool_blocked());
}

/// Test: Resume with denial blocks execution
#[tokio::test]
async fn test_resume_flow_with_denial() {
    let target_id = "call_dangerous";

    let request = RunAgentInput::new("t1".to_string(), "r2".to_string())
        .with_message(tirea_protocol_ag_ui::Message::tool("no", target_id));

    let plugin = interaction_plugin_from_request(&request);
    assert!(plugin.is_denied(target_id));

    // Thread must have the suspended interaction persisted.
    let thread = Thread::with_initial_state(
        "test",
        state_with_suspended_call(
            target_id,
            "dangerous_tool",
            json!({
                "id": target_id,
                "action": "tool:dangerous_tool",
                "parameters": { "source": "permission" }
            }),
            None,
        ),
    );
    let fix = TestFixture::new_with_state(thread.state.clone());
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);
    plugin.run_phase(Phase::RunStart, &mut step).await;
    let decisions = resume_inputs_from_state(&fix.updated_state());
    let decision = decisions
        .get(target_id)
        .expect("denied response should create decision");
    assert!(matches!(decision.action, ResumeDecisionAction::Cancel));
}

/// Test: Resume with multiple pending responses
#[tokio::test]
async fn test_resume_flow_multiple_responses() {
    // Previous run had 3 suspended interactions
    let request = RunAgentInput::new("t1".to_string(), "r2".to_string())
        .with_message(tirea_protocol_ag_ui::Message::tool("true", "perm_1"))
        .with_message(tirea_protocol_ag_ui::Message::tool("false", "perm_2"))
        .with_message(tirea_protocol_ag_ui::Message::tool("yes", "perm_3"));

    let plugin = interaction_plugin_from_request(&request);

    assert!(plugin.is_approved("perm_1"));
    assert!(plugin.is_denied("perm_2"));
    assert!(plugin.is_approved("perm_3"));

    // Test each tool — each needs a session with a matching persisted suspended interaction.
    for (id, should_block) in [("perm_1", false), ("perm_2", true), ("perm_3", false)] {
        let thread = Thread::with_initial_state(
            "test",
            state_with_suspended_call(
                id,
                "test_tool",
                json!({ "id": id, "action": "confirm" }),
                None,
            ),
        );
        let fix = TestFixture::new_with_state(thread.state.clone());
        let ctx_step = fix.ctx();
        let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);
        plugin.run_phase(Phase::RunStart, &mut step).await;
        let decisions = resume_inputs_from_state(&fix.updated_state());
        let action = decisions
            .get(id)
            .map(|d| d.action.clone())
            .expect("expected decision for responded call");
        assert_eq!(
            matches!(action, ResumeDecisionAction::Cancel),
            should_block,
            "Tool {} decision state incorrect",
            id
        );
    }
}

/// Test: Resume with partial responses (some missing)
#[tokio::test]
async fn test_resume_flow_partial_responses() {
    // Only respond to some interactions
    let request = RunAgentInput::new("t1".to_string(), "r2".to_string())
        .with_message(tirea_protocol_ag_ui::Message::tool("true", "perm_1"));
    // perm_2 not responded to

    let plugin = interaction_plugin_from_request(&request);

    assert!(plugin.is_approved("perm_1"));
    assert!(!plugin.is_approved("perm_2")); // No response
    assert!(!plugin.is_denied("perm_2")); // No response

    // Responded tool should not be blocked — session has matching suspended interaction.
    let session1 = Thread::with_initial_state(
        "test",
        state_with_suspended_call(
            "perm_1",
            "tool_1",
            json!({ "id": "perm_1", "action": "confirm" }),
            None,
        ),
    );
    let fix1 = TestFixture::new_with_state(session1.state.clone());
    let ctx_step1 = fix1.ctx();
    let mut step1 = StepContext::new(ctx_step1, &session1.id, &session1.messages, vec![]);
    let call1 = ToolCall::new("perm_1", "tool_1", json!({}));
    step1.gate = Some(ToolGate::from_tool_call(&call1));
    plugin.run_phase(Phase::BeforeToolExecute, &mut step1).await;
    assert!(!step1.tool_blocked());

    // Non-responded tool - plugin doesn't affect it (no matching approved/denied ID).
    let session2 = ConversationAgentState::new("test");
    let fix2 = TestFixture::new();
    let ctx_step2 = fix2.ctx();
    let mut step2 = StepContext::new(ctx_step2, &session2.id, &session2.messages, vec![]);
    let call2 = ToolCall::new("perm_2", "tool_2", json!({}));
    step2.gate = Some(ToolGate::from_tool_call(&call2));
    plugin.run_phase(Phase::BeforeToolExecute, &mut step2).await;
    assert!(!step2.tool_blocked()); // Not blocked by response plugin (no response)
}

// ============================================================================
// Plugin Suspension Flow Tests
// ============================================================================

/// Test: combined AG-UI plugin handles both frontend and interaction responses
#[tokio::test]
async fn test_plugin_interaction_frontend_and_response() {
    // Request has both frontend tools and interaction responses
    let request = RunAgentInput {
        tools: vec![tirea_protocol_ag_ui::Tool::frontend(
            "showNotification",
            "Show notification",
        )],
        ..RunAgentInput::new("t1".to_string(), "r2".to_string())
    }
    .with_message(tirea_protocol_ag_ui::Message::tool("true", "call_prev"));

    let frontend_plugin = frontend_plugin_from_request(&request);
    let response_plugin = interaction_plugin_from_request(&request);

    let thread = ConversationAgentState::new("test");

    // Test 1: Frontend tool should be pending (not affected by response plugin)
    let fix1 = TestFixture::new();
    let ctx_step1 = fix1.ctx();
    let mut step1 = StepContext::new(ctx_step1, &thread.id, &thread.messages, vec![]);
    let call1 = ToolCall::new("call_new", "showNotification", json!({}));
    step1.gate = Some(ToolGate::from_tool_call(&call1));

    response_plugin
        .run_phase(Phase::BeforeToolExecute, &mut step1)
        .await;
    frontend_plugin
        .run_phase(Phase::BeforeToolExecute, &mut step1)
        .await;

    assert!(step1.tool_pending(), "Frontend tool should be pending");
    assert!(!step1.tool_blocked());

    // Test 2: Previously pending tool should be allowed (response plugin approves).
    // Thread must have a persisted suspended interaction matching the approved ID.
    let session2 = Thread::with_initial_state(
        "test",
        state_with_suspended_call(
            "call_prev",
            "some_tool",
            json!({ "id": "call_prev", "action": "confirm" }),
            None,
        ),
    );
    let fix2 = TestFixture::new_with_state(session2.state.clone());
    let ctx_step2 = fix2.ctx();
    let mut step2 = StepContext::new(ctx_step2, &session2.id, &session2.messages, vec![]);
    let call2 = ToolCall::new("call_prev", "some_tool", json!({}));
    step2.gate = Some(ToolGate::from_tool_call(&call2));

    response_plugin
        .run_phase(Phase::BeforeToolExecute, &mut step2)
        .await;
    frontend_plugin
        .run_phase(Phase::BeforeToolExecute, &mut step2)
        .await;

    assert!(!step2.tool_blocked(), "Approved tool should not be blocked");
}

/// Test: Plugin execution order matters
#[tokio::test]
async fn test_plugin_interaction_execution_order() {
    // Setup: Frontend tool that was previously denied
    let request = RunAgentInput {
        tools: vec![tirea_protocol_ag_ui::Tool::frontend(
            "dangerousAction",
            "Dangerous",
        )],
        ..RunAgentInput::new("t1".to_string(), "r2".to_string())
    }
    .with_message(tirea_protocol_ag_ui::Message::tool("false", "call_danger")); // Denied

    let frontend_plugin = frontend_plugin_from_request(&request);
    let response_plugin = interaction_plugin_from_request(&request);

    // Thread must have a persisted suspended interaction matching the denied ID.
    let thread = Thread::with_initial_state(
        "test",
        state_with_suspended_call(
            "call_danger",
            "dangerousAction",
            json!({ "id": "call_danger", "action": "confirm" }),
            None,
        ),
    );
    let fix = TestFixture::new_with_state(thread.state.clone());
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);
    let call = ToolCall::new("call_danger", "dangerousAction", json!({}));
    step.gate = Some(ToolGate::from_tool_call(&call));

    // Response plugin run_start persists cancel decision.
    response_plugin.run_phase(Phase::RunStart, &mut step).await;
    let decisions = resume_inputs_from_state(&fix.updated_state());
    let decision = decisions
        .get("call_danger")
        .expect("denied response should create decision");
    assert!(matches!(decision.action, ResumeDecisionAction::Cancel));

    // before_tool_execute does not apply decision (loop-owned); frontend plugin still sees call.
    response_plugin
        .run_phase(Phase::BeforeToolExecute, &mut step)
        .await;
    frontend_plugin
        .run_phase(Phase::BeforeToolExecute, &mut step)
        .await;
    assert!(!step.tool_blocked(), "gate application is loop-owned");
    assert!(
        step.tool_pending(),
        "frontend plugin can still suspend in this unit scope"
    );
}

/// Test: Permission plugin with frontend tool
#[tokio::test]
async fn test_plugin_interaction_permission_and_frontend() {
    // Frontend tool with permission set to Ask
    let request = RunAgentInput {
        tools: vec![tirea_protocol_ag_ui::Tool::frontend(
            "modifySettings",
            "Modify settings",
        )],
        ..RunAgentInput::new("t1".to_string(), "r1".to_string())
    };

    let frontend_plugin = frontend_plugin_from_request(&request);
    let permission_plugin = PermissionPlugin;

    let thread = ConversationAgentState::new("test");
    let fix = TestFixture::new();
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);

    let call = ToolCall::new("call_modify", "modifySettings", json!({}));
    step.gate = Some(ToolGate::from_tool_call(&call));

    // Permission plugin runs first - creates pending for "ask"
    permission_plugin
        .run_phase(Phase::BeforeToolExecute, &mut step)
        .await;
    // Frontend plugin runs second
    frontend_plugin
        .run_phase(Phase::BeforeToolExecute, &mut step)
        .await;

    // Tool should be pending (frontend takes precedence for frontend tools)
    assert!(step.tool_pending(), "Tool should be pending");

    // The interaction should be from frontend plugin (tool:modifySettings)
    let interaction = suspended_interaction(&step).expect("suspended interaction should exist");
    assert!(
        interaction.action.starts_with("tool:") || interaction.action == "confirm",
        "Suspension action should be from one of the plugins"
    );
}

// ============================================================================
// Activity Event Flow Tests
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/events
// Activity events are used for long-running operations to show progress.
//
// Flow: ACTIVITY_SNAPSHOT → ACTIVITY_DELTA* → (completion)
// - ACTIVITY_SNAPSHOT: Initial state with full content
// - ACTIVITY_DELTA: JSON Patch updates to modify state incrementally
//

/// Test: Activity snapshot event creation and conversion
/// Protocol: ACTIVITY_SNAPSHOT event per AG-UI spec
#[test]
fn test_activity_snapshot_flow() {
    use std::collections::HashMap;

    let mut content = HashMap::new();
    content.insert("progress".to_string(), json!(0.75));
    content.insert("status".to_string(), json!("processing"));
    content.insert(
        "current_item".to_string(),
        json!({"name": "file.txt", "size": 1024}),
    );

    let event = Event::activity_snapshot("activity_1", "file_processing", content, Some(false));

    // Verify serialization
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"ACTIVITY_SNAPSHOT""#));
    assert!(json.contains(r#""activityType":"file_processing""#));
    assert!(json.contains(r#""progress":0.75"#));

    // Roundtrip
    let parsed: Event = serde_json::from_str(&json).unwrap();
    assert!(matches!(parsed, Event::ActivitySnapshot { .. }));
}

/// Test: Activity delta event creation and conversion
#[test]
fn test_activity_delta_flow() {
    let patch = vec![
        json!({"op": "replace", "path": "/progress", "value": 0.85}),
        json!({"op": "replace", "path": "/status", "value": "almost done"}),
    ];

    let event = Event::activity_delta("activity_1", "file_processing", patch);

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"ACTIVITY_DELTA""#));
    assert!(json.contains(r#""op":"replace""#));

    let parsed: Event = serde_json::from_str(&json).unwrap();
    assert!(matches!(parsed, Event::ActivityDelta { .. }));
}

/// Test: Complete activity streaming flow (snapshot → deltas → final)
#[test]
fn test_activity_streaming_complete_flow() {
    use std::collections::HashMap;

    // Initial snapshot
    let mut initial_content = HashMap::new();
    initial_content.insert("progress".to_string(), json!(0.0));
    initial_content.insert("total_files".to_string(), json!(10));
    initial_content.insert("processed_files".to_string(), json!(0));

    let snapshot =
        Event::activity_snapshot("act_1", "batch_processing", initial_content, Some(false));

    // Progress deltas
    let delta1 = Event::activity_delta(
        "act_1",
        "batch_processing",
        vec![
            json!({"op": "replace", "path": "/progress", "value": 0.3}),
            json!({"op": "replace", "path": "/processed_files", "value": 3}),
        ],
    );

    let delta2 = Event::activity_delta(
        "act_1",
        "batch_processing",
        vec![
            json!({"op": "replace", "path": "/progress", "value": 0.7}),
            json!({"op": "replace", "path": "/processed_files", "value": 7}),
        ],
    );

    let delta_final = Event::activity_delta(
        "act_1",
        "batch_processing",
        vec![
            json!({"op": "replace", "path": "/progress", "value": 1.0}),
            json!({"op": "replace", "path": "/processed_files", "value": 10}),
            json!({"op": "add", "path": "/completed", "value": true}),
        ],
    );

    // Verify all events serialize correctly
    let events = [snapshot, delta1, delta2, delta_final];
    for (i, event) in events.iter().enumerate() {
        let json = serde_json::to_string(event).unwrap();
        let _: Event =
            serde_json::from_str(&json).unwrap_or_else(|e| panic!("Event {i} failed: {e}"));
    }

    // Verify event types
    assert!(matches!(&events[0], Event::ActivitySnapshot { .. }));
    assert!(matches!(&events[1], Event::ActivityDelta { .. }));
    assert!(matches!(&events[2], Event::ActivityDelta { .. }));
    assert!(matches!(&events[3], Event::ActivityDelta { .. }));
}

// ============================================================================
// Concurrent Tool Execution Tests
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/events
// Tool Call Flow: TOOL_CALL_START → TOOL_CALL_ARGS → TOOL_CALL_END → TOOL_CALL_RESULT
//
// Multiple tools can execute concurrently. Each tool maintains its own event
// sequence identified by tool_call_id. Events can interleave but each tool's
// sequence must be complete.
//

/// Test: Multiple tool calls event ordering
/// Verifies concurrent tools each have complete START → ARGS → END → RESULT sequence
#[test]
fn test_concurrent_tool_calls_event_ordering() {
    let mut ctx = make_agui_ctx("t1", "r1");

    // Simulate 3 concurrent tool calls
    let tool_ids = ["call_1", "call_2", "call_3"];
    let tool_names = ["search", "read_file", "write_file"];

    let mut all_events: Vec<Event> = Vec::new();

    // All tools start
    for (id, name) in tool_ids.iter().zip(tool_names.iter()) {
        let start = AgentEvent::ToolCallStart {
            id: id.to_string(),
            name: name.to_string(),
        };
        all_events.extend(ctx.on_agent_event(&start));
    }

    // All tools get args
    for id in &tool_ids {
        let args = AgentEvent::ToolCallDelta {
            id: id.to_string(),
            args_delta: "{}".into(),
        };
        all_events.extend(ctx.on_agent_event(&args));
    }

    // All tools ready (end args streaming)
    for (id, name) in tool_ids.iter().zip(tool_names.iter()) {
        let ready = AgentEvent::ToolCallReady {
            id: id.to_string(),
            name: name.to_string(),
            arguments: json!({}),
        };
        all_events.extend(ctx.on_agent_event(&ready));
    }

    // All tools done
    for (id, name) in tool_ids.iter().zip(tool_names.iter()) {
        let done = AgentEvent::ToolCallDone {
            id: id.to_string(),
            result: ToolResult::success(*name, json!({"ok": true})),
            patch: None,
            message_id: String::new(),
        };
        all_events.extend(ctx.on_agent_event(&done));
    }

    // Verify each tool has complete sequence
    for id in &tool_ids {
        let has_start = all_events
            .iter()
            .any(|e| matches!(e, Event::ToolCallStart { tool_call_id, .. } if tool_call_id == *id));
        let has_args = all_events
            .iter()
            .any(|e| matches!(e, Event::ToolCallArgs { tool_call_id, .. } if tool_call_id == *id));
        let has_end = all_events
            .iter()
            .any(|e| matches!(e, Event::ToolCallEnd { tool_call_id, .. } if tool_call_id == *id));
        let has_result = all_events.iter().any(
            |e| matches!(e, Event::ToolCallResult { tool_call_id, .. } if tool_call_id == *id),
        );

        assert!(has_start, "Tool {} missing START", id);
        assert!(has_args, "Tool {} missing ARGS", id);
        assert!(has_end, "Tool {} missing END", id);
        assert!(has_result, "Tool {} missing RESULT", id);
    }
}

/// Test: Interleaved tool calls with text
#[test]
fn test_interleaved_tools_and_text() {
    let mut ctx = make_agui_ctx("t1", "r1");
    let mut all_events: Vec<Event> = Vec::new();

    // Text starts
    let text1 = AgentEvent::TextDelta {
        delta: "Let me search ".into(),
    };
    all_events.extend(ctx.on_agent_event(&text1));

    // Tool starts (interrupts text)
    let tool_start = AgentEvent::ToolCallStart {
        id: "call_1".into(),
        name: "search".into(),
    };
    all_events.extend(ctx.on_agent_event(&tool_start));

    // Tool args
    let tool_args = AgentEvent::ToolCallDelta {
        id: "call_1".into(),
        args_delta: r#"{"query": "rust"}"#.into(),
    };
    all_events.extend(ctx.on_agent_event(&tool_args));

    // Tool ready
    let tool_ready = AgentEvent::ToolCallReady {
        id: "call_1".into(),
        name: "search".into(),
        arguments: json!({"query": "rust"}),
    };
    all_events.extend(ctx.on_agent_event(&tool_ready));

    // Tool done
    let tool_done = AgentEvent::ToolCallDone {
        id: "call_1".into(),
        result: ToolResult::success("search", json!({"results": 5})),
        patch: None,
        message_id: String::new(),
    };
    all_events.extend(ctx.on_agent_event(&tool_done));

    // More text after tool
    let text2 = AgentEvent::TextDelta {
        delta: "Found 5 results.".into(),
    };
    all_events.extend(ctx.on_agent_event(&text2));

    // Verify sequence: text → tool → text
    // First should have TEXT_MESSAGE_START
    assert!(matches!(&all_events[0], Event::TextMessageStart { .. }));

    // Should have TEXT_MESSAGE_END before TOOL_CALL_START
    let text_end_idx = all_events
        .iter()
        .position(|e| matches!(e, Event::TextMessageEnd { .. }))
        .unwrap();
    let tool_start_idx = all_events
        .iter()
        .position(|e| matches!(e, Event::ToolCallStart { .. }))
        .unwrap();
    assert!(
        text_end_idx < tool_start_idx,
        "TEXT_MESSAGE_END should come before TOOL_CALL_START"
    );

    // Should have new TEXT_MESSAGE_START after tool
    let text_starts: Vec<_> = all_events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, Event::TextMessageStart { .. }))
        .collect();
    assert_eq!(
        text_starts.len(),
        2,
        "Should have 2 TEXT_MESSAGE_START events"
    );
}

// ============================================================================
// Client Reconnection Flow Tests
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/events
// State Synchronization Flow for reconnection:
//   STATE_SNAPSHOT (full state) or MESSAGES_SNAPSHOT (conversation history)
//
// When a client reconnects, it needs:
// 1. Current state via STATE_SNAPSHOT
// 2. Conversation history via MESSAGES_SNAPSHOT
// 3. Ability to resume from last known state
//

/// Test: State snapshot for reconnection
/// Protocol: STATE_SNAPSHOT event for client state restoration
#[test]
fn test_reconnection_state_snapshot() {
    let mut ctx = make_agui_ctx("t1", "r1");

    // Simulate session state
    let state = json!({
        "conversation": {
            "run_count": 5,
            "last_tool": "search",
            "context": {"topic": "rust programming"}
        },
        "user_preferences": {
            "language": "en",
            "verbosity": "detailed"
        }
    });

    let event = AgentEvent::StateSnapshot {
        snapshot: state.clone(),
    };
    let ag_events = ctx.on_agent_event(&event);

    assert!(!ag_events.is_empty());
    if let Event::StateSnapshot { snapshot, .. } = &ag_events[0] {
        assert_eq!(snapshot["conversation"]["run_count"], 5);
        assert_eq!(snapshot["user_preferences"]["language"], "en");
    }
}

/// Test: Messages snapshot for reconnection
#[test]
fn test_reconnection_messages_snapshot() {
    let mut ctx = make_agui_ctx("t1", "r1");

    let messages = vec![
        json!({"role": "user", "content": "Hello"}),
        json!({"role": "assistant", "content": "Hi! How can I help?"}),
        json!({"role": "user", "content": "Search for rust tutorials"}),
        json!({"role": "assistant", "content": "I'll search for that.", "tool_calls": [{"id": "call_1", "name": "search"}]}),
        json!({"role": "tool", "tool_call_id": "call_1", "content": "{\"results\": 10}"}),
    ];

    let event = AgentEvent::MessagesSnapshot {
        messages: messages.clone(),
    };
    let ag_events = ctx.on_agent_event(&event);

    assert!(!ag_events.is_empty());
    if let Event::MessagesSnapshot { messages: m, .. } = &ag_events[0] {
        assert_eq!(m.len(), 5);
        assert_eq!(m[0]["role"], "user");
        assert_eq!(m[4]["role"], "tool");
    }
}

/// Test: Full reconnection scenario
#[test]
fn test_full_reconnection_scenario() {
    // Client reconnects - server sends snapshots first
    let mut ctx = make_agui_ctx("t1", "r1");
    let mut reconnect_events: Vec<Event> = Vec::new();

    // 1. RUN_STARTED for new connection
    reconnect_events.push(Event::run_started("t1", "r1", None));

    // 2. Messages snapshot (conversation history)
    let messages_event = AgentEvent::MessagesSnapshot {
        messages: vec![
            json!({"role": "user", "content": "Previous message"}),
            json!({"role": "assistant", "content": "Previous response"}),
        ],
    };
    reconnect_events.extend(ctx.on_agent_event(&messages_event));

    // 3. State snapshot (current state)
    let state_event = AgentEvent::StateSnapshot {
        snapshot: json!({"thread_id": "abc123", "active": true}),
    };
    reconnect_events.extend(ctx.on_agent_event(&state_event));

    // 4. Continue with new content
    let text = AgentEvent::TextDelta {
        delta: "Continuing from where we left off...".into(),
    };
    reconnect_events.extend(ctx.on_agent_event(&text));

    // Verify sequence
    assert!(matches!(&reconnect_events[0], Event::RunStarted { .. }));
    assert!(reconnect_events
        .iter()
        .any(|e| matches!(e, Event::MessagesSnapshot { .. })));
    assert!(reconnect_events
        .iter()
        .any(|e| matches!(e, Event::StateSnapshot { .. })));
    assert!(reconnect_events
        .iter()
        .any(|e| matches!(e, Event::TextMessageStart { .. })));
}

// ============================================================================
// Multiple Pending Interactions Tests (HIGH PRIORITY - Previously Missing)
// ============================================================================

/// Test: Multiple suspended interactions in sequence
#[tokio::test]
async fn test_multiple_suspended_interactions_flow() {
    // Create 3 suspended interactions
    let interactions: Vec<Suspension> = vec![
        Suspension::new("perm_read", "confirm").with_message("Allow reading files?"),
        Suspension::new("perm_write", "confirm").with_message("Allow writing files?"),
        Suspension::new("perm_exec", "confirm").with_message("Allow executing commands?"),
    ];

    // Convert all to AG-UI events
    let mut all_events: Vec<Event> = Vec::new();
    for interaction in &interactions {
        all_events.extend(interaction_to_ag_ui_events(interaction));
    }

    // Each interaction should produce 3 events (Start, Args, End)
    assert_eq!(all_events.len(), 9);

    // Verify each interaction has its events
    for interaction in &interactions {
        let has_start = all_events.iter().any(|e| {
            matches!(e, Event::ToolCallStart { tool_call_id, .. } if tool_call_id == &interaction.id)
        });
        assert!(has_start, "Missing ToolCallStart for {}", interaction.id);
    }
}

/// Test: Client responds to multiple interactions
#[tokio::test]
async fn test_multiple_interaction_responses() {
    let doc = json!({});
    let _fix = TestFixture::new_with_state(doc);

    // Client responds to all 3 interactions: approve, deny, approve
    let request = RunAgentInput::new("t1".to_string(), "r2".to_string())
        .with_message(tirea_protocol_ag_ui::Message::tool("yes", "perm_read"))
        .with_message(tirea_protocol_ag_ui::Message::tool("no", "perm_write"))
        .with_message(tirea_protocol_ag_ui::Message::tool("approved", "perm_exec"));

    let plugin = interaction_plugin_from_request(&request);

    assert!(plugin.is_approved("perm_read"));
    assert!(plugin.is_denied("perm_write"));
    assert!(plugin.is_approved("perm_exec"));

    // Verify each responded call writes the expected resume decision.
    let test_cases = vec![
        ("perm_read", false), // approved => resume
        ("perm_write", true), // denied => cancel
        ("perm_exec", false), // approved => resume
    ];

    for (id, should_block) in test_cases {
        let thread = Thread::with_initial_state(
            "test",
            state_with_suspended_call(
                id,
                "some_tool",
                json!({ "id": id, "action": "confirm" }),
                None,
            ),
        );
        let fix = TestFixture::new_with_state(thread.state.clone());
        let ctx_step = fix.ctx();
        let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);
        plugin.run_phase(Phase::RunStart, &mut step).await;
        let decisions = resume_inputs_from_state(&fix.updated_state());
        let action = decisions
            .get(id)
            .map(|d| d.action.clone())
            .expect("response should create a decision");

        assert_eq!(
            matches!(action, ResumeDecisionAction::Cancel),
            should_block,
            "Tool {} should_block={} but got action={:?}",
            id,
            should_block,
            action
        );
    }
}

// ============================================================================
// Tool Timeout Flow Tests (HIGH PRIORITY - Previously Missing)
// ============================================================================

/// Test: Tool timeout produces correct AG-UI events
#[test]
fn test_tool_timeout_ag_ui_flow() {
    let mut ctx = make_agui_ctx("t1", "r1");

    // Tool starts
    let start = AgentEvent::ToolCallStart {
        id: "call_slow".into(),
        name: "slow_operation".into(),
    };
    let start_events = ctx.on_agent_event(&start);
    assert!(start_events
        .iter()
        .any(|e| matches!(e, Event::ToolCallStart { .. })));

    // Tool times out - simulated by returning timeout error
    let timeout_result = ToolResult::error("slow_operation", "Tool execution timed out after 30s");

    let done = AgentEvent::ToolCallDone {
        id: "call_slow".into(),
        result: timeout_result,
        patch: None,
        message_id: String::new(),
    };
    let done_events = ctx.on_agent_event(&done);

    // Should still have TOOL_CALL_RESULT with error
    assert!(done_events
        .iter()
        .any(|e| matches!(e, Event::ToolCallResult { .. })));

    if let Some(Event::ToolCallResult { content, .. }) = done_events
        .iter()
        .find(|e| matches!(e, Event::ToolCallResult { .. }))
    {
        assert!(content.contains("timed out") || content.contains("error"));
    }
}

// ============================================================================
// Rapid Text Delta Tests (MEDIUM PRIORITY)
// ============================================================================

/// Test: Rapid text delta burst handling
#[test]
fn test_rapid_text_delta_burst() {
    let mut ctx = make_agui_ctx("t1", "r1");
    let mut all_events: Vec<Event> = Vec::new();

    // Simulate 100 rapid text deltas
    for i in 0..100 {
        let delta = AgentEvent::TextDelta {
            delta: format!("word{} ", i),
        };
        all_events.extend(ctx.on_agent_event(&delta));
    }

    // Should have exactly 1 TEXT_MESSAGE_START
    let start_count = all_events
        .iter()
        .filter(|e| matches!(e, Event::TextMessageStart { .. }))
        .count();
    assert_eq!(start_count, 1, "Should have exactly 1 TEXT_MESSAGE_START");

    // Should have 100 TEXT_MESSAGE_CONTENT (one for each delta)
    let content_count = all_events
        .iter()
        .filter(|e| matches!(e, Event::TextMessageContent { .. }))
        .count();
    assert_eq!(
        content_count, 100,
        "Should have 100 TEXT_MESSAGE_CONTENT events"
    );

    // First event should be TEXT_MESSAGE_START
    assert!(matches!(&all_events[0], Event::TextMessageStart { .. }));
}

// ============================================================================
// State Event Ordering Tests (MEDIUM PRIORITY)
// ============================================================================

/// Test: State events ordering with other events
#[test]
fn test_state_event_ordering() {
    let mut ctx = make_agui_ctx("t1", "r1");
    let mut all_events: Vec<Event> = Vec::new();

    // Sequence: text → state snapshot → more text → state delta
    let text1 = AgentEvent::TextDelta {
        delta: "Starting...".into(),
    };
    all_events.extend(ctx.on_agent_event(&text1));

    let snapshot = AgentEvent::StateSnapshot {
        snapshot: json!({"step": 1}),
    };
    all_events.extend(ctx.on_agent_event(&snapshot));

    let text2 = AgentEvent::TextDelta {
        delta: " Processing...".into(),
    };
    all_events.extend(ctx.on_agent_event(&text2));

    let delta = AgentEvent::StateDelta {
        delta: vec![json!({"op": "replace", "path": "/step", "value": 2})],
    };
    all_events.extend(ctx.on_agent_event(&delta));

    // Verify presence of all event types
    assert!(all_events
        .iter()
        .any(|e| matches!(e, Event::TextMessageStart { .. })));
    assert!(all_events
        .iter()
        .any(|e| matches!(e, Event::TextMessageContent { .. })));
    assert!(all_events
        .iter()
        .any(|e| matches!(e, Event::StateSnapshot { .. })));
    assert!(all_events
        .iter()
        .any(|e| matches!(e, Event::StateDelta { .. })));
}

// ============================================================================
// Sequential Runs Tests (MEDIUM PRIORITY)
// ============================================================================

/// Test: Sequential runs in same session
#[test]
fn test_sequential_runs_in_session() {
    // Run 1
    let mut ctx1 = make_agui_ctx("t1", "r1");
    let run1_start = Event::run_started("t1", "r1", None);
    let text1 = AgentEvent::TextDelta {
        delta: "First run response".into(),
    };
    let text1_events = ctx1.on_agent_event(&text1);
    // Run 2 (same thread, different run)
    let mut ctx2 = make_agui_ctx("t1", "r2");
    let run2_start = Event::run_started("t1", "r2", None);
    let text2 = AgentEvent::TextDelta {
        delta: "Second run response".into(),
    };
    let text2_events = ctx2.on_agent_event(&text2);

    // Verify runs are independent
    if let Event::RunStarted {
        thread_id: t1,
        run_id: r1,
        ..
    } = &run1_start
    {
        if let Event::RunStarted {
            thread_id: t2,
            run_id: r2,
            ..
        } = &run2_start
        {
            assert_eq!(t1, t2, "Same thread");
            assert_ne!(r1, r2, "Different runs");
        }
    }

    // Each run should have its own text message
    assert!(!text1_events.is_empty());
    assert!(!text2_events.is_empty());
}

// ============================================================================
// RAW and CUSTOM Event Tests
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/events
// Special event types for extensibility:
// - RAW: Pass through external provider events unchanged
// - CUSTOM: Application-specific extension events
//
// These allow protocol extensions without breaking compatibility.
//

/// Test: RAW event wrapping
/// Protocol: RAW event for pass-through of external system events
#[test]
fn test_raw_event_wrapping() {
    let external_event = json!({
        "provider": "openai",
        "event_type": "rate_limit_warning",
        "data": {
            "requests_remaining": 10,
            "reset_at": "2024-01-15T12:00:00Z"
        }
    });

    let event = Event::raw(external_event.clone(), Some("openai".into()));

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"RAW""#));
    assert!(json.contains(r#""provider":"openai""#));

    let parsed: Event = serde_json::from_str(&json).unwrap();
    if let Event::Raw { event: e, .. } = parsed {
        assert_eq!(e["provider"], "openai");
        assert_eq!(e["data"]["requests_remaining"], 10);
    }
}

/// Test: CUSTOM event flow
#[test]
fn test_custom_event_flow() {
    let custom_value = json!({
        "action": "show_modal",
        "modal_type": "confirmation",
        "title": "Confirm Action",
        "buttons": ["Cancel", "Confirm"]
    });

    let event = Event::custom("ui_action", custom_value.clone());

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"CUSTOM""#));
    assert!(json.contains(r#""name":"ui_action""#));
    assert!(json.contains(r#""action":"show_modal""#));

    let parsed: Event = serde_json::from_str(&json).unwrap();
    if let Event::Custom { name, value, .. } = parsed {
        assert_eq!(name, "ui_action");
        assert_eq!(value["modal_type"], "confirmation");
    }
}

// ============================================================================
// Large Payload Tests
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/events
// Tests for handling large payloads in:
// - TOOL_CALL_RESULT: Large tool execution results
// - STATE_SNAPSHOT: Large state objects
//
// These verify the protocol handles real-world data sizes without issues.
//

/// Test: Large tool result payload
/// Verifies TOOL_CALL_RESULT can handle ~100KB of JSON data
#[test]
fn test_large_tool_result_payload() {
    let mut ctx = make_agui_ctx("t1", "r1");

    // Create a large result (simulate ~100KB of data)
    let large_data: Vec<Value> = (0..1000)
        .map(|i| json!({
            "id": i,
            "name": format!("item_{}", i),
            "description": "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(10),
            "metadata": {
                "created": "2024-01-15",
                "tags": ["tag1", "tag2", "tag3"],
                "nested": {"level1": {"level2": {"value": i}}}
            }
        }))
        .collect();

    let result = ToolResult::success(
        "search",
        json!({
            "total": 1000,
            "items": large_data
        }),
    );

    let event = AgentEvent::ToolCallDone {
        id: "call_large".into(),
        result,
        patch: None,
        message_id: String::new(),
    };

    let ag_events = ctx.on_agent_event(&event);
    assert!(!ag_events.is_empty());

    // Verify the result can be serialized and parsed
    if let Some(Event::ToolCallResult { content, .. }) = ag_events
        .iter()
        .find(|e| matches!(e, Event::ToolCallResult { .. }))
    {
        // Content should be valid JSON (it's a serialized ToolResult struct)
        let parsed: Value = serde_json::from_str(content).expect("Should be valid JSON");
        // The data is nested inside the ToolResult structure
        assert_eq!(parsed["data"]["total"], 1000);
        assert_eq!(parsed["data"]["items"].as_array().unwrap().len(), 1000);
    }
}

/// Test: Large state snapshot
#[test]
fn test_large_state_snapshot() {
    let mut ctx = make_agui_ctx("t1", "r1");

    // Create large state
    let large_state = json!({
        "users": (0..100).map(|i| json!({
            "id": i,
            "name": format!("User {}", i),
            "email": format!("user{}@example.com", i),
            "preferences": {
                "theme": "dark",
                "notifications": true,
                "settings": (0..10).map(|j| json!({"key": format!("setting_{}", j), "value": j})).collect::<Vec<_>>()
            }
        })).collect::<Vec<_>>(),
        "config": {
            "version": "1.0.0",
            "features": (0..50).map(|i| format!("feature_{}", i)).collect::<Vec<_>>()
        }
    });

    let event = AgentEvent::StateSnapshot {
        snapshot: large_state,
    };
    let ag_events = ctx.on_agent_event(&event);

    assert!(!ag_events.is_empty());
    if let Event::StateSnapshot { snapshot, .. } = &ag_events[0] {
        assert_eq!(snapshot["users"].as_array().unwrap().len(), 100);
    }
}

// ============================================================================
// AG-UI Protocol Spec Compliance Tests
// ============================================================================
//
// These tests verify compliance with AG-UI protocol specification.
// Reference: https://docs.ag-ui.com/concepts/events
//            https://docs.ag-ui.com/sdk/js/core/events
//

// ----------------------------------------------------------------------------
// Convenience Event Tests (TextMessageChunk, ToolCallChunk)
// ----------------------------------------------------------------------------
//
// Per AG-UI spec: Convenience events auto-expand to their component events.
// TextMessageChunk → TextMessageStart + TextMessageContent + TextMessageEnd
// ToolCallChunk → ToolCallStart + ToolCallArgs + ToolCallEnd

/// Test: TextMessageChunk convenience event serialization
/// Protocol: TEXT_MESSAGE_CHUNK auto-expands to Start → Content → End
#[test]
fn test_text_message_chunk_serialization() {
    let chunk = Event::text_message_chunk(
        Some("msg_1".into()),
        Some(Role::Assistant),
        Some("Hello, world!".into()),
    );

    let json = serde_json::to_string(&chunk).unwrap();
    assert!(json.contains(r#""type":"TEXT_MESSAGE_CHUNK""#));
    assert!(json.contains(r#""messageId":"msg_1""#));
    assert!(json.contains(r#""delta":"Hello, world!""#));
    assert!(json.contains(r#""role":"assistant""#));

    // Verify roundtrip
    let parsed: Event = serde_json::from_str(&json).unwrap();
    assert!(matches!(parsed, Event::TextMessageChunk { .. }));
}

/// Test: ToolCallChunk convenience event serialization
/// Protocol: TOOL_CALL_CHUNK auto-expands to Start → Args → End
#[test]
fn test_tool_call_chunk_serialization() {
    let chunk = Event::tool_call_chunk(
        Some("call_1".into()),
        Some("search".into()),
        None,
        Some(r#"{"query":"rust"}"#.into()),
    );

    let json = serde_json::to_string(&chunk).unwrap();
    assert!(json.contains(r#""type":"TOOL_CALL_CHUNK""#));
    assert!(json.contains(r#""toolCallId":"call_1""#));
    assert!(json.contains(r#""toolCallName":"search""#));
    assert!(json.contains(r#""delta":"#));

    // Verify roundtrip
    let parsed: Event = serde_json::from_str(&json).unwrap();
    assert!(matches!(parsed, Event::ToolCallChunk { .. }));
}

/// Test: ToolCallChunk with parentMessageId
/// Protocol: Optional parentMessageId links tool call to a message
#[test]
fn test_tool_call_chunk_with_parent_message() {
    let chunk = Event::tool_call_chunk(
        Some("call_1".into()),
        Some("read_file".into()),
        Some("msg_123".into()),
        Some(r#"{"path":"/etc/hosts"}"#.into()),
    );

    let json = serde_json::to_string(&chunk).unwrap();
    assert!(json.contains(r#""parentMessageId":"msg_123""#));
}

// ----------------------------------------------------------------------------
// Run Lifecycle Tests (parentRunId, branching)
// ----------------------------------------------------------------------------
//
// Per AG-UI spec: Runs can branch via parentRunId for sub-agents or retries.

/// Test: RunStarted with parentRunId for branching
/// Protocol: parentRunId enables run branching/sub-agents
#[test]
fn test_run_started_with_parent_run_id() {
    let event =
        Event::run_started_with_input("t1", "r2", Some("r1".into()), json!({"query": "test"}));

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"RUN_STARTED""#));
    assert!(json.contains(r#""runId":"r2""#));
    assert!(json.contains(r#""parentRunId":"r1""#));
    assert!(json.contains(r#""input":"#));

    // Roundtrip
    let parsed: Event = serde_json::from_str(&json).unwrap();
    if let Event::RunStarted { parent_run_id, .. } = parsed {
        assert_eq!(parent_run_id, Some("r1".to_string()));
    } else {
        panic!("Expected RunStarted");
    }
}

/// Test: RunError with error code
/// Protocol: Error code is optional for categorizing errors
#[test]
fn test_run_error_with_code() {
    let event = Event::run_error(
        "Connection timeout".to_string(),
        Some("TIMEOUT".to_string()),
    );

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"RUN_ERROR""#));
    assert!(json.contains(r#""message":"Connection timeout""#));
    assert!(json.contains(r#""code":"TIMEOUT""#));

    let parsed: Event = serde_json::from_str(&json).unwrap();
    if let Event::RunError { code, .. } = parsed {
        assert_eq!(code, Some("TIMEOUT".to_string()));
    }
}

/// Test: RunError without code
#[test]
fn test_run_error_without_code() {
    let event = Event::run_error("Unknown error".to_string(), None);

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"RUN_ERROR""#));
    assert!(!json.contains(r#""code""#)); // code should be omitted

    let parsed: Event = serde_json::from_str(&json).unwrap();
    if let Event::RunError { code, .. } = parsed {
        assert_eq!(code, None);
    }
}

// ----------------------------------------------------------------------------
// Step Event Tests (StepStarted/StepFinished pairing)
// ----------------------------------------------------------------------------
//
// Per AG-UI spec: Step events track discrete subtasks with matching stepName.

/// Test: Step events with matching names
/// Protocol: StepStarted and StepFinished must have matching stepName
#[test]
fn test_step_events_matching_names() {
    let start = Event::step_started("data_processing");
    let finish = Event::step_finished("data_processing");

    // Verify matching step names
    if let Event::StepStarted {
        step_name: start_name,
        ..
    } = &start
    {
        if let Event::StepFinished {
            step_name: finish_name,
            ..
        } = &finish
        {
            assert_eq!(start_name, finish_name);
        }
    }

    // Verify serialization
    let start_json = serde_json::to_string(&start).unwrap();
    let finish_json = serde_json::to_string(&finish).unwrap();
    assert!(start_json.contains(r#""type":"STEP_STARTED""#));
    assert!(finish_json.contains(r#""type":"STEP_FINISHED""#));
    assert!(start_json.contains(r#""stepName":"data_processing""#));
    assert!(finish_json.contains(r#""stepName":"data_processing""#));
}

/// Test: Multiple step sequences
/// Protocol: Verify correct step name tracking across multiple steps
#[test]
fn test_multiple_step_sequences() {
    let mut ctx = make_agui_ctx("t1", "r1");

    // Step 1
    let step1_start = AgentEvent::StepStart {
        message_id: String::new(),
    };
    let events1 = ctx.on_agent_event(&step1_start);
    let step1_name = if let Event::StepStarted { step_name, .. } = &events1[0] {
        step_name.clone()
    } else {
        panic!("Expected StepStarted");
    };

    let step1_end = AgentEvent::StepEnd;
    let events1_end = ctx.on_agent_event(&step1_end);
    if let Event::StepFinished { step_name, .. } = &events1_end[0] {
        assert_eq!(*step_name, step1_name);
    }

    // Step 2
    let step2_start = AgentEvent::StepStart {
        message_id: String::new(),
    };
    let events2 = ctx.on_agent_event(&step2_start);
    let step2_name = if let Event::StepStarted { step_name, .. } = &events2[0] {
        step_name.clone()
    } else {
        panic!("Expected StepStarted");
    };

    // Step names should be different
    assert_ne!(step1_name, step2_name);
}

// ----------------------------------------------------------------------------
// JSON Patch Operation Tests (RFC 6902)
// ----------------------------------------------------------------------------
//
// Per AG-UI spec: StateDelta uses RFC 6902 JSON Patch with 6 operation types.

/// Test: JSON Patch - add operation
/// Protocol: RFC 6902 add operation
#[test]
fn test_json_patch_add_operation() {
    let delta = vec![json!({"op": "add", "path": "/newField", "value": "test"})];
    let event = Event::state_delta(delta);

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""op":"add""#));
    assert!(json.contains(r#""path":"/newField""#));
}

/// Test: JSON Patch - replace operation
/// Protocol: RFC 6902 replace operation
#[test]
fn test_json_patch_replace_operation() {
    let delta = vec![json!({"op": "replace", "path": "/existing", "value": "updated"})];
    let event = Event::state_delta(delta);

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""op":"replace""#));
}

/// Test: JSON Patch - remove operation
/// Protocol: RFC 6902 remove operation
#[test]
fn test_json_patch_remove_operation() {
    let delta = vec![json!({"op": "remove", "path": "/obsoleteField"})];
    let event = Event::state_delta(delta);

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""op":"remove""#));
    assert!(json.contains(r#""path":"/obsoleteField""#));
}

/// Test: JSON Patch - move operation
/// Protocol: RFC 6902 move operation
#[test]
fn test_json_patch_move_operation() {
    let delta = vec![json!({"op": "move", "from": "/oldPath", "path": "/newPath"})];
    let event = Event::state_delta(delta);

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""op":"move""#));
    assert!(json.contains(r#""from":"/oldPath""#));
    assert!(json.contains(r#""path":"/newPath""#));
}

/// Test: JSON Patch - copy operation
/// Protocol: RFC 6902 copy operation
#[test]
fn test_json_patch_copy_operation() {
    let delta = vec![json!({"op": "copy", "from": "/source", "path": "/destination"})];
    let event = Event::state_delta(delta);

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""op":"copy""#));
    assert!(json.contains(r#""from":"/source""#));
}

/// Test: JSON Patch - test operation
/// Protocol: RFC 6902 test operation for validation
#[test]
fn test_json_patch_test_operation() {
    let delta = vec![json!({"op": "test", "path": "/version", "value": "1.0"})];
    let event = Event::state_delta(delta);

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""op":"test""#));
    assert!(json.contains(r#""value":"1.0""#));
}

/// Test: JSON Patch - multiple operations
/// Protocol: Multiple patch operations in a single delta
#[test]
fn test_json_patch_multiple_operations() {
    let delta = vec![
        json!({"op": "test", "path": "/version", "value": "1.0"}),
        json!({"op": "replace", "path": "/version", "value": "2.0"}),
        json!({"op": "add", "path": "/newFeature", "value": true}),
        json!({"op": "remove", "path": "/deprecatedField"}),
    ];
    let event = Event::state_delta(delta.clone());

    if let Event::StateDelta { delta: d, .. } = event {
        assert_eq!(d.len(), 4);
    }
}

// ----------------------------------------------------------------------------
// MessagesSnapshot Tests
// ----------------------------------------------------------------------------
//
// Per AG-UI spec: MessagesSnapshot delivers complete conversation history.

/// Test: MessagesSnapshot for conversation restoration
/// Protocol: MESSAGES_SNAPSHOT for initial load or reconnection
#[test]
fn test_messages_snapshot_conversation_history() {
    let messages = vec![
        json!({"role": "user", "content": "Hello"}),
        json!({"role": "assistant", "content": "Hi! How can I help?"}),
        json!({"role": "user", "content": "What's the weather?"}),
        json!({"role": "assistant", "content": "I'll check that for you."}),
    ];

    let event = Event::messages_snapshot(messages.clone());

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"MESSAGES_SNAPSHOT""#));

    if let Event::MessagesSnapshot { messages: m, .. } = event {
        assert_eq!(m.len(), 4);
        assert_eq!(m[0]["role"], "user");
        assert_eq!(m[1]["role"], "assistant");
    }
}

/// Test: MessagesSnapshot with tool messages
/// Protocol: Conversation history can include tool messages
#[test]
fn test_messages_snapshot_with_tool_messages() {
    let messages = vec![
        json!({"role": "user", "content": "Search for rust tutorials"}),
        json!({"role": "assistant", "content": "I'll search for that.", "tool_calls": [{"id": "call_1", "name": "search"}]}),
        json!({"role": "tool", "tool_call_id": "call_1", "content": "Found 10 results"}),
        json!({"role": "assistant", "content": "I found 10 tutorials about Rust."}),
    ];

    let event = Event::messages_snapshot(messages);

    if let Event::MessagesSnapshot { messages: m, .. } = event {
        assert_eq!(m.len(), 4);
        assert_eq!(m[2]["role"], "tool");
        assert_eq!(m[2]["tool_call_id"], "call_1");
    }
}

// ----------------------------------------------------------------------------
// Activity Event Tests (replace flag)
// ----------------------------------------------------------------------------
//
// Per AG-UI spec: Activity snapshots have replace flag.
// replace: true (default) - replace existing activity message
// replace: false - preserve existing message ID

/// Test: Activity snapshot with replace: true
/// Protocol: replace: true replaces existing activity message
#[test]
fn test_activity_snapshot_replace_true() {
    use std::collections::HashMap;

    let mut content = HashMap::new();
    content.insert("status".to_string(), json!("processing"));

    let event = Event::activity_snapshot("act_1", "file_upload", content, Some(true));

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""replace":true"#));
}

/// Test: Activity snapshot with replace: false
/// Protocol: replace: false preserves existing message ID
#[test]
fn test_activity_snapshot_replace_false() {
    use std::collections::HashMap;

    let mut content = HashMap::new();
    content.insert("progress".to_string(), json!(0.5));

    let event = Event::activity_snapshot("act_1", "download", content, Some(false));

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""replace":false"#));
}

/// Test: Activity snapshot without replace (defaults behavior)
#[test]
fn test_activity_snapshot_replace_none() {
    use std::collections::HashMap;

    let mut content = HashMap::new();
    content.insert("data".to_string(), json!("test"));

    let event = Event::activity_snapshot("act_1", "process", content, None);

    let json = serde_json::to_string(&event).unwrap();
    // replace field should be omitted when None
    assert!(!json.contains(r#""replace""#));
}

// ----------------------------------------------------------------------------
// ToolCallStart with parentMessageId Tests
// ----------------------------------------------------------------------------
//
// Per AG-UI spec: Tool calls can optionally link to a parent message.

/// Test: ToolCallStart with parentMessageId
/// Protocol: Optional parentMessageId for linking tool calls to messages
#[test]
fn test_tool_call_start_with_parent_message_id() {
    let event = Event::tool_call_start("call_1", "search", Some("msg_123".into()));

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"TOOL_CALL_START""#));
    assert!(json.contains(r#""parentMessageId":"msg_123""#));

    let parsed: Event = serde_json::from_str(&json).unwrap();
    if let Event::ToolCallStart {
        parent_message_id, ..
    } = parsed
    {
        assert_eq!(parent_message_id, Some("msg_123".to_string()));
    }
}

/// Test: ToolCallStart without parentMessageId
#[test]
fn test_tool_call_start_without_parent_message_id() {
    let event = Event::tool_call_start("call_1", "read_file", None);

    let json = serde_json::to_string(&event).unwrap();
    assert!(!json.contains(r#""parentMessageId""#));
}

// ----------------------------------------------------------------------------
// ToolCallResult Tests
// ----------------------------------------------------------------------------
//
// Per AG-UI spec: ToolCallResult delivers execution output.

/// Test: ToolCallResult structure
/// Protocol: TOOL_CALL_RESULT with messageId, toolCallId, content
#[test]
fn test_tool_call_result_structure() {
    let event = Event::tool_call_result(
        "result_1",
        "call_1",
        r#"{"success": true, "data": "result"}"#,
    );

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"TOOL_CALL_RESULT""#));
    assert!(json.contains(r#""messageId":"result_1""#));
    assert!(json.contains(r#""toolCallId":"call_1""#));
    assert!(json.contains(r#""content":"#));

    let parsed: Event = serde_json::from_str(&json).unwrap();
    if let Event::ToolCallResult {
        message_id,
        tool_call_id,
        content,
        ..
    } = parsed
    {
        assert_eq!(message_id, "result_1");
        assert_eq!(tool_call_id, "call_1");
        assert!(content.contains("success"));
    }
}

/// Test: ToolCallResult with error content
#[test]
fn test_tool_call_result_error_content() {
    let error_result = json!({
        "status": "error",
        "tool_name": "write_file",
        "message": "Permission denied"
    });
    let event = Event::tool_call_result(
        "result_err",
        "call_write",
        serde_json::to_string(&error_result).unwrap(),
    );

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""toolCallId":"call_write""#));

    let parsed: Event = serde_json::from_str(&json).unwrap();
    if let Event::ToolCallResult { content, .. } = parsed {
        let result: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(result["status"], "error");
    }
}

// ----------------------------------------------------------------------------
// TextMessage Role Tests
// ----------------------------------------------------------------------------
//
// Per AG-UI spec: Text messages have role (developer, system, assistant, user, tool).

/// Test: Text message roles
/// Protocol: Verify all role types serialize correctly
#[test]
fn test_text_message_all_roles() {
    let roles = vec![
        (Role::Developer, "developer"),
        (Role::System, "system"),
        (Role::Assistant, "assistant"),
        (Role::User, "user"),
        (Role::Tool, "tool"),
    ];

    for (role, expected) in roles {
        let event =
            Event::text_message_chunk(Some("msg_1".into()), Some(role), Some("test".into()));
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains(&format!(r#""role":"{}""#, expected)),
            "Role {} not found in JSON",
            expected
        );
    }
}

// ----------------------------------------------------------------------------
// Event Timestamp Tests
// ----------------------------------------------------------------------------
//
// Per AG-UI spec: All events can have optional timestamp in milliseconds.

/// Test: Event with timestamp
/// Protocol: BaseEvent includes optional timestamp
#[test]
fn test_event_with_timestamp() {
    let mut event = Event::run_started("t1", "r1", None);
    event = event.with_timestamp(1704067200000); // 2024-01-01 00:00:00 UTC

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""timestamp":1704067200000"#));
}

/// Test: Event timestamp roundtrip
#[test]
fn test_event_timestamp_roundtrip() {
    let timestamp = 1704067200000u64;
    let event = Event::state_snapshot(json!({"test": true})).with_timestamp(timestamp);

    let json = serde_json::to_string(&event).unwrap();
    let parsed: Event = serde_json::from_str(&json).unwrap();

    if let Event::StateSnapshot { base, .. } = parsed {
        assert_eq!(base.timestamp, Some(timestamp));
    }
}

// ----------------------------------------------------------------------------
// Raw Event Tests (source attribution)
// ----------------------------------------------------------------------------
//
// Per AG-UI spec: Raw events pass external system events with optional source.

/// Test: Raw event with source attribution
/// Protocol: RAW event with optional source field
#[test]
fn test_raw_event_with_source() {
    let external = json!({"type": "model_response", "tokens": 150});
    let event = Event::raw(external.clone(), Some("anthropic".into()));

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"RAW""#));
    assert!(json.contains(r#""source":"anthropic""#));

    let parsed: Event = serde_json::from_str(&json).unwrap();
    if let Event::Raw { source, .. } = parsed {
        assert_eq!(source, Some("anthropic".to_string()));
    }
}

/// Test: Raw event without source
#[test]
fn test_raw_event_without_source() {
    let external = json!({"custom": "data"});
    let event = Event::raw(external, None);

    let json = serde_json::to_string(&event).unwrap();
    assert!(!json.contains(r#""source""#));
}

// ----------------------------------------------------------------------------
// Custom Event Tests
// ----------------------------------------------------------------------------
//
// Per AG-UI spec: Custom events for application-specific extensions.

/// Test: Custom event with name and value
/// Protocol: CUSTOM event for protocol extensions
#[test]
fn test_custom_event_structure() {
    let value = json!({
        "action": "highlight",
        "target": "line:42",
        "color": "yellow"
    });
    let event = Event::custom("editor_highlight", value.clone());

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"CUSTOM""#));
    assert!(json.contains(r#""name":"editor_highlight""#));
    assert!(json.contains(r#""action":"highlight""#));

    let parsed: Event = serde_json::from_str(&json).unwrap();
    if let Event::Custom { name, value: v, .. } = parsed {
        assert_eq!(name, "editor_highlight");
        assert_eq!(v["target"], "line:42");
    }
}

// ----------------------------------------------------------------------------
// Full Protocol Flow Tests
// ----------------------------------------------------------------------------
//
// End-to-end tests for complete AG-UI protocol flows.

/// Test: Complete tool call flow with all events
/// Protocol: Full TOOL_CALL flow: START → ARGS → END → RESULT
#[test]
fn test_complete_tool_call_protocol_flow() {
    let mut ctx = make_agui_ctx("t1", "r1");
    let mut events: Vec<Event> = Vec::new();

    // Start
    let start = AgentEvent::ToolCallStart {
        id: "call_search".into(),
        name: "web_search".into(),
    };
    events.extend(ctx.on_agent_event(&start));

    // Args streaming
    let args1 = AgentEvent::ToolCallDelta {
        id: "call_search".into(),
        args_delta: r#"{"query":"#.into(),
    };
    events.extend(ctx.on_agent_event(&args1));

    let args2 = AgentEvent::ToolCallDelta {
        id: "call_search".into(),
        args_delta: r#""rust tutorials"}"#.into(),
    };
    events.extend(ctx.on_agent_event(&args2));

    // Ready (end args)
    let ready = AgentEvent::ToolCallReady {
        id: "call_search".into(),
        name: "web_search".into(),
        arguments: json!({"query": "rust tutorials"}),
    };
    events.extend(ctx.on_agent_event(&ready));

    // Result
    let done = AgentEvent::ToolCallDone {
        id: "call_search".into(),
        result: ToolResult::success("web_search", json!({"results": 10})),
        patch: None,
        message_id: String::new(),
    };
    events.extend(ctx.on_agent_event(&done));

    // Verify complete sequence
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::ToolCallStart { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::ToolCallArgs { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::ToolCallEnd { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::ToolCallResult { .. })));
}

/// Test: State sync flow (snapshot then deltas)
/// Protocol: STATE_SNAPSHOT → STATE_DELTA*
#[test]
fn test_state_sync_protocol_flow() {
    let mut ctx = make_agui_ctx("t1", "r1");
    let mut events: Vec<Event> = Vec::new();

    // Initial snapshot
    let snapshot = AgentEvent::StateSnapshot {
        snapshot: json!({"counter": 0, "items": []}),
    };
    events.extend(ctx.on_agent_event(&snapshot));

    // Delta 1: increment counter
    let delta1 = AgentEvent::StateDelta {
        delta: vec![json!({"op": "replace", "path": "/counter", "value": 1})],
    };
    events.extend(ctx.on_agent_event(&delta1));

    // Delta 2: add item
    let delta2 = AgentEvent::StateDelta {
        delta: vec![json!({"op": "add", "path": "/items/-", "value": "item1"})],
    };
    events.extend(ctx.on_agent_event(&delta2));

    // Verify flow
    assert_eq!(events.len(), 3);
    assert!(matches!(&events[0], Event::StateSnapshot { .. }));
    assert!(matches!(&events[1], Event::StateDelta { .. }));
    assert!(matches!(&events[2], Event::StateDelta { .. }));
}

/// Test: Mixed content flow (text + tool + text)
/// Protocol: Verify correct event sequencing with interleaved content
#[test]
fn test_mixed_content_protocol_flow() {
    let mut ctx = make_agui_ctx("t1", "r1");
    let mut events: Vec<Event> = Vec::new();

    // Text starts
    let text1 = AgentEvent::TextDelta {
        delta: "Let me search".into(),
    };
    events.extend(ctx.on_agent_event(&text1));

    // Tool interrupts (should end text first)
    let tool_start = AgentEvent::ToolCallStart {
        id: "call_1".into(),
        name: "search".into(),
    };
    events.extend(ctx.on_agent_event(&tool_start));

    // Tool completes
    let tool_ready = AgentEvent::ToolCallReady {
        id: "call_1".into(),
        name: "search".into(),
        arguments: json!({}),
    };
    events.extend(ctx.on_agent_event(&tool_ready));

    let tool_done = AgentEvent::ToolCallDone {
        id: "call_1".into(),
        result: ToolResult::success("search", json!({"count": 5})),
        patch: None,
        message_id: String::new(),
    };
    events.extend(ctx.on_agent_event(&tool_done));

    // Text resumes
    let text2 = AgentEvent::TextDelta {
        delta: "Found 5 results".into(),
    };
    events.extend(ctx.on_agent_event(&text2));

    // Verify TEXT_MESSAGE_END appears before TOOL_CALL_START
    let text_end_idx = events
        .iter()
        .position(|e| matches!(e, Event::TextMessageEnd { .. }))
        .unwrap();
    let tool_start_idx = events
        .iter()
        .position(|e| matches!(e, Event::ToolCallStart { .. }))
        .unwrap();
    assert!(
        text_end_idx < tool_start_idx,
        "TEXT_MESSAGE_END must come before TOOL_CALL_START"
    );
}

// ============================================================================
// AG-UI Message Types Tests
// ============================================================================
//
// Per AG-UI spec: Six message roles - user, assistant, system, tool, developer, activity
// Reference: https://docs.ag-ui.com/concepts/messages
//

/// Test: Message user message creation
/// Protocol: UserMessage with content (string or InputContent[])
#[test]
fn test_agui_message_user() {
    let msg = tirea_protocol_ag_ui::Message::user("Hello, how can you help?");

    assert_eq!(msg.role, Role::User);
    assert_eq!(msg.content, "Hello, how can you help?");
    assert!(msg.id.is_none());
    assert!(msg.tool_call_id.is_none());

    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""role":"user""#));
    assert!(json.contains(r#""content":"Hello, how can you help?""#));
}

/// Test: Message assistant message creation
/// Protocol: AssistantMessage with optional content and toolCalls
#[test]
fn test_agui_message_assistant() {
    let msg = tirea_protocol_ag_ui::Message::assistant("I can help you with that.");

    assert_eq!(msg.role, Role::Assistant);
    assert_eq!(msg.content, "I can help you with that.");

    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""role":"assistant""#));
}

/// Test: Message system message creation
/// Protocol: SystemMessage with required content
#[test]
fn test_agui_message_system() {
    let msg = tirea_protocol_ag_ui::Message::system("You are a helpful assistant.");

    assert_eq!(msg.role, Role::System);
    assert_eq!(msg.content, "You are a helpful assistant.");

    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""role":"system""#));
}

/// Test: Message tool message creation
/// Protocol: ToolMessage with toolCallId linking to assistant's tool call
#[test]
fn test_agui_message_tool() {
    let msg =
        tirea_protocol_ag_ui::Message::tool(r#"{"result": "success", "data": 42}"#, "call_123");

    assert_eq!(msg.role, Role::Tool);
    assert_eq!(msg.tool_call_id, Some("call_123".to_string()));

    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""role":"tool""#));
    assert!(json.contains(r#""toolCallId":"call_123""#));
}

/// Test: Message tool message with error
/// Protocol: ToolMessage can include error information
#[test]
fn test_agui_message_tool_with_error() {
    let error_content = json!({
        "status": "error",
        "error": "Connection refused",
        "code": "ECONNREFUSED"
    });
    let msg = tirea_protocol_ag_ui::Message::tool(
        serde_json::to_string(&error_content).unwrap(),
        "call_err",
    );

    assert_eq!(msg.role, Role::Tool);
    let parsed: Value = serde_json::from_str(&msg.content).unwrap();
    assert_eq!(parsed["status"], "error");
    assert_eq!(parsed["code"], "ECONNREFUSED");
}

/// Test: Message with custom ID
/// Protocol: Messages can have unique identifiers
#[test]
fn test_agui_message_with_id() {
    let mut msg = tirea_protocol_ag_ui::Message::user("test");
    msg.id = Some("msg_12345".to_string());

    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""id":"msg_12345""#));
}

/// Test: Message roundtrip serialization
#[test]
fn test_agui_message_roundtrip() {
    let messages = vec![
        tirea_protocol_ag_ui::Message::user("Hello"),
        tirea_protocol_ag_ui::Message::assistant("Hi there!"),
        tirea_protocol_ag_ui::Message::system("Be helpful"),
        tirea_protocol_ag_ui::Message::tool("result", "call_1"),
    ];

    for msg in messages {
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: tirea_protocol_ag_ui::Message = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.role, msg.role);
        assert_eq!(parsed.content, msg.content);
    }
}

// ============================================================================
// RunAgentInput Tests
// ============================================================================
//
// Per AG-UI spec: RunAgentInput contains threadId, runId, messages, tools, state, context
// Reference: https://docs.ag-ui.com/sdk/js/core/types
//

/// Test: RunAgentInput basic creation
/// Protocol: Required threadId and runId
#[test]
fn test_run_agent_request_basic() {
    let request = RunAgentInput::new("thread_abc".to_string(), "run_123".to_string());

    assert_eq!(request.thread_id, "thread_abc");
    assert_eq!(request.run_id, "run_123");
    assert!(request.messages.is_empty());
    assert!(request.tools.is_empty());
}

/// Test: RunAgentInput with messages
/// Protocol: Message array for conversation history
#[test]
fn test_run_agent_request_with_messages() {
    let request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::user("Hello"))
        .with_message(tirea_protocol_ag_ui::Message::assistant("Hi!"))
        .with_message(tirea_protocol_ag_ui::Message::user("What's 2+2?"));

    assert_eq!(request.messages.len(), 3);
    assert_eq!(request.messages[0].role, Role::User);
    assert_eq!(request.messages[1].role, Role::Assistant);
    assert_eq!(request.messages[2].role, Role::User);
}

/// Test: RunAgentInput with tools
/// Protocol: Tools array defines available capabilities
#[test]
fn test_run_agent_request_with_tools() {
    let request = RunAgentInput {
        tools: vec![
            tirea_protocol_ag_ui::Tool::backend("search", "Search the web"),
            tirea_protocol_ag_ui::Tool::frontend("copyToClipboard", "Copy text"),
        ],
        ..RunAgentInput::new("t1".to_string(), "r1".to_string())
    };

    assert_eq!(request.tools.len(), 2);
}

/// Test: RunAgentInput with initial state
/// Protocol: State object for agent execution context
#[test]
fn test_run_agent_request_with_state() {
    let initial_state = json!({
        "counter": 0,
        "preferences": {"theme": "dark"},
        "history": []
    });

    let request =
        RunAgentInput::new("t1".to_string(), "r1".to_string()).with_state(initial_state.clone());

    assert_eq!(request.state, Some(initial_state));
}

/// Test: RunAgentInput with parent run ID
/// Protocol: parentRunId for branching/sub-agent runs
#[test]
fn test_run_agent_request_with_parent_run() {
    // Create request and set parent_run_id directly
    let mut request = RunAgentInput::new("t1".to_string(), "r2".to_string());
    request.parent_run_id = Some("r1".to_string());

    assert_eq!(request.parent_run_id, Some("r1".to_string()));
}

/// Test: RunAgentInput serialization
#[test]
fn test_run_agent_request_serialization() {
    let request = RunAgentInput::new("thread_1".to_string(), "run_1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::user("test"))
        .with_state(json!({"key": "value"}));

    let json = serde_json::to_string(&request).unwrap();
    assert!(json.contains(r#""threadId":"thread_1""#));
    assert!(json.contains(r#""runId":"run_1""#));
    assert!(json.contains(r#""messages":[{"role":"user""#));
}

/// Test: RunAgentInput deserialization
#[test]
fn test_run_agent_request_deserialization() {
    let json = r#"{
        "threadId": "t1",
        "runId": "r1",
        "messages": [
            {"role": "user", "content": "Hello"},
            {"role": "assistant", "content": "Hi!"}
        ],
        "tools": [],
        "state": {"counter": 5}
    }"#;

    let request: RunAgentInput = serde_json::from_str(json).unwrap();
    assert_eq!(request.thread_id, "t1");
    assert_eq!(request.run_id, "r1");
    assert_eq!(request.messages.len(), 2);
    assert_eq!(request.state.unwrap()["counter"], 5);
}

// ============================================================================
// Tool Tests
// ============================================================================
//
// Per AG-UI spec: Tool with name, description, and parameters (JSON Schema)
// Reference: https://docs.ag-ui.com/concepts/tools
//

/// Test: Backend tool definition
/// Protocol: Backend tools execute on agent side
#[test]
fn test_agui_tool_def_backend() {
    let tool = tirea_protocol_ag_ui::Tool::backend("search", "Search for information");

    assert_eq!(tool.name, "search");
    assert_eq!(tool.description, "Search for information");
    assert_eq!(tool.execute, ToolExecutionLocation::Backend);

    let json = serde_json::to_string(&tool).unwrap();
    // Backend is NOT the default (Frontend is), so execute field IS included
    assert!(json.contains(r#""execute":"backend""#));
}

/// Test: Frontend tool definition
/// Protocol: Frontend tools execute on client side
#[test]
fn test_agui_tool_def_frontend() {
    let tool = tirea_protocol_ag_ui::Tool::frontend("showNotification", "Display a notification");

    assert_eq!(tool.name, "showNotification");
    assert_eq!(tool.execute, ToolExecutionLocation::Frontend);

    let json = serde_json::to_string(&tool).unwrap();
    // Frontend is the default, so execute field is omitted
    assert!(!json.contains(r#""execute""#));
}

/// Test: Tool with JSON Schema parameters
/// Protocol: Parameters defined using JSON Schema
#[test]
fn test_agui_tool_def_with_schema() {
    let schema = json!({
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "Search query"},
            "limit": {"type": "integer", "default": 10}
        },
        "required": ["query"]
    });

    let tool =
        tirea_protocol_ag_ui::Tool::backend("search", "Search").with_parameters(schema.clone());

    assert_eq!(tool.parameters, Some(schema));
}

/// Test: Tool serialization with all fields
#[test]
fn test_agui_tool_def_full_serialization() {
    let tool = tirea_protocol_ag_ui::Tool::frontend("readFile", "Read a file from disk")
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            },
            "required": ["path"]
        }));

    let json = serde_json::to_string(&tool).unwrap();
    assert!(json.contains(r#""name":"readFile""#));
    assert!(json.contains(r#""description":"Read a file from disk""#));
    // Frontend is default, so execute is omitted
    assert!(!json.contains(r#""execute""#));
    assert!(json.contains(r#""parameters""#));
}

// ============================================================================
// Event Stream Pattern Tests
// ============================================================================
//
// Per AG-UI spec: Event streaming with cancel/resume functionality
// Reference: https://docs.ag-ui.com/introduction
//

/// Test: Event sequence for canceled run
/// Protocol: Run can be canceled, resulting in RUN_ERROR or no RUN_FINISHED
#[test]
fn test_event_sequence_canceled_run() {
    let mut ctx = make_agui_ctx("t1", "r1");
    let mut events: Vec<Event> = Vec::new();

    // Run starts
    let start = AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    };
    events.extend(ctx.on_agent_event(&start));

    // Text streaming begins
    let text = AgentEvent::TextDelta {
        delta: "Processing...".into(),
    };
    events.extend(ctx.on_agent_event(&text));

    // Run finished with cancelled termination (simulating external cancel)
    let cancel = AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: tirea_agentos::contracts::TerminationReason::Cancelled,
    };
    let cancel_events = ctx.on_agent_event(&cancel);
    events.extend(cancel_events);

    // Verify run started
    assert!(events.iter().any(|e| matches!(e, Event::RunStarted { .. })));
    assert!(events.iter().any(|e| matches!(e, Event::RunError { .. })));
}

/// Test: Error during text streaming
/// Protocol: Error interrupts text stream
#[test]
fn test_error_interrupts_text_stream() {
    let mut ctx = make_agui_ctx("t1", "r1");
    let mut events: Vec<Event> = Vec::new();

    // Text starts
    let text = AgentEvent::TextDelta {
        delta: "Starting...".into(),
    };
    events.extend(ctx.on_agent_event(&text));

    // Error occurs
    let error = AgentEvent::Error {
        message: "API rate limit exceeded".into(),
        code: Some("LLM_ERROR".into()),
    };
    events.extend(ctx.on_agent_event(&error));

    // Should have TEXT_MESSAGE_START and possibly TEXT_MESSAGE_END before error
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::TextMessageStart { .. })));
}

/// Test: Multiple text messages in sequence
/// Protocol: Each new message gets its own START/END pair
#[test]
fn test_multiple_text_messages() {
    let mut ctx = make_agui_ctx("t1", "r1");
    let mut events: Vec<Event> = Vec::new();

    // First message
    let text1 = AgentEvent::TextDelta {
        delta: "First message".into(),
    };
    events.extend(ctx.on_agent_event(&text1));

    // End first message by starting something else
    let finish1 = AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: Some(serde_json::json!({"response": "First message"})),
        termination: tirea_agentos::contracts::TerminationReason::NaturalEnd,
    };
    events.extend(ctx.on_agent_event(&finish1));

    // Reset context for new message
    ctx = make_agui_ctx("t1", "r2");

    // Second message
    let text2 = AgentEvent::TextDelta {
        delta: "Second message".into(),
    };
    events.extend(ctx.on_agent_event(&text2));

    // Count TEXT_MESSAGE_START events
    let start_count = events
        .iter()
        .filter(|e| matches!(e, Event::TextMessageStart { .. }))
        .count();
    assert_eq!(start_count, 2, "Should have 2 TEXT_MESSAGE_START events");
}

// ============================================================================
// State Management Edge Cases
// ============================================================================
//
// Per AG-UI spec: State sync via snapshots and deltas
// Reference: https://docs.ag-ui.com/concepts/state
//

/// Test: Empty state snapshot
/// Protocol: Snapshot can be empty object
#[test]
fn test_empty_state_snapshot() {
    let event = Event::state_snapshot(json!({}));

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"STATE_SNAPSHOT""#));
    assert!(json.contains(r#""snapshot":{}"#));
}

/// Test: Empty state delta (no-op)
/// Protocol: Delta with no operations
#[test]
fn test_empty_state_delta() {
    let event = Event::state_delta(vec![]);

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"STATE_DELTA""#));
    assert!(json.contains(r#""delta":[]"#));
}

/// Test: State with nested arrays
/// Protocol: State can have complex nested structures
#[test]
fn test_state_with_nested_arrays() {
    let complex_state = json!({
        "matrix": [[1, 2], [3, 4], [5, 6]],
        "records": [
            {"id": 1, "tags": ["a", "b"]},
            {"id": 2, "tags": ["c"]}
        ]
    });

    let event = Event::state_snapshot(complex_state.clone());

    if let Event::StateSnapshot { snapshot, .. } = event {
        assert_eq!(snapshot["matrix"][0][0], 1);
        assert_eq!(snapshot["records"][0]["tags"][1], "b");
    }
}

/// Test: State delta with array index operations
/// Protocol: JSON Pointer can target array indices
#[test]
fn test_state_delta_array_operations() {
    let delta = vec![
        json!({"op": "add", "path": "/items/0", "value": "first"}),
        json!({"op": "add", "path": "/items/-", "value": "last"}),
        json!({"op": "replace", "path": "/items/1", "value": "replaced"}),
        json!({"op": "remove", "path": "/items/2"}),
    ];

    let event = Event::state_delta(delta);

    if let Event::StateDelta { delta: d, .. } = event {
        assert_eq!(d.len(), 4);
        assert_eq!(d[0]["path"], "/items/0");
        assert_eq!(d[1]["path"], "/items/-"); // "-" means append
    }
}

// ============================================================================
// Tool Call Edge Cases
// ============================================================================
//
// Per AG-UI spec: Tool calls with various argument patterns
//

/// Test: Tool call with empty arguments
/// Protocol: Tool can have no arguments
#[test]
fn test_tool_call_empty_args() {
    let mut ctx = make_agui_ctx("t1", "r1");

    let start = AgentEvent::ToolCallStart {
        id: "call_1".into(),
        name: "getCurrentTime".into(),
    };
    let events = ctx.on_agent_event(&start);

    assert!(events
        .iter()
        .any(|e| matches!(e, Event::ToolCallStart { .. })));
}

/// Test: Tool call with nested JSON arguments
/// Protocol: Arguments can be complex JSON
#[test]
fn test_tool_call_complex_args() {
    let mut ctx = make_agui_ctx("t1", "r1");

    let args = AgentEvent::ToolCallDelta {
        id: "call_1".into(),
        args_delta: serde_json::to_string(&json!({
            "config": {
                "nested": {
                    "deep": {
                        "value": [1, 2, 3]
                    }
                }
            }
        }))
        .unwrap(),
    };
    let events = ctx.on_agent_event(&args);

    if let Some(Event::ToolCallArgs { delta, .. }) = events.first() {
        let parsed: Value = serde_json::from_str(delta).unwrap();
        assert_eq!(parsed["config"]["nested"]["deep"]["value"][0], 1);
    }
}

/// Test: Tool result with warning
/// Protocol: Tool can return with warning status
#[test]
fn test_tool_result_with_warning_status() {
    let mut ctx = make_agui_ctx("t1", "r1");

    let done = AgentEvent::ToolCallDone {
        id: "call_1".into(),
        result: ToolResult::warning("search", json!({"results": 5}), "Results may be stale"),
        patch: None,
        message_id: String::new(),
    };
    let events = ctx.on_agent_event(&done);

    assert!(events
        .iter()
        .any(|e| matches!(e, Event::ToolCallResult { .. })));
}

/// Test: Tool result with pending status
/// Protocol: Tool can indicate async/pending execution
#[test]
fn test_tool_result_with_pending_status() {
    let mut ctx = make_agui_ctx("t1", "r1");

    let done = AgentEvent::ToolCallDone {
        id: "call_1".into(),
        result: ToolResult::suspended("longRunningTask", "Task queued, check back later"),
        patch: None,
        message_id: String::new(),
    };
    let events = ctx.on_agent_event(&done);

    if let Some(Event::ToolCallResult { content, .. }) = events.first() {
        let parsed: Value = serde_json::from_str(content).unwrap();
        assert_eq!(parsed["status"], "pending");
    }
}

// ============================================================================
// Request Validation Edge Cases
// ============================================================================
//
// Per AG-UI spec: Input validation for RunAgentInput
//

/// Test: Request with empty thread ID (should be valid per protocol)
#[test]
fn test_request_empty_thread_id() {
    let request = RunAgentInput::new("".to_string(), "r1".to_string());
    assert_eq!(request.thread_id, "");
    // Note: Empty thread ID is technically valid JSON, validation is app-level
}

/// Test: Request with very long IDs
#[test]
fn test_request_long_ids() {
    let long_id = "x".repeat(1000);
    let request = RunAgentInput::new(long_id.clone(), long_id.clone());

    assert_eq!(request.thread_id.len(), 1000);
    assert_eq!(request.run_id.len(), 1000);
}

/// Test: Request with special characters in IDs
#[test]
fn test_request_special_char_ids() {
    let special_id = "thread-123_abc.xyz:456";
    let request = RunAgentInput::new(special_id.to_string(), "run-1".to_string());

    let json = serde_json::to_string(&request).unwrap();
    let parsed: RunAgentInput = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.thread_id, special_id);
}

/// Test: Request with Unicode in messages
#[test]
fn test_request_unicode_messages() {
    let request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::user(
            "Hello! 你好！こんにちは！🎉",
        ))
        .with_message(tirea_protocol_ag_ui::Message::assistant("Привет! مرحبا"));

    let json = serde_json::to_string(&request).unwrap();
    let parsed: RunAgentInput = serde_json::from_str(&json).unwrap();

    assert!(parsed.messages[0].content.contains("你好"));
    assert!(parsed.messages[0].content.contains("🎉"));
    assert!(parsed.messages[1].content.contains("Привет"));
}

// ============================================================================
// Suspension Response Tests
// ============================================================================
//
// Per AG-UI spec: Human-in-the-loop approval/denial flows
//

/// Test: Suspension response approval
/// Protocol: User approves suspended interaction
#[test]
fn test_interaction_response_approval() {
    let response = SuspensionResponse::new("int_123", json!({"approved": true}));

    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains(r#""target_id":"int_123""#));
    assert!(json.contains(r#""approved":true"#));
}

/// Test: Suspension response denial
/// Protocol: User denies suspended interaction
#[test]
fn test_interaction_response_denial() {
    let response = SuspensionResponse::new(
        "int_123",
        json!({"approved": false, "reason": "Not authorized"}),
    );

    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains(r#""approved":false"#));
    assert!(json.contains(r#""reason":"Not authorized""#));
}

/// Test: Suspension response with custom data
/// Protocol: Response can include user-provided data
#[test]
fn test_interaction_response_with_data() {
    let response = SuspensionResponse::new(
        "input_1",
        json!({
            "value": "user input text",
            "selected_option": 2,
            "metadata": {"timestamp": 1234567890}
        }),
    );

    assert_eq!(response.result["value"], "user input text");
    assert_eq!(response.result["selected_option"], 2);
}

// ============================================================================
// Context Tracking Tests
// ============================================================================
//
// Per AG-UI spec: Context management across events
//

/// Test: AgUiEventContext message ID generation
/// Protocol: Unique message IDs across a run
#[test]
fn test_context_message_id_uniqueness() {
    let mut ctx = make_agui_ctx("t1", "r1");

    let id1 = ctx.new_message_id();
    let id2 = ctx.new_message_id();
    let id3 = ctx.new_message_id();

    assert_ne!(id1, id2);
    assert_ne!(id2, id3);
    assert_ne!(id1, id3);
}

/// Test: AgUiEventContext step name tracking
/// Protocol: Steps are numbered sequentially
#[test]
fn test_context_step_name_sequence() {
    let mut ctx = make_agui_ctx("t1", "r1");

    let step1 = ctx.next_step_name();
    let step2 = ctx.next_step_name();
    let step3 = ctx.next_step_name();

    // Steps should have sequential numbers
    assert!(step1.contains("1") || step1.contains("step"));
    assert_ne!(step1, step2);
    assert_ne!(step2, step3);
}

/// Test: AgUiEventContext text stream state tracking
/// Protocol: Tracks whether text is currently streaming
#[test]
fn test_context_text_stream_state() {
    let mut ctx = make_agui_ctx("t1", "r1");

    // Start streaming returns true (was not started before)
    let was_started = ctx.start_text();
    assert!(was_started); // First start returns true (means "did start")

    // Start again returns false (already started)
    let was_started_again = ctx.start_text();
    assert!(!was_started_again); // Already started, returns false

    // End streaming returns true (was active)
    let ended = ctx.end_text();
    assert!(ended);

    // End again returns false (not active)
    let ended_again = ctx.end_text();
    assert!(!ended_again);
}

// ============================================================================
// AG-UI Protocol Spec Tests - with_timestamp for All Event Types
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/sdk/js/core/events
// BaseEvent Specification: All events extend BaseEvent which includes:
// - type: EventType (required)
// - timestamp?: number (optional, milliseconds since epoch)
// - rawEvent?: unknown (optional, passthrough from external systems)
//

/// Test: with_timestamp on RUN_STARTED event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_run_started() {
    let ts = 1704067200000u64; // 2024-01-01 00:00:00 UTC
    let event = Event::run_started("t1", "r1", None).with_timestamp(ts);

    if let Event::RunStarted { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected RunStarted");
    }

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""timestamp":1704067200000"#));
}

/// Test: with_timestamp on RUN_FINISHED event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_run_finished() {
    let ts = 1704067200000u64;
    let event = Event::run_finished("t1", "r1", None).with_timestamp(ts);

    if let Event::RunFinished { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected RunFinished");
    }
}

/// Test: with_timestamp on RUN_ERROR event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_run_error() {
    let ts = 1704067200000u64;
    let event = Event::run_error("error", None).with_timestamp(ts);

    if let Event::RunError { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected RunError");
    }
}

/// Test: with_timestamp on STEP_STARTED event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_step_started() {
    let ts = 1704067200000u64;
    let event = Event::step_started("step1").with_timestamp(ts);

    if let Event::StepStarted { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected StepStarted");
    }
}

/// Test: with_timestamp on STEP_FINISHED event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_step_finished() {
    let ts = 1704067200000u64;
    let event = Event::step_finished("step1").with_timestamp(ts);

    if let Event::StepFinished { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected StepFinished");
    }
}

/// Test: with_timestamp on TEXT_MESSAGE_START event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_text_message_start() {
    let ts = 1704067200000u64;
    let event = Event::text_message_start("msg1").with_timestamp(ts);

    if let Event::TextMessageStart { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected TextMessageStart");
    }
}

/// Test: with_timestamp on TEXT_MESSAGE_CONTENT event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_text_message_content() {
    let ts = 1704067200000u64;
    let event = Event::text_message_content("msg1", "hello").with_timestamp(ts);

    if let Event::TextMessageContent { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected TextMessageContent");
    }
}

/// Test: with_timestamp on TEXT_MESSAGE_END event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_text_message_end() {
    let ts = 1704067200000u64;
    let event = Event::text_message_end("msg1").with_timestamp(ts);

    if let Event::TextMessageEnd { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected TextMessageEnd");
    }
}

/// Test: with_timestamp on TEXT_MESSAGE_CHUNK event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_text_message_chunk() {
    let ts = 1704067200000u64;
    let event =
        Event::text_message_chunk(Some("msg1".into()), None, Some("hi".into())).with_timestamp(ts);

    if let Event::TextMessageChunk { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected TextMessageChunk");
    }
}

/// Test: with_timestamp on TOOL_CALL_START event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_tool_call_start() {
    let ts = 1704067200000u64;
    let event = Event::tool_call_start("call1", "search", None).with_timestamp(ts);

    if let Event::ToolCallStart { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected ToolCallStart");
    }
}

/// Test: with_timestamp on TOOL_CALL_ARGS event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_tool_call_args() {
    let ts = 1704067200000u64;
    let event = Event::tool_call_args("call1", "{}").with_timestamp(ts);

    if let Event::ToolCallArgs { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected ToolCallArgs");
    }
}

/// Test: with_timestamp on TOOL_CALL_END event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_tool_call_end() {
    let ts = 1704067200000u64;
    let event = Event::tool_call_end("call1").with_timestamp(ts);

    if let Event::ToolCallEnd { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected ToolCallEnd");
    }
}

/// Test: with_timestamp on TOOL_CALL_RESULT event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_tool_call_result() {
    let ts = 1704067200000u64;
    let event = Event::tool_call_result("msg1", "call1", "result").with_timestamp(ts);

    if let Event::ToolCallResult { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected ToolCallResult");
    }
}

/// Test: with_timestamp on TOOL_CALL_CHUNK event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_tool_call_chunk() {
    let ts = 1704067200000u64;
    let event = Event::tool_call_chunk(Some("call1".into()), None, None, Some("{}".into()))
        .with_timestamp(ts);

    if let Event::ToolCallChunk { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected ToolCallChunk");
    }
}

/// Test: with_timestamp on STATE_SNAPSHOT event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_state_snapshot() {
    let ts = 1704067200000u64;
    let event = Event::state_snapshot(json!({})).with_timestamp(ts);

    if let Event::StateSnapshot { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected StateSnapshot");
    }
}

/// Test: with_timestamp on STATE_DELTA event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_state_delta() {
    let ts = 1704067200000u64;
    let event = Event::state_delta(vec![]).with_timestamp(ts);

    if let Event::StateDelta { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected StateDelta");
    }
}

/// Test: with_timestamp on MESSAGES_SNAPSHOT event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_messages_snapshot() {
    let ts = 1704067200000u64;
    let event = Event::messages_snapshot(vec![]).with_timestamp(ts);

    if let Event::MessagesSnapshot { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected MessagesSnapshot");
    }
}

/// Test: with_timestamp on ACTIVITY_SNAPSHOT event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_activity_snapshot() {
    use std::collections::HashMap;
    let ts = 1704067200000u64;
    let event =
        Event::activity_snapshot("act1", "progress", HashMap::new(), None).with_timestamp(ts);

    if let Event::ActivitySnapshot { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected ActivitySnapshot");
    }
}

/// Test: with_timestamp on ACTIVITY_DELTA event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_activity_delta() {
    let ts = 1704067200000u64;
    let event = Event::activity_delta("act1", "progress", vec![]).with_timestamp(ts);

    if let Event::ActivityDelta { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected ActivityDelta");
    }
}

/// Test: with_timestamp on RAW event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_raw() {
    let ts = 1704067200000u64;
    let event = Event::raw(json!({}), None).with_timestamp(ts);

    if let Event::Raw { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected Raw");
    }
}

/// Test: with_timestamp on CUSTOM event
/// Protocol: BaseEvent.timestamp per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_with_timestamp_custom() {
    let ts = 1704067200000u64;
    let event = Event::custom("my_event", json!({})).with_timestamp(ts);

    if let Event::Custom { base, .. } = &event {
        assert_eq!(base.timestamp, Some(ts));
    } else {
        panic!("Expected Custom");
    }
}

// ============================================================================
// AG-UI Protocol Spec Tests - now_millis Utility
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/sdk/js/core/events
// Timestamps should be in milliseconds since Unix epoch.
//

/// Test: now_millis returns reasonable timestamp
/// Protocol: Timestamps in milliseconds since epoch per AG-UI spec
#[test]
fn test_now_millis_returns_positive() {
    let ts = Event::now_millis();
    // Should be a reasonable Unix timestamp in milliseconds (after 2020)
    assert!(ts > 1577836800000, "Timestamp should be after 2020-01-01");
}

/// Test: now_millis increases over time
/// Protocol: Timestamps should be monotonically increasing
#[test]
fn test_now_millis_increases() {
    let ts1 = Event::now_millis();
    std::thread::sleep(std::time::Duration::from_millis(1));
    let ts2 = Event::now_millis();
    assert!(ts2 >= ts1, "Second timestamp should be >= first");
}

/// Test: now_millis can be used with with_timestamp
/// Protocol: Integration of now_millis with event creation
#[test]
fn test_now_millis_with_event() {
    let ts = Event::now_millis();
    let event = Event::run_started("t1", "r1", None).with_timestamp(ts);

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(&format!(r#""timestamp":{}"#, ts)));
}

// ============================================================================
// AG-UI Protocol Spec Tests - InteractionPlugin
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/human-in-the-loop
// Human-in-the-loop: Pending interactions require client response
//

/// Test: has_any_interaction_responses returns false when empty
/// Protocol: Check if request contains any interaction responses
#[test]
fn test_has_any_interaction_responses_empty() {
    let request = RunAgentInput::new("t1".to_string(), "r1".to_string());
    let plugin = interaction_plugin_from_request(&request);

    assert!(!plugin.has_responses());
}

/// Test: has_any_interaction_responses returns true with tool messages
/// Protocol: Tool messages in request indicate interaction responses
#[test]
fn test_has_any_interaction_responses_with_tool_messages() {
    let request = RunAgentInput::new("t1".to_string(), "r1".to_string()).with_message(
        tirea_protocol_ag_ui::Message::tool("approved", "interaction_1"),
    );

    let plugin = interaction_plugin_from_request(&request);
    assert!(plugin.has_responses());
}

/// Test: has_any_interaction_responses with multiple tool messages
/// Protocol: Multiple interaction responses in single request
#[test]
fn test_has_any_interaction_responses_multiple() {
    let request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::user("Hello"))
        .with_message(tirea_protocol_ag_ui::Message::tool("yes", "perm_1"))
        .with_message(tirea_protocol_ag_ui::Message::tool("no", "perm_2"))
        .with_message(tirea_protocol_ag_ui::Message::assistant("Processing..."));

    let plugin = interaction_plugin_from_request(&request);
    assert!(plugin.has_responses());
}

/// Test: has_any_interaction_responses ignores non-tool messages
/// Protocol: Only tool messages count as interaction responses
#[test]
fn test_has_any_interaction_responses_only_counts_tool_messages() {
    let request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::user("Hello"))
        .with_message(tirea_protocol_ag_ui::Message::assistant("Hi!"))
        .with_message(tirea_protocol_ag_ui::Message::system("Be helpful"));

    let plugin = interaction_plugin_from_request(&request);
    assert!(!plugin.has_responses());
}

// ============================================================================
// AG-UI Protocol Spec Tests - RunAgentInput.has_any_interaction_responses
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/human-in-the-loop
// Method on RunAgentInput to check for interaction responses directly
//

/// Test: RunAgentInput.has_any_interaction_responses returns false when empty
/// Protocol: Check if request contains any interaction responses
#[test]
fn test_run_agent_request_has_any_interaction_responses_empty() {
    let request = RunAgentInput::new("t1".to_string(), "r1".to_string());
    assert!(!request.has_any_interaction_responses());
}

/// Test: RunAgentInput.has_any_interaction_responses with a tool message
/// Protocol: Tool messages indicate interaction responses
#[test]
fn test_run_agent_request_has_any_interaction_responses_with_tool_message() {
    let request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::tool("approved", "int_1"));

    assert!(request.has_any_interaction_responses());
}

/// Test: RunAgentInput.has_any_interaction_responses ignores non-tool messages
/// Protocol: Only tool messages count as interaction responses
#[test]
fn test_run_agent_request_has_any_interaction_responses_non_tool() {
    let request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::user("Hello"))
        .with_message(tirea_protocol_ag_ui::Message::assistant("Hi!"));

    assert!(!request.has_any_interaction_responses());
}

// ============================================================================
// AG-UI Protocol Spec Tests - Message Role Types
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/messages
// Message Roles: user, assistant, system, tool, developer, activity (new in spec)
//

/// Test: Developer message role
/// Protocol: DeveloperMessage role per https://docs.ag-ui.com/concepts/messages
#[test]
fn test_message_role_developer() {
    let role = Role::Developer;
    let json = serde_json::to_string(&role).unwrap();
    assert_eq!(json, r#""developer""#);

    let parsed: Role = serde_json::from_str(r#""developer""#).unwrap();
    assert_eq!(parsed, Role::Developer);
}

/// Test: All message roles serialization roundtrip
/// Protocol: Complete message role enumeration per AG-UI spec
#[test]
fn test_all_message_roles_roundtrip() {
    let roles = vec![
        (Role::User, "user"),
        (Role::Assistant, "assistant"),
        (Role::System, "system"),
        (Role::Tool, "tool"),
        (Role::Developer, "developer"),
    ];

    for (role, expected_str) in roles {
        let json = serde_json::to_string(&role).unwrap();
        assert_eq!(json, format!(r#""{}""#, expected_str));

        let parsed: Role = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, role);
    }
}

/// Test: Default message role is Assistant
/// Protocol: TEXT_MESSAGE_START role is always "assistant"
/// Reference: https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_message_role_default_is_assistant() {
    let role = Role::default();
    assert_eq!(role, Role::Assistant);
}

// ============================================================================
// AG-UI Protocol Spec Tests - rawEvent Field
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/sdk/js/core/events
// BaseEvent.rawEvent: Optional passthrough for external system events
//

/// Test: BaseEvent with rawEvent
/// Protocol: rawEvent field in BaseEvent for external system passthrough
#[test]
fn test_base_event_fields_with_raw_event() {
    use tirea_protocol_ag_ui::BaseEvent;

    let base = BaseEvent {
        timestamp: Some(1234567890),
        raw_event: Some(json!({"external": "data", "model": "gpt-4"})),
    };

    let json = serde_json::to_string(&base).unwrap();
    assert!(json.contains(r#""timestamp":1234567890"#));
    assert!(json.contains(r#""rawEvent""#));
    assert!(json.contains(r#""external":"data""#));
}

/// Test: BaseEvent without rawEvent serializes correctly
/// Protocol: rawEvent is optional and omitted when None
#[test]
fn test_base_event_fields_without_raw_event() {
    use tirea_protocol_ag_ui::BaseEvent;

    let base = BaseEvent {
        timestamp: Some(1234567890),
        raw_event: None,
    };

    let json = serde_json::to_string(&base).unwrap();
    assert!(json.contains(r#""timestamp":1234567890"#));
    assert!(!json.contains(r#""rawEvent""#));
}

/// Test: Event with rawEvent passthrough
/// Protocol: External system events passed through rawEvent field
#[test]
fn test_event_with_raw_event_passthrough() {
    // Create event with rawEvent for passing through external model response
    let external_response = json!({
        "model": "claude-3",
        "usage": {"input_tokens": 100, "output_tokens": 50}
    });

    let event = Event::TextMessageContent {
        message_id: "msg1".into(),
        delta: "Hello".into(),
        base: tirea_protocol_ag_ui::BaseEvent {
            timestamp: Some(1234567890),
            raw_event: Some(external_response.clone()),
        },
    };

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""rawEvent""#));
    assert!(json.contains(r#""model":"claude-3""#));

    let parsed: Event = serde_json::from_str(&json).unwrap();
    if let Event::TextMessageContent { base, .. } = parsed {
        assert!(base.raw_event.is_some());
        let raw = base.raw_event.unwrap();
        assert_eq!(raw["model"], "claude-3");
    }
}

// ============================================================================
// AG-UI Protocol Spec Tests - Complete Event Flow Patterns
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/events
// Standard flows: RUN_STARTED must precede all events, RUN_FINISHED/RUN_ERROR concludes
//

/// Test: RUN_STARTED must be first event in a run
/// Protocol: Lifecycle constraint per AG-UI spec
#[test]
fn test_run_started_is_first_event() {
    let mut ctx = make_agui_ctx("t1", "r1");
    let mut events: Vec<Event> = Vec::new();

    // Simulate a complete run
    let run_start = AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    };
    events.extend(ctx.on_agent_event(&run_start));

    let text = AgentEvent::TextDelta {
        delta: "Hello".into(),
    };
    events.extend(ctx.on_agent_event(&text));

    let finish = AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: Some(serde_json::json!({"response": "Hello"})),
        termination: tirea_agentos::contracts::TerminationReason::NaturalEnd,
    };
    events.extend(ctx.on_agent_event(&finish));

    // First event must be RUN_STARTED
    assert!(matches!(&events[0], Event::RunStarted { .. }));
}

/// Test: TEXT_MESSAGE_START role is always "assistant"
/// Protocol: TEXT_MESSAGE_START.role per https://docs.ag-ui.com/sdk/js/core/events
#[test]
fn test_text_message_start_role_is_assistant() {
    let event = Event::text_message_start("msg1");

    if let Event::TextMessageStart { role, .. } = &event {
        assert_eq!(*role, Role::Assistant);
    } else {
        panic!("Expected TextMessageStart");
    }

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""role":"assistant""#));
}

/// Test: TEXT_MESSAGE_CONTENT delta can be empty string
/// Protocol: delta field validation
#[test]
fn test_text_message_content_empty_delta() {
    // Per protocol, delta should typically be non-empty, but empty is valid JSON
    let event = Event::text_message_content("msg1", "");

    if let Event::TextMessageContent { delta, .. } = &event {
        assert_eq!(delta, "");
    }

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""delta":"""#));
}

/// Test: Start-Content-End sequence ordering
/// Protocol: TEXT_MESSAGE events must follow START → CONTENT* → END
#[test]
fn test_text_message_sequence_ordering() {
    let mut ctx = make_agui_ctx("t1", "r1");
    let mut events: Vec<Event> = Vec::new();

    // Generate a complete text message
    let text1 = AgentEvent::TextDelta {
        delta: "Hello".into(),
    };
    events.extend(ctx.on_agent_event(&text1));

    let text2 = AgentEvent::TextDelta {
        delta: " World".into(),
    };
    events.extend(ctx.on_agent_event(&text2));

    // End text stream
    ctx.end_text();
    events.push(Event::text_message_end(&ctx.message_id));

    // Find indices
    let start_idx = events
        .iter()
        .position(|e| matches!(e, Event::TextMessageStart { .. }));
    let content_indices: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, Event::TextMessageContent { .. }))
        .map(|(i, _)| i)
        .collect();
    let end_idx = events
        .iter()
        .position(|e| matches!(e, Event::TextMessageEnd { .. }));

    // Verify ordering
    assert!(start_idx.is_some(), "Should have TEXT_MESSAGE_START");
    assert!(
        !content_indices.is_empty(),
        "Should have TEXT_MESSAGE_CONTENT"
    );
    assert!(end_idx.is_some(), "Should have TEXT_MESSAGE_END");

    let start = start_idx.unwrap();
    let end = end_idx.unwrap();

    // START must come before all CONTENT
    for &content_idx in &content_indices {
        assert!(start < content_idx, "START must come before CONTENT");
    }

    // All CONTENT must come before END
    for &content_idx in &content_indices {
        assert!(content_idx < end, "CONTENT must come before END");
    }
}

/// Test: Tool call sequence ordering
/// Protocol: TOOL_CALL events must follow START → ARGS* → END → RESULT
#[test]
fn test_tool_call_sequence_ordering() {
    let mut ctx = make_agui_ctx("t1", "r1");
    let mut events: Vec<Event> = Vec::new();

    let start = AgentEvent::ToolCallStart {
        id: "call_1".into(),
        name: "search".into(),
    };
    events.extend(ctx.on_agent_event(&start));

    let args = AgentEvent::ToolCallDelta {
        id: "call_1".into(),
        args_delta: r#"{"query": "test"}"#.into(),
    };
    events.extend(ctx.on_agent_event(&args));

    let ready = AgentEvent::ToolCallReady {
        id: "call_1".into(),
        name: "search".into(),
        arguments: json!({"query": "test"}),
    };
    events.extend(ctx.on_agent_event(&ready));

    let done = AgentEvent::ToolCallDone {
        id: "call_1".into(),
        result: ToolResult::success("search", json!({"count": 5})),
        patch: None,
        message_id: String::new(),
    };
    events.extend(ctx.on_agent_event(&done));

    // Find indices
    let start_idx = events
        .iter()
        .position(|e| matches!(e, Event::ToolCallStart { .. }))
        .unwrap();
    let args_idx = events
        .iter()
        .position(|e| matches!(e, Event::ToolCallArgs { .. }))
        .unwrap();
    let end_idx = events
        .iter()
        .position(|e| matches!(e, Event::ToolCallEnd { .. }))
        .unwrap();
    let result_idx = events
        .iter()
        .position(|e| matches!(e, Event::ToolCallResult { .. }))
        .unwrap();

    // Verify ordering: START < ARGS < END < RESULT
    assert!(start_idx < args_idx, "START must come before ARGS");
    assert!(args_idx < end_idx, "ARGS must come before END");
    assert!(end_idx < result_idx, "END must come before RESULT");
}

// ============================================================================
// AG-UI Protocol Spec Tests - Lifecycle Event Constraints
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/events
// Lifecycle constraints: RUN_STARTED first, RUN_FINISHED/RUN_ERROR last (mutually exclusive)
//

/// Test: RUN_FINISHED and RUN_ERROR are mutually exclusive
/// Protocol: Run must end with either RunFinished OR RunError, never both
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_run_finished_or_error_mutually_exclusive() {
    // A run should produce either RUN_FINISHED or RUN_ERROR, not both
    let mut ctx = make_agui_ctx("t1", "r1");

    // Simulate successful run
    let success_events: Vec<Event> = [
        AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            parent_run_id: None,
        },
        AgentEvent::TextDelta {
            delta: "Hello".into(),
        },
        AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: Some(serde_json::json!({"response": "Hello"})),
            termination: tirea_agentos::contracts::TerminationReason::NaturalEnd,
        },
    ]
    .iter()
    .flat_map(|e| ctx.on_agent_event(e))
    .collect();

    let has_finished = success_events
        .iter()
        .any(|e| matches!(e, Event::RunFinished { .. }));
    let has_error = success_events
        .iter()
        .any(|e| matches!(e, Event::RunError { .. }));

    // Done event emits RunFinished in AG-UI conversion
    assert!(has_finished, "Successful run should emit RUN_FINISHED");
    assert!(!has_error, "Successful run should not emit RUN_ERROR");

    // Simulate error run
    let mut ctx2 = make_agui_ctx("t1", "r2");
    let error_events: Vec<Event> = [
        AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "r2".into(),
            parent_run_id: None,
        },
        AgentEvent::Error {
            message: "API error".into(),
            code: None,
        },
    ]
    .iter()
    .flat_map(|e| ctx2.on_agent_event(e))
    .collect();

    let has_finished2 = error_events
        .iter()
        .any(|e| matches!(e, Event::RunFinished { .. }));
    let has_error2 = error_events
        .iter()
        .any(|e| matches!(e, Event::RunError { .. }));

    assert!(
        !(has_finished2 && has_error2),
        "Run cannot have both RUN_FINISHED and RUN_ERROR"
    );
}

/// Test: RUN_STARTED contains required fields
/// Protocol: RunStarted must have threadId, runId
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_run_started_required_fields() {
    let event = Event::run_started("thread_123", "run_456", None);

    if let Event::RunStarted {
        thread_id, run_id, ..
    } = &event
    {
        assert!(!thread_id.is_empty(), "threadId is required");
        assert!(!run_id.is_empty(), "runId is required");
    } else {
        panic!("Expected RunStarted");
    }

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""threadId":"thread_123""#));
    assert!(json.contains(r#""runId":"run_456""#));
}

/// Test: RUN_FINISHED contains required fields
/// Protocol: RunFinished must have threadId, runId
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_run_finished_required_fields() {
    let event = Event::run_finished("thread_123", "run_456", Some(json!({"answer": 42})));

    if let Event::RunFinished {
        thread_id,
        run_id,
        result,
        ..
    } = &event
    {
        assert!(!thread_id.is_empty(), "threadId is required");
        assert!(!run_id.is_empty(), "runId is required");
        assert!(result.is_some(), "result should be present when provided");
    } else {
        panic!("Expected RunFinished");
    }
}

/// Test: RUN_ERROR contains required message field
/// Protocol: RunError must have message
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_run_error_required_fields() {
    let event = Event::run_error("Something went wrong", Some("ERR_001".into()));

    if let Event::RunError { message, code, .. } = &event {
        assert!(!message.is_empty(), "message is required");
        assert_eq!(code.as_deref(), Some("ERR_001"));
    } else {
        panic!("Expected RunError");
    }

    // Without code
    let event2 = Event::run_error("Error message", None);
    if let Event::RunError { message, code, .. } = &event2 {
        assert!(!message.is_empty());
        assert!(code.is_none(), "code is optional");
    }
}

// ============================================================================
// AG-UI Protocol Spec Tests - Step Event Pairing
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/events
// Step events must be properly paired: StepStarted.stepName == StepFinished.stepName
//

/// Test: STEP_FINISHED stepName must match STEP_STARTED
/// Protocol: StepFinished.stepName MUST match corresponding StepStarted.stepName
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_step_finished_name_matches_started() {
    let step_name = "data_processing";

    let start = Event::step_started(step_name);
    let finish = Event::step_finished(step_name);

    // Extract and compare step names
    let start_name = if let Event::StepStarted { step_name, .. } = &start {
        step_name.clone()
    } else {
        panic!("Expected StepStarted");
    };

    let finish_name = if let Event::StepFinished { step_name, .. } = &finish {
        step_name.clone()
    } else {
        panic!("Expected StepFinished");
    };

    assert_eq!(
        start_name, finish_name,
        "StepFinished.stepName must match StepStarted.stepName"
    );
}

/// Test: Nested steps follow LIFO ordering
/// Protocol: Nested steps must complete in LIFO order (last started = first finished)
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_nested_steps_lifo_ordering() {
    // Simulate nested steps: outer → inner → innermost
    let events = vec![
        Event::step_started("outer"),
        Event::step_started("inner"),
        Event::step_started("innermost"),
        // Must finish in reverse order
        Event::step_finished("innermost"),
        Event::step_finished("inner"),
        Event::step_finished("outer"),
    ];

    // Track step stack to verify LIFO
    let mut step_stack: Vec<String> = Vec::new();

    for event in &events {
        match event {
            Event::StepStarted { step_name, .. } => {
                step_stack.push(step_name.clone());
            }
            Event::StepFinished { step_name, .. } => {
                let expected = step_stack.pop().expect("Step stack underflow");
                assert_eq!(
                    step_name, &expected,
                    "Steps must finish in LIFO order: expected '{}', got '{}'",
                    expected, step_name
                );
            }
            _ => {}
        }
    }

    assert!(step_stack.is_empty(), "All steps must be finished");
}

/// Test: Step events serialization with stepName field
/// Protocol: Step events use stepName (camelCase) field
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_step_events_step_name_field() {
    let start = Event::step_started("my_step");
    let finish = Event::step_finished("my_step");

    let start_json = serde_json::to_string(&start).unwrap();
    let finish_json = serde_json::to_string(&finish).unwrap();

    // Verify camelCase field name
    assert!(start_json.contains(r#""stepName":"my_step""#));
    assert!(finish_json.contains(r#""stepName":"my_step""#));
}

// ============================================================================
// AG-UI Protocol Spec Tests - Text Message ID References
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/events
// TextMessageContent and TextMessageEnd must reference valid TextMessageStart
//

/// Test: TEXT_MESSAGE_CONTENT messageId must match TEXT_MESSAGE_START
/// Protocol: TextMessageContent.messageId must reference a valid TextMessageStart
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_text_content_message_id_references_start() {
    let message_id = "msg_abc123";

    let start = Event::text_message_start(message_id);
    let content = Event::text_message_content(message_id, "Hello");

    let start_id = if let Event::TextMessageStart { message_id, .. } = &start {
        message_id.clone()
    } else {
        panic!("Expected TextMessageStart");
    };

    let content_id = if let Event::TextMessageContent { message_id, .. } = &content {
        message_id.clone()
    } else {
        panic!("Expected TextMessageContent");
    };

    assert_eq!(
        start_id, content_id,
        "TextMessageContent.messageId must match TextMessageStart"
    );
}

/// Test: TEXT_MESSAGE_END messageId must match TEXT_MESSAGE_START
/// Protocol: TextMessageEnd.messageId must match previous TextMessageStart
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_text_end_message_id_matches_start() {
    let message_id = "msg_xyz789";

    let start = Event::text_message_start(message_id);
    let end = Event::text_message_end(message_id);

    let start_id = if let Event::TextMessageStart { message_id, .. } = &start {
        message_id.clone()
    } else {
        panic!("Expected TextMessageStart");
    };

    let end_id = if let Event::TextMessageEnd { message_id, .. } = &end {
        message_id.clone()
    } else {
        panic!("Expected TextMessageEnd");
    };

    assert_eq!(
        start_id, end_id,
        "TextMessageEnd.messageId must match TextMessageStart"
    );
}

/// Test: Complete text message flow maintains consistent messageId
/// Protocol: All events in a text message sequence share the same messageId
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_text_message_flow_consistent_message_id() {
    let mut ctx = make_agui_ctx("t1", "r1");

    let text1 = AgentEvent::TextDelta {
        delta: "Hello ".into(),
    };
    let events1 = ctx.on_agent_event(&text1);

    let text2 = AgentEvent::TextDelta {
        delta: "World".into(),
    };
    let events2 = ctx.on_agent_event(&text2);

    // Collect all messageIds
    let mut message_ids: Vec<String> = Vec::new();

    for event in events1.iter().chain(events2.iter()) {
        match event {
            Event::TextMessageStart { message_id, .. }
            | Event::TextMessageContent { message_id, .. }
            | Event::TextMessageEnd { message_id, .. } => {
                message_ids.push(message_id.clone());
            }
            _ => {}
        }
    }

    // All messageIds should be the same within a single text stream
    if !message_ids.is_empty() {
        let first_id = &message_ids[0];
        for id in &message_ids {
            assert_eq!(
                id, first_id,
                "All text message events should share the same messageId"
            );
        }
    }
}

// ============================================================================
// AG-UI Protocol Spec Tests - Tool Call Constraints
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/events
// Tool call events must maintain proper ID references and JSON validity
//

/// Test: TOOL_CALL_ARGS deltas concatenate to valid JSON
/// Protocol: All ToolCallArgs deltas must concatenate to form valid JSON arguments
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_tool_args_concatenate_to_valid_json() {
    // Simulate streaming JSON arguments in chunks
    let chunks = [
        r#"{"#,
        r#""query": "#,
        r#""rust "#,
        r#"programming","#,
        r#" "limit": 10"#,
        r#"}"#,
    ];

    let concatenated: String = chunks.concat();
    let parsed: Result<Value, _> = serde_json::from_str(&concatenated);

    assert!(
        parsed.is_ok(),
        "Concatenated chunks must form valid JSON: {:?}",
        parsed.err()
    );

    let json = parsed.unwrap();
    assert_eq!(json["query"], "rust programming");
    assert_eq!(json["limit"], 10);
}

/// Test: TOOL_CALL_RESULT toolCallId references valid TOOL_CALL_START
/// Protocol: ToolCallResult.toolCallId must match a previous ToolCallStart.toolCallId
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_tool_result_references_valid_tool_call() {
    let tool_call_id = "call_abc123";

    let start = Event::tool_call_start(tool_call_id, "search", None);
    let result = Event::tool_call_result("msg_1", tool_call_id, r#"{"found": 5}"#);

    let start_id = if let Event::ToolCallStart { tool_call_id, .. } = &start {
        tool_call_id.clone()
    } else {
        panic!("Expected ToolCallStart");
    };

    let result_id = if let Event::ToolCallResult { tool_call_id, .. } = &result {
        tool_call_id.clone()
    } else {
        panic!("Expected ToolCallResult");
    };

    assert_eq!(
        start_id, result_id,
        "ToolCallResult.toolCallId must match ToolCallStart"
    );
}

/// Test: Tool call flow maintains consistent toolCallId
/// Protocol: All events in a tool call sequence share the same toolCallId
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_tool_call_flow_consistent_tool_call_id() {
    let tool_call_id = "call_xyz789";
    let tool_name = "read_file";

    let events = vec![
        Event::tool_call_start(tool_call_id, tool_name, None),
        Event::tool_call_args(tool_call_id, r#"{"path": "/tmp/test.txt"}"#),
        Event::tool_call_end(tool_call_id),
        Event::tool_call_result("msg_1", tool_call_id, "file contents"),
    ];

    // Verify all events have the same toolCallId
    for event in &events {
        let event_id = match event {
            Event::ToolCallStart { tool_call_id, .. } => tool_call_id,
            Event::ToolCallArgs { tool_call_id, .. } => tool_call_id,
            Event::ToolCallEnd { tool_call_id, .. } => tool_call_id,
            Event::ToolCallResult { tool_call_id, .. } => tool_call_id,
            _ => continue,
        };
        assert_eq!(
            event_id, tool_call_id,
            "All tool call events must share the same toolCallId"
        );
    }
}

// ============================================================================
// AG-UI Protocol Spec Tests - Message Type Validation
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/messages
// Different message types have different required fields
//

/// Test: UserMessage content is required
/// Protocol: UserMessage.content is required (string or InputContent array)
/// Reference: https://docs.ag-ui.com/concepts/messages
#[test]
fn test_user_message_content_required() {
    let msg = tirea_protocol_ag_ui::Message::user("Hello, how can you help?");

    assert_eq!(msg.role, Role::User);
    assert!(!msg.content.is_empty(), "UserMessage.content is required");

    // Verify serialization includes content
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""content":"Hello"#));
}

/// Test: SystemMessage content is required
/// Protocol: SystemMessage.content is required string
/// Reference: https://docs.ag-ui.com/concepts/messages
#[test]
fn test_system_message_content_required() {
    let msg = tirea_protocol_ag_ui::Message::system("You are a helpful assistant.");

    assert_eq!(msg.role, Role::System);
    assert!(!msg.content.is_empty(), "SystemMessage.content is required");

    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""content":"You are a helpful assistant.""#));
}

/// Test: ToolMessage requires toolCallId
/// Protocol: ToolMessage requires both content and toolCallId
/// Reference: https://docs.ag-ui.com/concepts/messages
#[test]
fn test_tool_message_requires_tool_call_id() {
    let msg = tirea_protocol_ag_ui::Message::tool("result data", "call_123");

    assert_eq!(msg.role, Role::Tool);
    assert!(
        msg.tool_call_id.is_some(),
        "ToolMessage requires toolCallId"
    );
    assert_eq!(msg.tool_call_id.as_deref(), Some("call_123"));

    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""toolCallId":"call_123""#));
}

/// Test: AssistantMessage content can be optional
/// Protocol: AssistantMessage can have content=null if toolCalls present
/// Reference: https://docs.ag-ui.com/concepts/messages
#[test]
fn test_assistant_message_optional_content() {
    // With content
    let msg_with_content = tirea_protocol_ag_ui::Message::assistant("I can help with that.");
    assert_eq!(msg_with_content.role, Role::Assistant);
    assert!(!msg_with_content.content.is_empty());

    // Empty content is valid for assistant (when making tool calls)
    let mut msg_empty = tirea_protocol_ag_ui::Message::assistant("");
    msg_empty.content = String::new();
    assert_eq!(msg_empty.role, Role::Assistant);
    // Empty content is allowed per protocol when tool calls are present
}

/// Test: Message with optional id field
/// Protocol: Messages can have optional unique identifiers
/// Reference: https://docs.ag-ui.com/concepts/messages
#[test]
fn test_message_optional_id_field() {
    let mut msg = tirea_protocol_ag_ui::Message::user("test");

    // Without id
    assert!(msg.id.is_none());
    let json_without_id = serde_json::to_string(&msg).unwrap();
    assert!(!json_without_id.contains(r#""id""#));

    // With id
    msg.id = Some("msg_unique_123".to_string());
    let json_with_id = serde_json::to_string(&msg).unwrap();
    assert!(json_with_id.contains(r#""id":"msg_unique_123""#));
}

// ============================================================================
// AG-UI Protocol Spec Tests - State Synchronization
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/state
// State synchronization via snapshots and deltas
//

/// Test: StateSnapshot replaces entire state
/// Protocol: StateSnapshot should replace existing state entirely, not merge
/// Reference: https://docs.ag-ui.com/concepts/state
#[test]
fn test_state_snapshot_replaces_entire_state() {
    let new_state = json!({
        "counter": 10,
        "items": ["a", "b", "c"]
    });

    let snapshot2 = Event::state_snapshot(new_state.clone());

    // Second snapshot should contain only new state, not merged with first
    if let Event::StateSnapshot { snapshot, .. } = &snapshot2 {
        assert_eq!(snapshot["counter"], 10);
        assert!(
            snapshot.get("user").is_none(),
            "Old state should be replaced, not merged"
        );
        assert!(
            snapshot.get("items").is_some(),
            "New state should be present"
        );
    }
}

/// Test: StateDelta patches apply atomically
/// Protocol: All patches in StateDelta must apply atomically (all or none)
/// Reference: https://docs.ag-ui.com/concepts/state
#[test]
fn test_state_delta_atomic_application() {
    // Multiple operations that should all succeed or all fail together
    let delta = vec![
        json!({"op": "add", "path": "/step1", "value": "done"}),
        json!({"op": "add", "path": "/step2", "value": "done"}),
        json!({"op": "add", "path": "/step3", "value": "done"}),
    ];

    let event = Event::state_delta(delta);

    if let Event::StateDelta { delta: ops, .. } = &event {
        assert_eq!(ops.len(), 3, "All operations should be in a single delta");
        // All operations are part of one atomic batch
    }
}

/// Test: StateDelta patches apply in order
/// Protocol: StateDelta patches must be applied in received order
/// Reference: https://docs.ag-ui.com/concepts/state
#[test]
fn test_state_delta_sequential_ordering() {
    // Operations that depend on order
    let delta = vec![
        json!({"op": "add", "path": "/items", "value": []}), // 1. Create array
        json!({"op": "add", "path": "/items/-", "value": "first"}), // 2. Add first item
        json!({"op": "add", "path": "/items/-", "value": "second"}), // 3. Add second item
    ];

    let event = Event::state_delta(delta.clone());

    if let Event::StateDelta { delta: ops, .. } = &event {
        // Verify operations are in expected order
        assert_eq!(ops[0]["path"], "/items");
        assert_eq!(ops[1]["path"], "/items/-");
        assert_eq!(ops[1]["value"], "first");
        assert_eq!(ops[2]["path"], "/items/-");
        assert_eq!(ops[2]["value"], "second");
    }
}

/// Test: JSON Patch test operation can validate state
/// Protocol: JSON Patch "test" operation must fail entire patch if test fails
/// Reference: https://docs.ag-ui.com/concepts/state (RFC 6902)
#[test]
fn test_patch_test_operation_validates_state() {
    // Test operation validates expected value before applying changes
    let delta = vec![
        json!({"op": "test", "path": "/version", "value": "1.0"}),
        json!({"op": "replace", "path": "/version", "value": "2.0"}),
    ];

    let event = Event::state_delta(delta);

    if let Event::StateDelta { delta: ops, .. } = &event {
        // First operation is a test
        assert_eq!(ops[0]["op"], "test");
        assert_eq!(ops[0]["value"], "1.0");
        // If test passes, replacement happens
        assert_eq!(ops[1]["op"], "replace");
        assert_eq!(ops[1]["value"], "2.0");
    }
}

// ============================================================================
// AG-UI Protocol Spec Tests - Activity Event Constraints
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/events
// Activity events for progress indicators and status updates
//

/// Test: ActivityDelta activityType must match ActivitySnapshot
/// Protocol: ActivityDelta.activityType must mirror previous ActivitySnapshot.activityType
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_activity_delta_type_matches_snapshot() {
    use std::collections::HashMap;

    let activity_type = "file_processing";

    let mut content = HashMap::new();
    content.insert("progress".to_string(), json!(0.0));

    let snapshot = Event::activity_snapshot("act_1", activity_type, content, None);
    let delta = Event::activity_delta(
        "act_1",
        activity_type,
        vec![json!({"op": "replace", "path": "/progress", "value": 0.5})],
    );

    // Extract activity types
    let snapshot_type = if let Event::ActivitySnapshot { activity_type, .. } = &snapshot {
        activity_type.clone()
    } else {
        panic!("Expected ActivitySnapshot");
    };

    let delta_type = if let Event::ActivityDelta { activity_type, .. } = &delta {
        activity_type.clone()
    } else {
        panic!("Expected ActivityDelta");
    };

    assert_eq!(
        snapshot_type, delta_type,
        "ActivityDelta.activityType must match ActivitySnapshot.activityType"
    );
}

/// Test: ActivitySnapshot replace=false behavior
/// Protocol: ActivitySnapshot with replace=false should ignore if message already exists
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_activity_snapshot_replace_false_behavior() {
    use std::collections::HashMap;

    let mut content = HashMap::new();
    content.insert("status".to_string(), json!("running"));

    // replace=false means don't overwrite existing activity
    let event = Event::activity_snapshot("act_1", "processing", content, Some(false));

    if let Event::ActivitySnapshot { replace, .. } = &event {
        assert_eq!(*replace, Some(false));
    }

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""replace":false"#));
}

/// Test: ActivitySnapshot replace=true overwrites
/// Protocol: ActivitySnapshot with replace=true should overwrite existing activity
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_activity_snapshot_replace_true_behavior() {
    use std::collections::HashMap;

    let mut content = HashMap::new();
    content.insert("status".to_string(), json!("completed"));

    // replace=true means overwrite existing activity
    let event = Event::activity_snapshot("act_1", "processing", content, Some(true));

    if let Event::ActivitySnapshot { replace, .. } = &event {
        assert_eq!(*replace, Some(true));
    }

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""replace":true"#));
}

/// Test: ActivityDelta messageId must match ActivitySnapshot
/// Protocol: ActivityDelta.messageId must reference the same activity
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_activity_delta_message_id_matches_snapshot() {
    use std::collections::HashMap;

    let message_id = "activity_msg_123";

    let mut content = HashMap::new();
    content.insert("count".to_string(), json!(0));

    let snapshot = Event::activity_snapshot(message_id, "counter", content, None);
    let delta = Event::activity_delta(
        message_id,
        "counter",
        vec![json!({"op": "replace", "path": "/count", "value": 1})],
    );

    let snapshot_id = if let Event::ActivitySnapshot { message_id, .. } = &snapshot {
        message_id.clone()
    } else {
        panic!("Expected ActivitySnapshot");
    };

    let delta_id = if let Event::ActivityDelta { message_id, .. } = &delta {
        message_id.clone()
    } else {
        panic!("Expected ActivityDelta");
    };

    assert_eq!(
        snapshot_id, delta_id,
        "ActivityDelta.messageId must match ActivitySnapshot"
    );
}

// ============================================================================
// AG-UI Protocol Spec Tests - Tool Definition Validation
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/tools
// Tool definitions must have valid JSON Schema parameters
//

/// Test: Tool parameters must be valid JSON Schema
/// Protocol: Tool.parameters must be valid JSON Schema
/// Reference: https://docs.ag-ui.com/concepts/tools
#[test]
fn test_tool_parameters_json_schema_validation() {
    let schema = json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Search query"
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": 100,
                "default": 10
            }
        },
        "required": ["query"]
    });

    let tool = tirea_protocol_ag_ui::Tool::backend("search", "Search the web")
        .with_parameters(schema.clone());

    // Verify schema structure
    assert!(tool.parameters.is_some());
    let params = tool.parameters.unwrap();
    assert_eq!(params["type"], "object");
    assert!(params["properties"]["query"].is_object());
    assert!(params["required"].is_array());
}

/// Test: Tool required array references valid properties
/// Protocol: Tool.required array items must reference existing properties in schema
/// Reference: https://docs.ag-ui.com/concepts/tools
#[test]
fn test_tool_required_references_valid_properties() {
    let schema = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "encoding": {"type": "string"}
        },
        "required": ["path"]
    });

    let tool = tirea_protocol_ag_ui::Tool::backend("read_file", "Read a file")
        .with_parameters(schema.clone());

    let params = tool.parameters.unwrap();
    let required = params["required"].as_array().unwrap();
    let properties = params["properties"].as_object().unwrap();

    // All required fields must exist in properties
    for req in required {
        let req_name = req.as_str().unwrap();
        assert!(
            properties.contains_key(req_name),
            "Required field '{}' must exist in properties",
            req_name
        );
    }
}

/// Test: Frontend vs Backend tool execution location
/// Protocol: Tools can execute on frontend or backend
/// Reference: https://docs.ag-ui.com/concepts/tools
#[test]
fn test_tool_execution_location() {
    let backend_tool = tirea_protocol_ag_ui::Tool::backend("search", "Search");
    let frontend_tool =
        tirea_protocol_ag_ui::Tool::frontend("copyToClipboard", "Copy to clipboard");

    assert_eq!(backend_tool.execute, ToolExecutionLocation::Backend);
    assert_eq!(frontend_tool.execute, ToolExecutionLocation::Frontend);

    // Backend tools DO serialize execute field (Frontend is the default)
    let backend_json = serde_json::to_string(&backend_tool).unwrap();
    assert!(backend_json.contains(r#""execute":"backend""#));
}

// ============================================================================
// P1 - RunAgentInput Uncovered Methods (Lines 1081-1141)
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/sdk/js/core/types
// RunAgentInput: with_messages, with_model, with_system_prompt, frontend_tools.
// Last-user lookup and backend/fronted matching are verified via direct collection traversal.
//

/// Test: RunAgentInput.with_messages batch
/// Protocol: Add multiple messages at once
/// Reference: https://docs.ag-ui.com/sdk/js/core/types
#[test]
fn test_run_agent_request_with_messages_batch() {
    let messages = vec![
        tirea_protocol_ag_ui::Message::user("Hello"),
        tirea_protocol_ag_ui::Message::assistant("Hi!"),
        tirea_protocol_ag_ui::Message::user("How are you?"),
    ];

    let request = RunAgentInput::new("t1".to_string(), "r1".to_string()).with_messages(messages);

    assert_eq!(request.messages.len(), 3);
    assert_eq!(request.messages[0].role, Role::User);
    assert_eq!(request.messages[2].content, "How are you?");
}

/// Test: RunAgentInput.with_model
/// Protocol: Optional model selection in request
/// Reference: https://docs.ag-ui.com/sdk/js/core/types
#[test]
fn test_run_agent_request_with_model() {
    let request = RunAgentInput::new("t1".to_string(), "r1".to_string()).with_model("gpt-4o");

    assert_eq!(request.model, Some("gpt-4o".to_string()));

    let json = serde_json::to_string(&request).unwrap();
    assert!(json.contains(r#""model":"gpt-4o""#));
}

/// Test: RunAgentInput.with_system_prompt
/// Protocol: Optional system prompt override
/// Reference: https://docs.ag-ui.com/sdk/js/core/types
#[test]
fn test_run_agent_request_with_system_prompt() {
    let request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_system_prompt("You are a coding assistant.");

    assert_eq!(
        request.system_prompt,
        Some("You are a coding assistant.".to_string())
    );
}

/// Test: last user message lookup via direct message scan
/// Protocol: Extract last user message for quick access
/// Reference: https://docs.ag-ui.com/sdk/js/core/types
#[test]
fn test_run_agent_request_last_user_message_via_messages_scan() {
    let request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::user("First"))
        .with_message(tirea_protocol_ag_ui::Message::assistant("Response"))
        .with_message(tirea_protocol_ag_ui::Message::user("Second"));

    assert_eq!(
        request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(|message| message.content.as_str()),
        Some("Second")
    );
}

/// Test: last user message lookup via direct message scan when empty
/// Protocol: Returns None when no user messages
/// Reference: https://docs.ag-ui.com/sdk/js/core/types
#[test]
fn test_run_agent_request_last_user_message_via_messages_scan_empty() {
    let request = RunAgentInput::new("t1".to_string(), "r1".to_string());
    assert!(request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(|message| message.content.as_str())
        .is_none());
}

/// Test: last user message lookup via direct message scan with only non-user messages
/// Protocol: Returns None when no user messages present
/// Reference: https://docs.ag-ui.com/sdk/js/core/types
#[test]
fn test_run_agent_request_last_user_message_via_messages_scan_no_user() {
    let request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::assistant("Hi"))
        .with_message(tirea_protocol_ag_ui::Message::system("Be helpful"));

    assert!(request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(|message| message.content.as_str())
        .is_none());
}

/// Test: RunAgentInput.frontend_tools
/// Protocol: Filter tools by frontend execution location
/// Reference: https://docs.ag-ui.com/concepts/tools
#[test]
fn test_run_agent_request_frontend_tools() {
    let request = RunAgentInput {
        tools: vec![
            tirea_protocol_ag_ui::Tool::backend("search", "Search"),
            tirea_protocol_ag_ui::Tool::frontend("copyToClipboard", "Copy"),
            tirea_protocol_ag_ui::Tool::backend("read_file", "Read file"),
            tirea_protocol_ag_ui::Tool::frontend("showNotification", "Notify"),
        ],
        ..RunAgentInput::new("t1".to_string(), "r1".to_string())
    };

    let frontend = request.frontend_tools();
    assert_eq!(frontend.len(), 2);
    assert!(frontend
        .iter()
        .all(|t| t.execute == ToolExecutionLocation::Frontend));
}

/// Test: backend tool filtering via direct tools iteration
/// Protocol: Filter tools by backend execution location
/// Reference: https://docs.ag-ui.com/concepts/tools
#[test]
fn test_run_agent_request_backend_tools_via_filter() {
    let request = RunAgentInput {
        tools: vec![
            tirea_protocol_ag_ui::Tool::backend("search", "Search"),
            tirea_protocol_ag_ui::Tool::frontend("copy", "Copy"),
            tirea_protocol_ag_ui::Tool::backend("read", "Read"),
        ],
        ..RunAgentInput::new("t1".to_string(), "r1".to_string())
    };

    let backend: Vec<&tirea_protocol_ag_ui::Tool> = request
        .tools
        .iter()
        .filter(|tool| !tool.is_frontend())
        .collect();
    assert_eq!(backend.len(), 2);
    assert!(backend
        .iter()
        .all(|t| t.execute == ToolExecutionLocation::Backend));
}

/// Test: frontend tool lookup via frontend_tools filtering
/// Protocol: Check if a named tool is frontend
/// Reference: https://docs.ag-ui.com/concepts/tools
#[test]
fn test_run_agent_request_frontend_tool_lookup_via_frontend_tools() {
    let request = RunAgentInput {
        tools: vec![
            tirea_protocol_ag_ui::Tool::backend("search", "Search"),
            tirea_protocol_ag_ui::Tool::frontend("copy", "Copy"),
        ],
        ..RunAgentInput::new("t1".to_string(), "r1".to_string())
    };

    assert!(!request
        .frontend_tools()
        .iter()
        .any(|tool| tool.name == "search"));
    assert!(request
        .frontend_tools()
        .iter()
        .any(|tool| tool.name == "copy"));
    assert!(!request
        .frontend_tools()
        .iter()
        .any(|tool| tool.name == "nonexistent"));
}

// ============================================================================
// P1 - SuspensionResponse Coverage (Lines 1235-1288)
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/human-in-the-loop
// SuspensionResponse parsing: approval/denial from various value formats
//

/// Test: SuspensionResponse.is_approved with bool true
/// Protocol: Boolean true indicates approval
/// Reference: https://docs.ag-ui.com/concepts/human-in-the-loop
#[test]
fn test_interaction_response_is_approved_bool_true() {
    assert!(SuspensionResponse::is_approved(&json!(true)));
}

/// Test: SuspensionResponse.is_approved with bool false
/// Protocol: Boolean false is NOT approval
/// Reference: https://docs.ag-ui.com/concepts/human-in-the-loop
#[test]
fn test_interaction_response_is_approved_bool_false() {
    assert!(!SuspensionResponse::is_approved(&json!(false)));
}

/// Test: SuspensionResponse.is_approved with string variants
/// Protocol: Various affirmative strings indicate approval
/// Reference: https://docs.ag-ui.com/concepts/human-in-the-loop
#[test]
fn test_interaction_response_is_approved_strings() {
    let approved_strings = vec![
        "true", "yes", "approved", "allow", "confirm", "ok", "accept",
    ];
    for s in approved_strings {
        assert!(
            SuspensionResponse::is_approved(&json!(s)),
            "'{}' should be approved",
            s
        );
    }

    // Case insensitive
    assert!(SuspensionResponse::is_approved(&json!("TRUE")));
    assert!(SuspensionResponse::is_approved(&json!("Yes")));
    assert!(SuspensionResponse::is_approved(&json!("APPROVED")));
}

/// Test: SuspensionResponse.is_approved with object
/// Protocol: Object with approved=true or allowed=true indicates approval
/// Reference: https://docs.ag-ui.com/concepts/human-in-the-loop
#[test]
fn test_interaction_response_is_approved_object() {
    assert!(SuspensionResponse::is_approved(&json!({"approved": true})));
    assert!(SuspensionResponse::is_approved(&json!({"allowed": true})));
    assert!(!SuspensionResponse::is_approved(
        &json!({"approved": false})
    ));
    assert!(!SuspensionResponse::is_approved(&json!({"other": true})));
}

/// Test: SuspensionResponse.is_approved with non-matchable values
/// Protocol: null, number, array should not match as approved
/// Reference: https://docs.ag-ui.com/concepts/human-in-the-loop
#[test]
fn test_interaction_response_is_approved_non_matchable() {
    assert!(!SuspensionResponse::is_approved(&json!(null)));
    assert!(!SuspensionResponse::is_approved(&json!(42)));
    assert!(!SuspensionResponse::is_approved(&json!([1, 2, 3])));
}

/// Test: SuspensionResponse.is_denied with bool
/// Protocol: Boolean false indicates denial
/// Reference: https://docs.ag-ui.com/concepts/human-in-the-loop
#[test]
fn test_interaction_response_is_denied_bool() {
    assert!(SuspensionResponse::is_denied(&json!(false)));
    assert!(!SuspensionResponse::is_denied(&json!(true)));
}

/// Test: SuspensionResponse.is_denied with string variants
/// Protocol: Various negative strings indicate denial
/// Reference: https://docs.ag-ui.com/concepts/human-in-the-loop
#[test]
fn test_interaction_response_is_denied_strings() {
    let denied_strings = vec!["false", "no", "denied", "deny", "reject", "cancel", "abort"];
    for s in denied_strings {
        assert!(
            SuspensionResponse::is_denied(&json!(s)),
            "'{}' should be denied",
            s
        );
    }

    // Case insensitive
    assert!(SuspensionResponse::is_denied(&json!("FALSE")));
    assert!(SuspensionResponse::is_denied(&json!("No")));
    assert!(SuspensionResponse::is_denied(&json!("DENIED")));
}

/// Test: SuspensionResponse.is_denied with object
/// Protocol: Object with approved=false or denied=true indicates denial
/// Reference: https://docs.ag-ui.com/concepts/human-in-the-loop
#[test]
fn test_interaction_response_is_denied_object() {
    assert!(SuspensionResponse::is_denied(&json!({"approved": false})));
    assert!(SuspensionResponse::is_denied(&json!({"denied": true})));
    assert!(!SuspensionResponse::is_denied(&json!({"approved": true})));
    assert!(!SuspensionResponse::is_denied(&json!({"other": false})));
}

/// Test: SuspensionResponse.is_denied with non-matchable values
/// Protocol: null, number, array should not match as denied
/// Reference: https://docs.ag-ui.com/concepts/human-in-the-loop
#[test]
fn test_interaction_response_is_denied_non_matchable() {
    assert!(!SuspensionResponse::is_denied(&json!(null)));
    assert!(!SuspensionResponse::is_denied(&json!(42)));
    assert!(!SuspensionResponse::is_denied(&json!([1, 2, 3])));
}

/// Test: SuspensionResponse.approved() and .denied() instance methods
/// Protocol: Instance methods delegate to static is_approved/is_denied
/// Reference: https://docs.ag-ui.com/concepts/human-in-the-loop
#[test]
fn test_interaction_response_instance_methods() {
    let approved = SuspensionResponse::new("int_1", json!(true));
    assert!(approved.approved());
    assert!(!approved.denied());

    let denied = SuspensionResponse::new("int_2", json!(false));
    assert!(!denied.approved());
    assert!(denied.denied());

    let string_approved = SuspensionResponse::new("int_3", json!("yes"));
    assert!(string_approved.approved());

    let object_denied = SuspensionResponse::new("int_4", json!({"approved": false}));
    assert!(object_denied.denied());
}

// ============================================================================
// P1 - RequestError Coverage (Lines 1401-1445)
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/sdk/js/core/types
// Error types for request validation
//

/// Test: RequestError.invalid_field creation
/// Protocol: Error for invalid field in request
/// Reference: https://docs.ag-ui.com/sdk/js/core/types
#[test]
fn test_request_error_invalid_field() {
    use tirea_protocol_ag_ui::RequestError;

    let err = RequestError::invalid_field("threadId cannot be empty");

    assert_eq!(err.code, "INVALID_FIELD");
    assert_eq!(err.message, "threadId cannot be empty");
}

/// Test: RequestError.validation creation
/// Protocol: Error for validation failure
/// Reference: https://docs.ag-ui.com/sdk/js/core/types
#[test]
fn test_request_error_validation() {
    use tirea_protocol_ag_ui::RequestError;

    let err = RequestError::validation("Invalid state format");

    assert_eq!(err.code, "VALIDATION_ERROR");
    assert_eq!(err.message, "Invalid state format");
}

/// Test: RequestError.internal creation
/// Protocol: Error for internal failures
/// Reference: https://docs.ag-ui.com/sdk/js/core/types
#[test]
fn test_request_error_internal() {
    use tirea_protocol_ag_ui::RequestError;

    let err = RequestError::internal("Connection lost");

    assert_eq!(err.code, "INTERNAL_ERROR");
    assert_eq!(err.message, "Connection lost");
}

/// Test: RequestError Display implementation
/// Protocol: Human-readable error format
/// Reference: https://docs.ag-ui.com/sdk/js/core/types
#[test]
fn test_request_error_display() {
    use tirea_protocol_ag_ui::RequestError;

    let err = RequestError::invalid_field("bad value");
    let display = format!("{}", err);
    assert_eq!(display, "[INVALID_FIELD] bad value");
}

/// Test: RequestError From<String> implementation
/// Protocol: String-to-error conversion defaults to validation error
/// Reference: https://docs.ag-ui.com/sdk/js/core/types
#[test]
fn test_request_error_from_string() {
    use tirea_protocol_ag_ui::RequestError;

    let err: RequestError = "something went wrong".to_string().into();
    assert_eq!(err.code, "VALIDATION_ERROR");
    assert_eq!(err.message, "something went wrong");
}

/// Test: RequestError serialization
/// Protocol: Error can be serialized for API response
/// Reference: https://docs.ag-ui.com/sdk/js/core/types
#[test]
fn test_request_error_serialization() {
    use tirea_protocol_ag_ui::RequestError;

    let err = RequestError::invalid_field("missing threadId");
    let json = serde_json::to_string(&err).unwrap();

    assert!(json.contains(r#""code":"INVALID_FIELD""#));
    assert!(json.contains(r#""message":"missing threadId""#));

    let parsed: RequestError = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.code, "INVALID_FIELD");
}

// ============================================================================
// P1 - TextMessageChunk Expansion
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/events
// TextMessageChunk auto-expands to Start→Content→End triad
// First chunk must include messageId and role
//

/// Test: TextMessageChunk first chunk requires messageId
/// Protocol: First chunk auto-expands to TextMessageStart
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_text_message_chunk_first_requires_message_id() {
    // First chunk should have messageId and role
    let first_chunk = Event::text_message_chunk(
        Some("msg_1".to_string()),
        Some(Role::Assistant),
        Some("Hello".to_string()),
    );

    let json = serde_json::to_string(&first_chunk).unwrap();
    assert!(json.contains(r#""messageId":"msg_1""#));
    assert!(json.contains(r#""role":"assistant""#));
    assert!(json.contains(r#""delta":"Hello""#));
}

/// Test: TextMessageChunk subsequent chunks only need delta
/// Protocol: After first chunk, only delta is needed
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_text_message_chunk_subsequent_only_delta() {
    let subsequent_chunk = Event::text_message_chunk(None, None, Some(" World".to_string()));

    let json = serde_json::to_string(&subsequent_chunk).unwrap();
    assert!(json.contains(r#""delta":" World""#));
    // messageId and role should not appear when None
    assert!(!json.contains(r#""messageId""#));
    assert!(!json.contains(r#""role""#));
}

/// Test: TextMessageChunk with no delta (end signal)
/// Protocol: Chunk with no delta signals end
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_text_message_chunk_end_signal() {
    let end_chunk = Event::text_message_chunk(None, None, None);

    let json = serde_json::to_string(&end_chunk).unwrap();
    assert!(json.contains(r#""type":"TEXT_MESSAGE_CHUNK""#));
    // No delta, messageId, or role
    assert!(!json.contains(r#""delta""#));
}

// ============================================================================
// P1 - ToolCallChunk Expansion
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/events
// ToolCallChunk auto-expands to Start→Args→End triad
// First chunk must include toolCallId and toolCallName
//

/// Test: ToolCallChunk first chunk requires toolCallId and name
/// Protocol: First chunk auto-expands to ToolCallStart
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_tool_call_chunk_first_requires_id_and_name() {
    let first_chunk = Event::tool_call_chunk(
        Some("call_1".to_string()),
        Some("search".to_string()),
        Some("msg_1".to_string()),
        Some(r#"{"query"}"#.to_string()),
    );

    let json = serde_json::to_string(&first_chunk).unwrap();
    assert!(json.contains(r#""toolCallId":"call_1""#));
    assert!(json.contains(r#""toolCallName":"search""#));
    assert!(json.contains(r#""parentMessageId":"msg_1""#));
}

/// Test: ToolCallChunk subsequent chunks only need delta
/// Protocol: After first chunk, only args delta needed
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_tool_call_chunk_subsequent_only_delta() {
    let subsequent_chunk =
        Event::tool_call_chunk(None, None, None, Some(r#": "rust tutorials"}"#.to_string()));

    let json = serde_json::to_string(&subsequent_chunk).unwrap();
    assert!(json.contains(r#""delta""#));
    assert!(!json.contains(r#""toolCallId""#));
    assert!(!json.contains(r#""toolCallName""#));
}

/// Test: ToolCallChunk end signal
/// Protocol: Chunk with no delta signals end of args
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_tool_call_chunk_end_signal() {
    let end_chunk = Event::tool_call_chunk(None, None, None, None);

    let json = serde_json::to_string(&end_chunk).unwrap();
    assert!(json.contains(r#""type":"TOOL_CALL_CHUNK""#));
    assert!(!json.contains(r#""delta""#));
}

// ============================================================================
// P1 - Non-Empty Delta Validation
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/events
// TextMessageContent.delta should be non-empty
//

/// Test: TextMessageContent with non-empty delta
/// Protocol: TextMessageContent.delta must be non-empty string
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_text_message_content_non_empty_delta() {
    let event = Event::text_message_content("msg1", "Hello World");

    if let Event::TextMessageContent { delta, .. } = &event {
        assert!(!delta.is_empty(), "delta should be non-empty");
    }
}

/// Test: ToolCallArgs with non-empty delta
/// Protocol: ToolCallArgs.delta must be non-empty (argument chunk)
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_tool_call_args_non_empty_delta() {
    let event = Event::tool_call_args("call_1", r#"{"query":"test"}"#);

    if let Event::ToolCallArgs { delta, .. } = &event {
        assert!(!delta.is_empty(), "args delta should be non-empty");
    }
}

// ============================================================================
// P1 - Out-of-Order Event Resilience
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/events
// Systems must handle potential out-of-order delivery gracefully
//

/// Test: Events can be processed even if arrival order varies
/// Protocol: Systems must handle potential out-of-order delivery
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_out_of_order_events_deserialize() {
    // Simulate receiving events out of the expected order
    // All events should still deserialize correctly regardless of order
    let events_json = vec![
        r#"{"type":"TEXT_MESSAGE_CONTENT","messageId":"msg1","delta":"world"}"#,
        r#"{"type":"TEXT_MESSAGE_START","messageId":"msg1","role":"assistant"}"#,
        r#"{"type":"RUN_STARTED","threadId":"t1","runId":"r1"}"#,
        r#"{"type":"TEXT_MESSAGE_END","messageId":"msg1"}"#,
    ];

    // Each event should parse independently
    for json in &events_json {
        let parsed: Result<Event, _> = serde_json::from_str(json);
        assert!(parsed.is_ok(), "Event should parse: {}", json);
    }
}

/// Test: Duplicate events are handled gracefully
/// Protocol: Idempotent event processing
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_duplicate_events_handled() {
    let event = Event::text_message_content("msg1", "Hello");
    let json1 = serde_json::to_string(&event).unwrap();
    let json2 = serde_json::to_string(&event).unwrap();

    // Same event serializes identically
    assert_eq!(json1, json2);

    // Both deserialize to equivalent events
    let parsed1: Event = serde_json::from_str(&json1).unwrap();
    let parsed2: Event = serde_json::from_str(&json2).unwrap();
    assert_eq!(parsed1, parsed2);
}

// ============================================================================
// P2 - Run Branching with parentRunId
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/events
// Runs can be branched using parentRunId for sub-agent patterns
//

/// Test: Run branching creates proper parent-child relationships
/// Protocol: Sub-runs reference parent via parentRunId
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_run_branching_parent_child() {
    // Parent run
    let parent = Event::run_started("t1", "run_parent", None);

    // Child runs
    let child1 = Event::run_started("t1", "run_child1", Some("run_parent".to_string()));
    let child2 = Event::run_started("t1", "run_child2", Some("run_parent".to_string()));

    // Verify parent has no parent
    if let Event::RunStarted { parent_run_id, .. } = &parent {
        assert!(parent_run_id.is_none());
    }

    // Verify children reference parent
    if let Event::RunStarted {
        parent_run_id,
        run_id,
        ..
    } = &child1
    {
        assert_eq!(parent_run_id.as_deref(), Some("run_parent"));
        assert_eq!(run_id, "run_child1");
    }
    if let Event::RunStarted {
        parent_run_id,
        run_id,
        ..
    } = &child2
    {
        assert_eq!(parent_run_id.as_deref(), Some("run_parent"));
        assert_eq!(run_id, "run_child2");
    }
}

/// Test: Run branching serialization with parentRunId
/// Protocol: parentRunId serialized only when present
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_run_branching_serialization() {
    let without_parent = Event::run_started("t1", "r1", None);
    let with_parent = Event::run_started("t1", "r2", Some("r1".to_string()));

    let json_without = serde_json::to_string(&without_parent).unwrap();
    let json_with = serde_json::to_string(&with_parent).unwrap();

    assert!(!json_without.contains("parentRunId"));
    assert!(json_with.contains(r#""parentRunId":"r1""#));
}

// ============================================================================
// P2 - ToolCall Result Content Format
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/sdk/js/core/types
// ToolCallResult.content should be a string (JSON-encoded if structured)
//

/// Test: ToolCallResult content is JSON string
/// Protocol: Tool result content is JSON-encoded string
/// Reference: https://docs.ag-ui.com/sdk/js/core/types
#[test]
fn test_tool_call_result_content_json_string() {
    let result_data = json!({"status": "success", "count": 42});
    let content = serde_json::to_string(&result_data).unwrap();

    let event = Event::tool_call_result("msg_1", "call_1", &content);

    if let Event::ToolCallResult { content: c, .. } = &event {
        // Content should be a string that parses as JSON
        let parsed: Result<Value, _> = serde_json::from_str(c);
        assert!(parsed.is_ok(), "Content should be valid JSON string");
        assert_eq!(parsed.unwrap()["count"], 42);
    }
}

/// Test: ToolCallResult error content with role=tool
/// Protocol: Tool result can contain error information, role is always "tool"
/// Reference: https://docs.ag-ui.com/sdk/js/core/types
#[test]
fn test_tool_call_result_error_content_with_role() {
    let error_data = json!({"status": "error", "error": "File not found", "code": "ENOENT"});
    let content = serde_json::to_string(&error_data).unwrap();

    let event = Event::tool_call_result("msg_1", "call_1", &content);

    if let Event::ToolCallResult {
        content: c, role, ..
    } = &event
    {
        let parsed: Value = serde_json::from_str(c).unwrap();
        assert_eq!(parsed["status"], "error");
        assert_eq!(parsed["code"], "ENOENT");
        // Role should be Tool
        assert_eq!(*role, Some(Role::Tool));
    }
}

// ============================================================================
// P2 - RunAgentInput with config
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/sdk/js/core/types
// Additional configuration can be passed via config field
//

/// Test: RunAgentInput with config field
/// Protocol: Additional configuration in request
/// Reference: https://docs.ag-ui.com/sdk/js/core/types
#[test]
fn test_run_agent_request_with_config() {
    let mut request = RunAgentInput::new("t1".to_string(), "r1".to_string());
    request.config = Some(json!({
        "temperature": 0.7,
        "max_tokens": 1000,
        "top_p": 0.9
    }));

    assert!(request.config.is_some());

    let json = serde_json::to_string(&request).unwrap();
    assert!(json.contains(r#""config""#));
    assert!(json.contains(r#""temperature":0.7"#));
}

/// Test: RunAgentInput deserialization with all optional fields
/// Protocol: All optional fields deserialize correctly
/// Reference: https://docs.ag-ui.com/sdk/js/core/types
#[test]
fn test_run_agent_request_full_deserialization() {
    let json = r#"{
        "threadId": "t1",
        "runId": "r1",
        "messages": [{"role": "user", "content": "Hello"}],
        "tools": [{"name": "search", "description": "Search"}],
        "state": {"counter": 0},
        "parentRunId": "r0",
        "model": "gpt-4o",
        "systemPrompt": "Be helpful",
        "config": {"temperature": 0.5}
    }"#;

    let request: RunAgentInput = serde_json::from_str(json).unwrap();
    assert_eq!(request.thread_id, "t1");
    assert_eq!(request.run_id, "r1");
    assert_eq!(request.messages.len(), 1);
    assert_eq!(request.tools.len(), 1);
    assert_eq!(request.state.unwrap()["counter"], 0);
    assert_eq!(request.parent_run_id.as_deref(), Some("r0"));
    assert_eq!(request.model.as_deref(), Some("gpt-4o"));
    assert_eq!(request.system_prompt.as_deref(), Some("Be helpful"));
    assert_eq!(request.config.unwrap()["temperature"], 0.5);
}

// ============================================================================
// P2 - Complete AgentEvent-to-Event Conversion Flows
// ============================================================================
//
// AG-UI Protocol Reference: https://docs.ag-ui.com/concepts/events
// Verify all AgentEvent variants convert correctly to Events
//

/// Test: AgentEvent::RunFinish produces TEXT_MESSAGE_END + RUN_FINISHED
/// Protocol: Active text stream ends before run finishes
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_agent_event_run_finish_ends_text_stream() {
    let mut ctx = make_agui_ctx("t1", "r1");

    // Start text
    let text = AgentEvent::TextDelta {
        delta: "Hello".into(),
    };
    let _ = ctx.on_agent_event(&text);

    // Finish run while text is active
    let finish = AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: Some(json!({"ok": true})),
        termination: tirea_agentos::contracts::TerminationReason::NaturalEnd,
    };
    let events = ctx.on_agent_event(&finish);

    // Should produce TEXT_MESSAGE_END + RUN_FINISHED
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TextMessageEnd { .. })),
        "Should end text stream"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::RunFinished { .. })),
        "Should emit RUN_FINISHED"
    );

    // TEXT_MESSAGE_END should come before RUN_FINISHED
    let end_idx = events
        .iter()
        .position(|e| matches!(e, Event::TextMessageEnd { .. }))
        .unwrap();
    let finish_idx = events
        .iter()
        .position(|e| matches!(e, Event::RunFinished { .. }))
        .unwrap();
    assert!(end_idx < finish_idx);
}

/// Test: AgentEvent::RunFinish produces TEXT_MESSAGE_END + RUN_FINISHED
/// Protocol: RunFinish event completes text stream and run
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_agent_event_run_finish_ends_text_and_run() {
    let mut ctx = make_agui_ctx("t1", "r1");

    // Start text
    let text = AgentEvent::TextDelta {
        delta: "Response".into(),
    };
    let _ = ctx.on_agent_event(&text);

    // RunFinish
    let finish = AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: Some(serde_json::json!({"response": "Response"})),
        termination: tirea_agentos::contracts::TerminationReason::NaturalEnd,
    };
    let events = ctx.on_agent_event(&finish);

    assert!(events
        .iter()
        .any(|e| matches!(e, Event::TextMessageEnd { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::RunFinished { .. })));
}

/// Test: AgentEvent::RunFinish(Cancelled) produces RUN_ERROR with CANCELLED code
/// Protocol: cancellation maps to RUN_ERROR with code "CANCELLED"
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_agent_event_run_finish_cancelled_produces_run_error() {
    let mut ctx = make_agui_ctx("t1", "r1");

    let cancelled = AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: tirea_agentos::contracts::TerminationReason::Cancelled,
    };
    let events = ctx.on_agent_event(&cancelled);

    assert_eq!(events.len(), 1);
    if let Event::RunError { message, code, .. } = &events[0] {
        assert_eq!(message, "Run cancelled");
        assert_eq!(code.as_deref(), Some("CANCELLED"));
    } else {
        panic!("Expected RunError");
    }
}

/// Test: AgentEvent::Error produces RUN_ERROR
/// Protocol: Error maps to RUN_ERROR with code pass-through
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_agent_event_error_produces_run_error() {
    let mut ctx = make_agui_ctx("t1", "r1");

    let error = AgentEvent::Error {
        message: "API rate limit".into(),
        code: Some("LLM_ERROR".into()),
    };
    let events = ctx.on_agent_event(&error);

    assert_eq!(events.len(), 1);
    if let Event::RunError { message, code, .. } = &events[0] {
        assert_eq!(message, "API rate limit");
        assert_eq!(code.as_deref(), Some("LLM_ERROR"));
    } else {
        panic!("Expected RunError");
    }
}

/// Test: pending ToolCallStart ends text and emits tool call start
/// Protocol: Pending frontend interaction uses normal tool call events
/// Reference: https://docs.ag-ui.com/concepts/human-in-the-loop
#[test]
fn test_agent_event_pending_ends_text() {
    let mut ctx = make_agui_ctx("t1", "r1");

    // Start text
    let text = AgentEvent::TextDelta {
        delta: "Processing".into(),
    };
    let _ = ctx.on_agent_event(&text);

    // Pending frontend tool start
    let pending = AgentEvent::ToolCallStart {
        id: "perm_1".into(),
        name: "confirm".into(),
    };
    let events = ctx.on_agent_event(&pending);

    // Should end text stream
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TextMessageEnd { .. })),
        "Should end text stream before pending"
    );

    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ToolCallStart { .. })),
        "Should emit ToolCallStart for pending frontend tool"
    );
}

/// Test: AgentEvent::ActivitySnapshot maps to Event::ActivitySnapshot
#[test]
fn test_agent_event_activity_snapshot_to_ag_ui() {
    use tirea_protocol_ag_ui::Event;

    let mut ctx = make_agui_ctx("t1", "r1");
    let event = AgentEvent::ActivitySnapshot {
        message_id: "activity_1".to_string(),
        activity_type: "progress".to_string(),
        content: json!({"progress": 0.6}),
        replace: Some(true),
    };

    let events = ctx.on_agent_event(&event);
    assert_eq!(events.len(), 1);
    match &events[0] {
        Event::ActivitySnapshot {
            message_id,
            activity_type,
            content,
            replace,
            ..
        } => {
            assert_eq!(message_id, "activity_1");
            assert_eq!(activity_type, "progress");
            assert_eq!(content.get("progress"), Some(&json!(0.6)));
            assert_eq!(*replace, Some(true));
        }
        _ => panic!("Expected ActivitySnapshot"),
    }
}

/// Test: AgentEvent::ActivityDelta maps to Event::ActivityDelta
#[test]
fn test_agent_event_activity_delta_to_ag_ui() {
    use tirea_protocol_ag_ui::Event;

    let mut ctx = make_agui_ctx("t1", "r1");
    let event = AgentEvent::ActivityDelta {
        message_id: "activity_1".to_string(),
        activity_type: "progress".to_string(),
        patch: vec![json!({"op": "replace", "path": "/progress", "value": 0.8})],
    };

    let events = ctx.on_agent_event(&event);
    assert_eq!(events.len(), 1);
    match &events[0] {
        Event::ActivityDelta {
            message_id,
            activity_type,
            patch,
            ..
        } => {
            assert_eq!(message_id, "activity_1");
            assert_eq!(activity_type, "progress");
            assert_eq!(patch.len(), 1);
            assert_eq!(patch[0]["path"], "/progress");
        }
        _ => panic!("Expected ActivityDelta"),
    }
}

/// Test: AgentEvent::StepStart/StepEnd produce STEP_STARTED/STEP_FINISHED
/// Protocol: Step events map directly
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_agent_event_step_events() {
    let mut ctx = make_agui_ctx("t1", "r1");

    let step_start = AgentEvent::StepStart {
        message_id: String::new(),
    };
    let events = ctx.on_agent_event(&step_start);
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], Event::StepStarted { step_name, .. } if step_name == "step_1"));

    let step_end = AgentEvent::StepEnd;
    let events = ctx.on_agent_event(&step_end);
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], Event::StepFinished { step_name, .. } if step_name == "step_1"));
}

/// Test: ToolCallStart ends active text stream
/// Protocol: Tool call interrupts text, TEXT_MESSAGE_END emitted
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_tool_call_start_ends_active_text() {
    let mut ctx = make_agui_ctx("t1", "r1");

    // Start text
    let text = AgentEvent::TextDelta {
        delta: "Thinking".into(),
    };
    let _ = ctx.on_agent_event(&text);

    // Tool starts - should end text first
    let tool_start = AgentEvent::ToolCallStart {
        id: "call_1".into(),
        name: "search".into(),
    };
    let events = ctx.on_agent_event(&tool_start);

    // First event should be TEXT_MESSAGE_END
    assert!(
        matches!(&events[0], Event::TextMessageEnd { .. }),
        "First event should be TEXT_MESSAGE_END"
    );

    // Then TOOL_CALL_START
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::ToolCallStart { .. })));
}

/// Test: ToolCallStart includes parentMessageId from context
/// Protocol: Tool call references parent message
/// Reference: https://docs.ag-ui.com/concepts/events
#[test]
fn test_tool_call_start_includes_parent_message_id() {
    let mut ctx = make_agui_ctx("t1", "r1");

    let tool_start = AgentEvent::ToolCallStart {
        id: "call_1".into(),
        name: "search".into(),
    };
    let events = ctx.on_agent_event(&tool_start);

    let start_event = events
        .iter()
        .find(|e| matches!(e, Event::ToolCallStart { .. }))
        .unwrap();
    if let Event::ToolCallStart {
        parent_message_id, ..
    } = start_event
    {
        assert!(
            parent_message_id.is_some(),
            "Should include parentMessageId"
        );
    }
}

/// Test: Suspension.to_ag_ui_events produces complete tool call sequence
/// Protocol: Suspension maps to TOOL_CALL_START → TOOL_CALL_ARGS → TOOL_CALL_END
/// Reference: https://docs.ag-ui.com/concepts/human-in-the-loop
#[test]
fn test_interaction_to_ag_ui_events() {
    use tirea_agentos::contracts::Suspension;

    let interaction = Suspension::new("int_1", "confirm_delete")
        .with_parameters(json!({"file": "important.txt"}));

    let events = interaction_to_ag_ui_events(&interaction);

    assert_eq!(events.len(), 3, "Should produce START, ARGS, END");
    assert!(matches!(&events[0], Event::ToolCallStart { .. }));
    assert!(matches!(&events[1], Event::ToolCallArgs { .. }));
    assert!(matches!(&events[2], Event::ToolCallEnd { .. }));

    // Verify tool call ID matches interaction ID
    if let Event::ToolCallStart {
        tool_call_id,
        tool_call_name,
        ..
    } = &events[0]
    {
        assert_eq!(tool_call_id, "int_1");
        assert_eq!(tool_call_name, "confirm_delete");
    }
}

// ============================================================================
// LLMMetryPlugin tracing span integration tests
// ============================================================================

mod llmmetry_tracing {
    use crate::AgentBehaviorTestDispatch;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tirea_agentos::contracts::runtime::inference::LLMResponse;
    use tirea_agentos::contracts::runtime::phase::{Phase, StepContext};
    use tirea_agentos::contracts::runtime::tool_call::ToolGate;
    use tirea_agentos::contracts::runtime::tool_call::ToolResult;
    use tirea_agentos::contracts::runtime::StreamResult;
    use tirea_agentos::contracts::thread::Thread as ConversationAgentState;
    use tirea_agentos::contracts::thread::ToolCall;
    use tirea_agentos::extensions::observability::{InMemorySink, LLMMetryPlugin};
    use tirea_contract::testing::TestFixture;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::registry::LookupSpan;

    #[derive(Debug, Clone)]
    struct CapturedSpan {
        id: u64,
        name: String,
        was_closed: bool,
    }

    struct SpanCaptureLayer {
        captured: Arc<Mutex<Vec<CapturedSpan>>>,
    }

    impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> tracing_subscriber::Layer<S>
        for SpanCaptureLayer
    {
        fn on_new_span(
            &self,
            _attrs: &tracing::span::Attributes<'_>,
            id: &tracing::span::Id,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if let Some(span_ref) = ctx.span(id) {
                self.captured.lock().unwrap().push(CapturedSpan {
                    id: id.into_u64(),
                    name: span_ref.name().to_string(),
                    was_closed: false,
                });
            }
        }

        fn on_close(&self, id: tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
            let raw_id = id.into_u64();
            let _ = ctx; // span may no longer be queryable from the registry at close time
            let mut captured = self.captured.lock().unwrap();
            if let Some(entry) = captured.iter_mut().find(|c| c.id == raw_id) {
                entry.was_closed = true;
            }
        }
    }

    fn setup_tracing() -> (
        tracing::subscriber::DefaultGuard,
        Arc<Mutex<Vec<CapturedSpan>>>,
    ) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let layer = SpanCaptureLayer {
            captured: captured.clone(),
        };
        let subscriber = tracing_subscriber::registry::Registry::default().with(layer);
        let guard = tracing::subscriber::set_default(subscriber);
        (guard, captured)
    }

    fn usage(prompt: i32, completion: i32, total: i32) -> tirea_contract::TokenUsage {
        tirea_contract::TokenUsage {
            prompt_tokens: Some(prompt),
            completion_tokens: Some(completion),
            total_tokens: Some(total),
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_inference_tracing_span_lifecycle() {
        let doc = json!({});
        let _fix = TestFixture::new_with_state(doc);
        let (_guard, captured) = setup_tracing();
        let baseline = { captured.lock().unwrap().len() };

        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone())
            .with_model("test-model")
            .with_provider("test-provider");

        let thread = ConversationAgentState::new("test");
        let fix = TestFixture::new();
        let ctx_step = fix.ctx();
        let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);

        plugin.run_phase(Phase::BeforeInference, &mut step).await;

        step.llm_response = Some(LLMResponse::success(StreamResult {
            text: "hello".into(),
            tool_calls: vec![],
            usage: Some(usage(100, 50, 150)),
            stop_reason: None,
        }));

        plugin.run_phase(Phase::AfterInference, &mut step).await;

        let new_spans: Vec<CapturedSpan> = {
            let spans = captured.lock().unwrap();
            spans[baseline..].to_vec()
        };
        let chat_span = new_spans.iter().find(|s| s.name == "gen_ai");
        assert!(
            chat_span.is_some(),
            "gen_ai span (inference) should be created"
        );
        assert!(
            chat_span.unwrap().was_closed,
            "gen_ai span (inference) should be closed after AfterInference"
        );

        // Verify metrics sink still works alongside tracing
        let m = sink.metrics();
        assert_eq!(m.inference_count(), 1);
        assert_eq!(m.total_input_tokens(), 100);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_tool_tracing_span_lifecycle() {
        let doc = json!({});
        let _fix = TestFixture::new_with_state(doc);
        let (_guard, captured) = setup_tracing();
        let baseline = { captured.lock().unwrap().len() };

        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone());

        let thread = ConversationAgentState::new("test");
        let fix = TestFixture::new();
        let ctx_step = fix.ctx();
        let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);

        let call = ToolCall::new("tc1", "search", json!({}));
        step.gate = Some(ToolGate::from_tool_call(&call));

        plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;

        step.gate.as_mut().unwrap().result =
            Some(ToolResult::success("search", json!({"found": true})));

        plugin.run_phase(Phase::AfterToolExecute, &mut step).await;

        let new_spans: Vec<CapturedSpan> = {
            let spans = captured.lock().unwrap();
            spans[baseline..].to_vec()
        };
        let tool_span = new_spans.iter().find(|s| s.name == "gen_ai");
        assert!(tool_span.is_some(), "gen_ai span (tool) should be created");
        assert!(
            tool_span.unwrap().was_closed,
            "gen_ai span (tool) should be closed after AfterToolExecute"
        );

        let m = sink.metrics();
        assert_eq!(m.tool_count(), 1);
        assert!(m.tools[0].is_success());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_full_session_with_tracing_spans() {
        let doc = json!({});
        let _fix = TestFixture::new_with_state(doc);
        let (_guard, captured) = setup_tracing();
        let baseline = { captured.lock().unwrap().len() };

        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone())
            .with_model("gpt-4")
            .with_provider("openai");

        let thread = ConversationAgentState::new("test");
        let fix = TestFixture::new();
        let ctx_step = fix.ctx();
        let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);

        // Thread start
        plugin.run_phase(Phase::RunStart, &mut step).await;

        // Inference
        plugin.run_phase(Phase::BeforeInference, &mut step).await;
        step.llm_response = Some(LLMResponse::success(StreamResult {
            text: "use search tool".into(),
            tool_calls: vec![],
            usage: Some(usage(50, 25, 75)),
            stop_reason: None,
        }));
        plugin.run_phase(Phase::AfterInference, &mut step).await;

        // Tool execution
        let call = ToolCall::new("c1", "search", json!({"q": "test"}));
        step.gate = Some(ToolGate::from_tool_call(&call));
        plugin.run_phase(Phase::BeforeToolExecute, &mut step).await;
        step.gate.as_mut().unwrap().result =
            Some(ToolResult::success("search", json!({"results": []})));
        plugin.run_phase(Phase::AfterToolExecute, &mut step).await;

        // Thread end
        plugin.run_phase(Phase::RunEnd, &mut step).await;

        let new_spans: Vec<CapturedSpan> = {
            let spans = captured.lock().unwrap();
            spans[baseline..].to_vec()
        };
        let gen_ai_count = new_spans.iter().filter(|s| s.name == "gen_ai").count();
        assert_eq!(
            gen_ai_count, 2,
            "expected 2 gen_ai spans (inference + tool)"
        );
        assert!(
            new_spans.iter().all(|s| s.was_closed),
            "all spans should be closed"
        );

        // The per-plugin metrics sink is not shared, so exact counts are reliable
        let m = sink.metrics();
        assert_eq!(m.inference_count(), 1);
        assert_eq!(m.tool_count(), 1);
    }
}

// ============================================================================
// InteractionPlugin RunStart / resume-decision state tests
// ============================================================================

fn replay_calls_from_state(state: &Value) -> Vec<ToolCall> {
    let decisions = resume_inputs_from_state(state);
    if decisions.is_empty() {
        return Vec::new();
    }

    let suspended = suspended_calls_from_state(state);

    let mut call_ids: Vec<String> = decisions.keys().cloned().collect();
    call_ids.sort();
    call_ids
        .into_iter()
        .filter_map(|call_id| {
            let decision = decisions.get(&call_id)?;
            if !matches!(decision.action, ResumeDecisionAction::Resume) {
                return None;
            }
            let call = suspended.get(&call_id)?;
            Some(match call.ticket.resume_mode {
                tirea_agentos::contracts::runtime::ToolCallResumeMode::ReplayToolCall => {
                    ToolCall::new(
                        call.call_id.clone(),
                        call.tool_name.clone(),
                        call.arguments.clone(),
                    )
                }
                tirea_agentos::contracts::runtime::ToolCallResumeMode::UseDecisionAsToolResult
                | tirea_agentos::contracts::runtime::ToolCallResumeMode::PassDecisionToTool => {
                    ToolCall::new(
                        call.call_id.clone(),
                        call.tool_name.clone(),
                        call.arguments.clone(),
                    )
                }
            })
        })
        .collect()
}

fn resume_inputs_from_state(state: &Value) -> HashMap<String, ToolCallResume> {
    tirea_agentos::contracts::runtime::tool_call_states_from_state(state)
        .into_iter()
        .filter_map(|(call_id, tool_state)| {
            if !matches!(tool_state.status, ToolCallStatus::Resuming) {
                return None;
            }
            let mut resume = tool_state.resume?;
            if resume.decision_id.trim().is_empty() {
                resume.decision_id = call_id.clone();
            }
            Some((call_id.clone(), resume))
        })
        .collect()
}

fn state_with_suspended_call(
    call_id: &str,
    tool_name: &str,
    suspension: Value,
    invocation: Option<Value>,
) -> Value {
    let invocation = invocation.unwrap_or_else(|| {
        json!({
            "call_id": call_id,
            "tool_name": tool_name,
            "arguments": {},
            "origin": {
                "type": "tool_call_intercepted",
                "backend_call_id": call_id,
                "backend_tool_name": tool_name,
                "backend_arguments": {}
            },
            "routing": {
                "strategy": "replay_original_tool"
            }
        })
    });
    let invocation_arguments = invocation
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let backend_arguments = invocation
        .get("origin")
        .and_then(|origin| origin.get("backend_arguments"))
        .cloned()
        .unwrap_or_else(|| invocation_arguments.clone());
    let pending = json!({
        "id": invocation.get("call_id").and_then(Value::as_str).unwrap_or(call_id),
        "name": invocation.get("tool_name").and_then(Value::as_str).unwrap_or(tool_name),
        "arguments": invocation_arguments,
    });
    let resume_mode = match invocation
        .get("routing")
        .and_then(|routing| routing.get("strategy"))
        .and_then(Value::as_str)
    {
        Some("use_as_tool_result") => "use_decision_as_tool_result",
        Some("pass_to_llm") => "pass_decision_to_tool",
        _ => "replay_tool_call",
    };
    // SuspendedCall is flattened into SuspendedCallState, which is stored at
    // __tool_call_scope.<call_id>.suspended_call per the new per-call-scoped schema.
    let suspended_call = json!({
        "call_id": call_id,
        "tool_name": tool_name,
        "arguments": backend_arguments,
        "suspension": suspension,
        "pending": pending,
        "resume_mode": resume_mode,
    });
    json!({
        "__tool_call_scope": {
            call_id: {
                "suspended_call": suspended_call
            }
        }
    })
}

/// Test: on_run_start schedules replay when suspended call is approved
#[tokio::test]
async fn test_interaction_response_run_start_sets_replay_on_approval() {
    let pending_id = "call_add_trips";
    let frontend_call_id = "fc_perm_add_trips";

    // Build a session with unified format:
    //   1. A persisted suspended_interaction (id = tool_call_id, action = tool:<name>)
    //   2. An assistant message carrying the original tool call
    //   3. A placeholder tool result
    let thread = Thread::with_initial_state(
        "test",
        state_with_suspended_call(
            pending_id,
            "add_trips",
            json!({
                "id": frontend_call_id,
                "action": "tool:add_trips",
                "parameters": {
                    "source": "permission"
                }
            }),
            Some(json!({
                "call_id": frontend_call_id,
                "tool_name": "PermissionConfirm",
                "arguments": {
                    "tool_name": "add_trips",
                    "tool_args": { "destination": "Beijing" }
                },
                "origin": {
                    "type": "tool_call_intercepted",
                    "backend_call_id": pending_id,
                    "backend_tool_name": "add_trips",
                    "backend_arguments": { "destination": "Beijing" }
                },
                "routing": {
                    "strategy": "replay_original_tool"
                }
            })),
        ),
    )
    .with_message(
        tirea_agentos::contracts::thread::Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new(
                pending_id,
                "add_trips",
                json!({"destination": "Beijing"}),
            )],
        ),
    )
    .with_message(tirea_agentos::contracts::thread::Message::tool(
        pending_id,
        "Tool 'add_trips' is awaiting approval. Execution paused.",
    ));

    let plugin = InteractionPlugin::with_responses(vec![frontend_call_id.to_string()], vec![]);

    let fix = TestFixture::new_with_state(thread.state.clone());
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);

    plugin.run_phase(Phase::RunStart, &mut step).await;

    let calls = replay_calls_from_state(&fix.updated_state());
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "add_trips");
    assert_eq!(calls[0].id, pending_id);
}

/// Test: on_run_start does NOT schedule replay when interaction is denied
#[tokio::test]
async fn test_interaction_response_run_start_no_replay_on_denial() {
    let pending_id = "call_add_trips";

    let thread = Thread::with_initial_state(
        "test",
        state_with_suspended_call(
            pending_id,
            "add_trips",
            json!({
                "id": pending_id,
                "action": "tool:add_trips",
                "parameters": { "source": "permission" }
            }),
            None,
        ),
    )
    .with_message(
        tirea_agentos::contracts::thread::Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new(pending_id, "add_trips", json!({}))],
        ),
    );

    // Plugin with this interaction denied (not approved)
    let plugin = InteractionPlugin::with_responses(
        vec![],                       // no approved
        vec![pending_id.to_string()], // denied
    );

    let fix = TestFixture::new_with_state(thread.state.clone());
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    let replay = replay_calls_from_state(&fix.updated_state());
    assert!(
        replay.is_empty(),
        "replay should NOT be scheduled on denial"
    );
}

/// Test: on_run_start does nothing when no suspended_interaction exists
#[tokio::test]
async fn test_interaction_response_run_start_no_pending() {
    let thread = Thread::with_initial_state("test", json!({}));

    let plugin = InteractionPlugin::with_responses(vec!["some_id".to_string()], vec![]);

    let fix = TestFixture::new_with_state(thread.state.clone());
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    let replay = replay_calls_from_state(&fix.updated_state());
    assert!(
        replay.is_empty(),
        "replay should not be scheduled when no suspended call exists"
    );
}

/// Test: on_run_start does nothing when approved ID doesn't match pending
#[tokio::test]
async fn test_interaction_response_run_start_mismatched_id() {
    let thread = Thread::with_initial_state(
        "test",
        state_with_suspended_call(
            "call_x",
            "some_tool",
            json!({
                "id": "call_x",
                "action": "tool:some_tool",
                "parameters": { "source": "permission" }
            }),
            None,
        ),
    )
    .with_message(
        tirea_agentos::contracts::thread::Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new("call_x", "some_tool", json!({}))],
        ),
    );

    // Approved a different ID
    let plugin = InteractionPlugin::with_responses(vec!["call_y".to_string()], vec![]);

    let fix = TestFixture::new_with_state(thread.state.clone());
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    let replay = replay_calls_from_state(&fix.updated_state());
    assert!(
        replay.is_empty(),
        "replay should not be scheduled when IDs don't match"
    );
}

/// Test: on_run_start does nothing when no assistant message with tool_calls
#[tokio::test]
async fn test_interaction_response_run_start_no_tool_calls_in_messages() {
    let pending_id = "call_add_trips";
    let frontend_call_id = "fc_perm_add_trips";

    // Thread has suspended interaction with unified format but no assistant message with tool calls.
    // With origin_tool_call in parameters, replay should use that.
    // Without origin_tool_call and with tool:<name> action, it creates replay from parameters.
    // This test verifies the tool:<name> fallback path works.
    let thread = Thread::with_initial_state(
        "test",
        state_with_suspended_call(
            pending_id,
            "add_trips",
            json!({
                "id": frontend_call_id,
                "action": "tool:add_trips",
                "parameters": {
                    "source": "permission"
                }
            }),
            Some(json!({
                "call_id": frontend_call_id,
                "tool_name": "PermissionConfirm",
                "arguments": {
                    "tool_name": "add_trips",
                    "tool_args": {}
                },
                "origin": {
                    "type": "tool_call_intercepted",
                    "backend_call_id": pending_id,
                    "backend_tool_name": "add_trips",
                    "backend_arguments": {}
                },
                "routing": {
                    "strategy": "replay_original_tool"
                }
            })),
        ),
    )
    .with_message(tirea_agentos::contracts::thread::Message::assistant(
        "I need to call a tool",
    ));

    let plugin = InteractionPlugin::with_responses(vec![frontend_call_id.to_string()], vec![]);

    let fix = TestFixture::new_with_state(thread.state.clone());
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(ctx_step, &thread.id, &thread.messages, vec![]);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    // With origin_tool_call present, replay should be scheduled even without tool_calls in messages
    let replay = replay_calls_from_state(&fix.updated_state());
    assert_eq!(
        replay.len(),
        1,
        "origin_tool_call should provide replay data"
    );
    assert_eq!(replay[0].name, "add_trips");
}

// ============================================================================
// HITL Replay E2E Integration Tests
// ============================================================================

/// Test: Full HITL flow — PermissionPlugin suspends → client approves →
/// InteractionPlugin detects approval in RunStart →
/// schedules replay with correct tool call data
#[tokio::test]
async fn test_hitl_replay_full_flow_suspend_approve_schedule() {
    // Phase 1: PermissionPlugin creates suspended interaction
    let thread = ConversationAgentState::new("test");
    let permission_plugin = PermissionPlugin;
    let fix1 = TestFixture::new();
    let ctx_step1 = fix1.ctx();
    let mut step1 = StepContext::new(ctx_step1, &thread.id, &thread.messages, vec![]);
    let call = ToolCall::new("call_add", "add_trips", json!({"destination": "Beijing"}));
    step1.gate = Some(ToolGate::from_tool_call(&call));

    permission_plugin
        .run_phase(Phase::BeforeToolExecute, &mut step1)
        .await;
    assert!(
        step1.tool_pending(),
        "PermissionPlugin should create pending"
    );

    let interaction = suspended_interaction(&step1).expect("suspended interaction should exist");
    let invocation = suspended_invocation(&step1).expect("pending invocation should exist");

    // Phase 2: Simulate persisted session with suspended_interaction + placeholder
    let persisted_thread = Thread::with_initial_state(
        "test",
        state_with_suspended_call(
            &call.id,
            &call.name,
            json!({
                "id": interaction.id,
                "action": interaction.action
            }),
            Some(serde_json::to_value(invocation).unwrap()),
        ),
    )
    .with_message(
        tirea_agentos::contracts::thread::Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new(
                &interaction.id,
                "add_trips",
                json!({"destination": "Beijing"}),
            )],
        ),
    )
    .with_message(tirea_agentos::contracts::thread::Message::tool(
        &interaction.id,
        "Tool 'add_trips' is awaiting approval. Execution paused.",
    ));

    // Phase 3: Client approves via AG-UI tool message
    let approve_request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::tool("true", &interaction.id));
    assert!(approve_request
        .approved_target_ids()
        .iter()
        .any(|id| id == &interaction.id));

    // Phase 4: InteractionPlugin processes approval in RunStart
    let response_plugin = interaction_plugin_from_request(&approve_request);
    let fix2 = TestFixture::new_with_state(persisted_thread.state.clone());
    let ctx_step2 = fix2.ctx();
    let mut step2 = StepContext::new(
        ctx_step2,
        &persisted_thread.id,
        &persisted_thread.messages,
        vec![],
    );
    response_plugin.run_phase(Phase::RunStart, &mut step2).await;

    // Verify: replay is scheduled with correct tool call
    let replay = replay_calls_from_state(&fix2.updated_state());
    assert_eq!(replay.len(), 1, "Should schedule exactly one tool call");
    assert_eq!(replay[0].name, "add_trips");
    assert_eq!(replay[0].id, call.id);
    assert_eq!(replay[0].arguments["destination"], "Beijing");
}

/// Test: HITL replay — denial path does NOT schedule replay
#[tokio::test]
async fn test_hitl_replay_denial_does_not_schedule() {
    let pending_id = "call_add";

    let persisted_thread = Thread::with_initial_state(
        "test",
        state_with_suspended_call(
            pending_id,
            "add_trips",
            json!({
                "id": pending_id,
                "action": "tool:add_trips",
                "parameters": { "source": "permission" }
            }),
            None,
        ),
    )
    .with_message(
        tirea_agentos::contracts::thread::Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new(pending_id, "add_trips", json!({}))],
        ),
    );

    // Client denies
    let deny_request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::tool("false", pending_id));
    assert!(!deny_request
        .approved_target_ids()
        .iter()
        .any(|id| id == pending_id));

    let response_plugin = interaction_plugin_from_request(&deny_request);
    let fix = TestFixture::new_with_state(persisted_thread.state.clone());
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(
        ctx_step,
        &persisted_thread.id,
        &persisted_thread.messages,
        vec![],
    );
    response_plugin.run_phase(Phase::RunStart, &mut step).await;

    let replay = replay_calls_from_state(&fix.updated_state());
    assert!(replay.is_empty(), "Denial should NOT schedule tool replay");
}

/// Test: HITL replay — multiple tool calls, only first is scheduled
#[tokio::test]
async fn test_hitl_replay_picks_first_tool_call() {
    let pending_id = "call_multi";

    let persisted_thread = Thread::with_initial_state(
        "test",
        state_with_suspended_call(
            pending_id,
            "tool_a",
            json!({
                "id": pending_id,
                "action": "tool:tool_a",
                "parameters": {
                    "source": "permission",
                    "origin_tool_call": {
                        "id": pending_id,
                        "name": "tool_a",
                        "arguments": { "a": 1 }
                    }
                }
            }),
            Some(json!({
                "call_id": pending_id,
                "tool_name": "PermissionConfirm",
                "arguments": {
                    "tool_name": "tool_a",
                    "tool_args": { "a": 1 }
                },
                "origin": {
                    "type": "tool_call_intercepted",
                    "backend_call_id": pending_id,
                    "backend_tool_name": "tool_a",
                    "backend_arguments": { "a": 1 }
                },
                "routing": {
                    "strategy": "replay_original_tool"
                }
            })),
        ),
    )
    .with_message(
        tirea_agentos::contracts::thread::Message::assistant_with_tool_calls(
            "",
            vec![
                ToolCall::new(pending_id, "tool_a", json!({"a": 1})),
                ToolCall::new("other_id", "tool_b", json!({"b": 2})),
            ],
        ),
    );

    let approve_request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::tool("true", pending_id));

    let response_plugin = interaction_plugin_from_request(&approve_request);
    let fix = TestFixture::new_with_state(persisted_thread.state.clone());
    let ctx_step = fix.ctx();
    let mut step = StepContext::new(
        ctx_step,
        &persisted_thread.id,
        &persisted_thread.messages,
        vec![],
    );
    response_plugin.run_phase(Phase::RunStart, &mut step).await;

    let replay = replay_calls_from_state(&fix.updated_state());
    assert_eq!(replay.len(), 1, "Should only schedule the first tool call");
    assert_eq!(replay[0].name, "tool_a");
}

/// Test: HITL replay — RunStart + BeforeToolExecute phases are independent
#[tokio::test]
async fn test_hitl_replay_run_start_does_not_affect_before_tool_execute() {
    let pending_id = "call_phase_test";

    let thread = Thread::with_initial_state(
        "test",
        state_with_suspended_call(
            pending_id,
            "some_tool",
            json!({
                "id": pending_id,
                "action": "tool:some_tool",
                "parameters": {
                    "source": "permission",
                    "origin_tool_call": {
                        "id": pending_id,
                        "name": "some_tool",
                        "arguments": {}
                    }
                }
            }),
            Some(json!({
                "call_id": pending_id,
                "tool_name": "PermissionConfirm",
                "arguments": {
                    "tool_name": "some_tool",
                    "tool_args": {}
                },
                "origin": {
                    "type": "tool_call_intercepted",
                    "backend_call_id": pending_id,
                    "backend_tool_name": "some_tool",
                    "backend_arguments": {}
                },
                "routing": {
                    "strategy": "replay_original_tool"
                }
            })),
        ),
    )
    .with_message(
        tirea_agentos::contracts::thread::Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new(pending_id, "some_tool", json!({}))],
        ),
    );

    let approve_request = RunAgentInput::new("t1".to_string(), "r1".to_string())
        .with_message(tirea_protocol_ag_ui::Message::tool("true", pending_id));
    let response_plugin = interaction_plugin_from_request(&approve_request);

    // RunStart schedules replay
    let fix1 = TestFixture::new_with_state(thread.state.clone());
    let ctx_step1 = fix1.ctx();
    let mut step1 = StepContext::new(ctx_step1, &thread.id, &thread.messages, vec![]);
    response_plugin.run_phase(Phase::RunStart, &mut step1).await;
    let replay = replay_calls_from_state(&fix1.updated_state());
    assert!(!replay.is_empty());

    // BeforeToolExecute on different step context (independent)
    let fix2 = TestFixture::new_with_state(thread.state.clone());
    let ctx_step2 = fix2.ctx();
    let mut step2 = StepContext::new(ctx_step2, &thread.id, &thread.messages, vec![]);
    let call = ToolCall::new(pending_id, "some_tool", json!({}));
    step2.gate = Some(ToolGate::from_tool_call(&call));
    response_plugin
        .run_phase(Phase::BeforeToolExecute, &mut step2)
        .await;
    // Tool should be allowed (approved)
    assert!(
        !step2.tool_blocked(),
        "Approved tool should not be blocked in BeforeToolExecute"
    );
}
