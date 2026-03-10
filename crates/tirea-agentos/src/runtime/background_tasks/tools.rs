//! Built-in tools for querying and managing background tasks.
//!
//! These tools provide a unified interface for the LLM to check status,
//! read output, and cancel any background task regardless of type.

use super::manager::BackgroundTaskManager;
use super::{TaskState, TaskStatus, TaskStore, TaskSummary};
use crate::contracts::runtime::tool_call::{ToolCallContext, ToolError, ToolResult};
use crate::contracts::storage::ThreadStore;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

pub const TASK_STATUS_TOOL_ID: &str = "task_status";
pub const TASK_CANCEL_TOOL_ID: &str = "task_cancel";
pub const TASK_OUTPUT_TOOL_ID: &str = "task_output";

fn owner_thread_id(ctx: &ToolCallContext<'_>) -> Option<String> {
    ctx.run_config()
        .value("__agent_tool_caller_thread_id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// task_status
// ---------------------------------------------------------------------------

/// Query background task status and result.
///
/// Supports querying a single task by `task_id` or listing all tasks.
#[derive(Debug, Clone)]
pub struct TaskStatusTool {
    manager: Arc<BackgroundTaskManager>,
    task_store: Option<Arc<TaskStore>>,
}

impl TaskStatusTool {
    pub fn new(manager: Arc<BackgroundTaskManager>) -> Self {
        Self {
            manager,
            task_store: None,
        }
    }

    pub fn with_task_store(mut self, task_store: Option<Arc<TaskStore>>) -> Self {
        self.task_store = task_store;
        self
    }

    async fn query_one(
        &self,
        owner_thread_id: &str,
        task_id: &str,
    ) -> Result<Option<TaskSummary>, String> {
        let persisted = if let Some(store) = &self.task_store {
            store
                .load_task_for_owner(owner_thread_id, task_id)
                .await
                .map_err(|e| e.to_string())?
                .map(|task| task.summary())
        } else {
            None
        };
        let live = self.manager.get(owner_thread_id, task_id).await;

        Ok(match (persisted, live) {
            (_, Some(live)) => Some(live),
            (Some(task), None) => Some(task),
            (None, None) => None,
        })
    }

    async fn list_all(&self, owner_thread_id: &str) -> Result<Vec<TaskSummary>, String> {
        let mut by_id: HashMap<String, TaskSummary> = HashMap::new();

        if let Some(store) = &self.task_store {
            let tasks = store
                .list_tasks_for_owner(owner_thread_id)
                .await
                .map_err(|e| e.to_string())?;
            for task in tasks {
                by_id.insert(task.id.clone(), task.summary());
            }
        }

        for summary in self.manager.list(owner_thread_id, None).await {
            by_id.insert(summary.task_id.clone(), summary);
        }

        let mut out: Vec<TaskSummary> = by_id.into_values().collect();
        out.sort_by(|a, b| {
            a.created_at_ms
                .cmp(&b.created_at_ms)
                .then_with(|| a.task_id.cmp(&b.task_id))
        });
        Ok(out)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskStatusArgs {
    /// Task ID to query. Omit to list all tasks.
    task_id: Option<String>,
}

#[async_trait]
impl crate::contracts::runtime::tool_call::TypedTool for TaskStatusTool {
    type Args = TaskStatusArgs;

    fn tool_id(&self) -> &str {
        TASK_STATUS_TOOL_ID
    }
    fn name(&self) -> &str {
        "Task Status"
    }
    fn description(&self) -> &str {
        "Check the status and result of background tasks. \
         Provide task_id to query a specific task, or omit to list all tasks."
    }

    async fn execute(
        &self,
        args: TaskStatusArgs,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let Some(thread_id) = owner_thread_id(ctx) else {
            return Ok(ToolResult::error(
                TASK_STATUS_TOOL_ID,
                "Missing caller thread context",
            ));
        };

        let task_id = args.task_id.as_deref().filter(|s| !s.trim().is_empty());

        if let Some(task_id) = task_id {
            match self.query_one(&thread_id, task_id).await {
                Ok(Some(summary)) => Ok(ToolResult::success(
                    TASK_STATUS_TOOL_ID,
                    serde_json::to_value(&summary).unwrap_or(Value::Null),
                )),
                Ok(None) => Ok(ToolResult::error(
                    TASK_STATUS_TOOL_ID,
                    format!("Unknown task_id: {task_id}"),
                )),
                Err(err) => Ok(ToolResult::error(TASK_STATUS_TOOL_ID, err)),
            }
        } else {
            match self.list_all(&thread_id).await {
                Ok(tasks) => Ok(ToolResult::success(
                    TASK_STATUS_TOOL_ID,
                    json!({
                        "tasks": serde_json::to_value(&tasks).unwrap_or(Value::Null),
                        "total": tasks.len(),
                    }),
                )),
                Err(err) => Ok(ToolResult::error(TASK_STATUS_TOOL_ID, err)),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// task_cancel
// ---------------------------------------------------------------------------

/// Cancel a running background task and any descendant tasks.
#[derive(Debug, Clone)]
pub struct TaskCancelTool {
    manager: Arc<BackgroundTaskManager>,
    task_store: Option<Arc<TaskStore>>,
}

impl TaskCancelTool {
    pub fn new(manager: Arc<BackgroundTaskManager>) -> Self {
        Self {
            manager,
            task_store: None,
        }
    }

    pub fn with_task_store(mut self, task_store: Option<Arc<TaskStore>>) -> Self {
        self.task_store = task_store;
        self
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskCancelArgs {
    /// The task ID to cancel.
    task_id: String,
}

#[async_trait]
impl crate::contracts::runtime::tool_call::TypedTool for TaskCancelTool {
    type Args = TaskCancelArgs;

    fn tool_id(&self) -> &str {
        TASK_CANCEL_TOOL_ID
    }
    fn name(&self) -> &str {
        "Task Cancel"
    }
    fn description(&self) -> &str {
        "Cancel a running background task by task_id. \
         Descendant tasks are cancelled automatically."
    }

    fn validate(&self, args: &Self::Args) -> Result<(), String> {
        if args.task_id.trim().is_empty() {
            return Err("task_id cannot be empty".to_string());
        }
        Ok(())
    }

    async fn execute(
        &self,
        args: TaskCancelArgs,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let task_id = &args.task_id;

        let Some(thread_id) = owner_thread_id(ctx) else {
            return Ok(ToolResult::error(
                TASK_CANCEL_TOOL_ID,
                "Missing caller thread context",
            ));
        };

        match self.manager.cancel_tree(&thread_id, task_id).await {
            Ok(cancelled) => {
                if let Some(store) = &self.task_store {
                    for summary in &cancelled {
                        let _ = store.mark_cancel_requested(&summary.task_id).await;
                    }
                }
                let ids: Vec<&str> = cancelled.iter().map(|s| s.task_id.as_str()).collect();
                Ok(ToolResult::success(
                    TASK_CANCEL_TOOL_ID,
                    json!({
                        "task_id": task_id,
                        "cancelled": true,
                        "cancelled_ids": ids,
                        "cancelled_count": cancelled.len(),
                    }),
                ))
            }
            Err(e) => Ok(ToolResult::error(TASK_CANCEL_TOOL_ID, e)),
        }
    }
}

// ---------------------------------------------------------------------------
// task_output
// ---------------------------------------------------------------------------

/// Read the output of a background task.
///
/// For `agent_run` tasks, returns the last assistant message from the sub-agent
/// thread. For other task types, returns the task result from the manager.
/// Reads durable task state from task threads and overlays live in-memory
/// results when available.
#[derive(Clone)]
pub struct TaskOutputTool {
    manager: Arc<BackgroundTaskManager>,
    task_store: Option<Arc<TaskStore>>,
}

impl std::fmt::Debug for TaskOutputTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskOutputTool")
            .field("has_task_store", &self.task_store.is_some())
            .finish()
    }
}

impl TaskOutputTool {
    pub fn new(
        manager: Arc<BackgroundTaskManager>,
        thread_store: Option<Arc<dyn ThreadStore>>,
    ) -> Self {
        Self {
            manager,
            task_store: thread_store.map(TaskStore::new).map(Arc::new),
        }
    }

    pub fn with_task_store(mut self, task_store: Option<Arc<TaskStore>>) -> Self {
        self.task_store = task_store;
        self
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskOutputArgs {
    /// The task ID to read output from.
    task_id: String,
}

#[async_trait]
impl crate::contracts::runtime::tool_call::TypedTool for TaskOutputTool {
    type Args = TaskOutputArgs;

    fn tool_id(&self) -> &str {
        TASK_OUTPUT_TOOL_ID
    }
    fn name(&self) -> &str {
        "Task Output"
    }
    fn description(&self) -> &str {
        "Read the output of a background task. \
         For agent runs, returns the last assistant message. \
         For other tasks, returns the task result."
    }

    fn validate(&self, args: &Self::Args) -> Result<(), String> {
        if args.task_id.trim().is_empty() {
            return Err("task_id cannot be empty".to_string());
        }
        Ok(())
    }

    async fn execute(
        &self,
        args: TaskOutputArgs,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let task_id = &args.task_id;

        let Some(thread_id) = owner_thread_id(ctx) else {
            return Ok(ToolResult::error(
                TASK_OUTPUT_TOOL_ID,
                "Missing caller thread context",
            ));
        };

        let Some(task_store) = &self.task_store else {
            if let Some(summary) = self.manager.get(&thread_id, task_id).await {
                return Ok(ToolResult::success(
                    TASK_OUTPUT_TOOL_ID,
                    json!({
                        "task_id": task_id,
                        "task_type": summary.task_type,
                        "status": summary.status.as_str(),
                        "output": summary.result,
                    }),
                ));
            }
            return Ok(ToolResult::error(
                TASK_OUTPUT_TOOL_ID,
                format!("Unknown task_id: {task_id}"),
            ));
        };

        let Some(task) = task_store
            .load_task_for_owner(&thread_id, task_id)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("task store lookup failed: {e}")))?
        else {
            return Ok(ToolResult::error(
                TASK_OUTPUT_TOOL_ID,
                format!("Unknown task_id: {task_id}"),
            ));
        };

        Ok(self.output_from_task(task_id, &task).await)
    }
}

impl TaskOutputTool {
    async fn output_from_task(&self, task_id: &str, task: &TaskState) -> ToolResult {
        let live = self
            .manager
            .get(&task.owner_thread_id, task_id)
            .await
            .filter(|summary| summary.status == task.status || task.status == TaskStatus::Running);

        let output = if task.task_type == "agent_run" {
            match &self.task_store {
                Some(store) => match store.load_output_text(task).await {
                    Ok(output) => output.map(Value::String),
                    Err(e) => {
                        return ToolResult::error(TASK_OUTPUT_TOOL_ID, e.to_string());
                    }
                },
                None => None,
            }
        } else {
            live.and_then(|summary| summary.result)
                .or_else(|| task.result.clone())
        };

        ToolResult::success(
            TASK_OUTPUT_TOOL_ID,
            json!({
                "task_id": task_id,
                "task_type": task.task_type.clone(),
                "agent_id": task.metadata.get("agent_id").cloned().unwrap_or(Value::Null),
                "status": task.status.as_str(),
                "output": output,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::runtime::tool_call::Tool;
    use crate::contracts::storage::ThreadStore;
    use crate::contracts::RunConfig;
    use crate::runtime::background_tasks::SpawnParams;
    use tirea_contract::testing::TestFixture;

    const THREAD_ID_KEY: &str = "__agent_tool_caller_thread_id";

    fn fixture_with_thread(thread_id: &str) -> TestFixture {
        let mut fix = TestFixture::new();
        fix.run_config = {
            let mut rc = RunConfig::new();
            rc.set(THREAD_ID_KEY, thread_id.to_string()).unwrap();
            rc
        };
        fix
    }

    #[test]
    fn task_status_descriptor_has_optional_task_id() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tool = TaskStatusTool::new(mgr);
        let desc = tool.descriptor();
        assert_eq!(desc.id, TASK_STATUS_TOOL_ID);
        // task_id is not in "required" — it's Option<String>.
        let required = desc.parameters.get("required");
        assert!(required.is_none() || required.unwrap().as_array().unwrap().is_empty());
        assert!(desc.parameters["properties"].get("task_id").is_some());
    }

    #[test]
    fn task_cancel_descriptor_requires_task_id() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tool = TaskCancelTool::new(mgr);
        let desc = tool.descriptor();
        assert_eq!(desc.id, TASK_CANCEL_TOOL_ID);
        let required = desc.parameters["required"].as_array().unwrap();
        assert!(required.contains(&json!("task_id")));
    }

    // -----------------------------------------------------------------------
    // TaskStatusTool execute() tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn status_tool_missing_thread_context_returns_error() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tool = TaskStatusTool::new(mgr);
        let fix = TestFixture::new(); // no __agent_tool_caller_thread_id
        let result = tool.execute(json!({}), &fix.ctx()).await.unwrap();
        assert!(!result.is_success());
        assert!(result
            .message
            .as_deref()
            .unwrap_or("")
            .contains("Missing caller thread context"));
    }

    #[tokio::test]
    async fn status_tool_list_all_when_no_tasks() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tool = TaskStatusTool::new(mgr);
        let fix = fixture_with_thread("thread-1");
        let result = tool.execute(json!({}), &fix.ctx()).await.unwrap();
        assert!(result.is_success());
        let content: Value = result.data.clone();
        assert_eq!(content["total"], 0);
        assert!(content["tasks"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn status_tool_query_single_task() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tid = mgr
            .spawn("thread-1", "shell", "echo hi", |_cancel| async {
                super::super::types::TaskResult::Success(json!({"exit": 0}))
            })
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let tool = TaskStatusTool::new(mgr);
        let fix = fixture_with_thread("thread-1");
        let result = tool
            .execute(json!({"task_id": tid}), &fix.ctx())
            .await
            .unwrap();
        assert!(result.is_success());
        let content: Value = result.data.clone();
        assert_eq!(content["status"], "completed");
        assert_eq!(content["result"]["exit"], 0);
    }

    #[tokio::test]
    async fn status_tool_query_unknown_task_returns_error() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tool = TaskStatusTool::new(mgr);
        let fix = fixture_with_thread("thread-1");
        let result = tool
            .execute(json!({"task_id": "bogus"}), &fix.ctx())
            .await
            .unwrap();
        assert!(!result.is_success());
        assert!(result
            .message
            .as_deref()
            .unwrap_or("")
            .contains("Unknown task_id"));
    }

    #[tokio::test]
    async fn status_tool_list_shows_running_and_completed() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        // Running task.
        let _running = mgr
            .spawn("thread-1", "shell", "long", |cancel| async move {
                cancel.cancelled().await;
                super::super::types::TaskResult::Cancelled
            })
            .await;
        // Completed task.
        mgr.spawn("thread-1", "http", "fetch", |_| async {
            super::super::types::TaskResult::Success(Value::Null)
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let tool = TaskStatusTool::new(mgr);
        let fix = fixture_with_thread("thread-1");
        let result = tool.execute(json!({}), &fix.ctx()).await.unwrap();
        assert!(result.is_success());
        let content: Value = result.data.clone();
        assert_eq!(content["total"], 2);
    }

    #[tokio::test]
    async fn status_tool_thread_isolation() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tid = mgr
            .spawn("thread-A", "shell", "private", |_| async {
                super::super::types::TaskResult::Success(Value::Null)
            })
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let tool = TaskStatusTool::new(mgr);

        // Thread-B cannot see thread-A's task.
        let fix_b = fixture_with_thread("thread-B");
        let result = tool
            .execute(json!({"task_id": tid}), &fix_b.ctx())
            .await
            .unwrap();
        assert!(!result.is_success());

        // Thread-A can see it.
        let fix_a = fixture_with_thread("thread-A");
        let result = tool
            .execute(json!({"task_id": tid}), &fix_a.ctx())
            .await
            .unwrap();
        assert!(result.is_success());
    }

    #[tokio::test]
    async fn status_tool_reads_persisted_task_without_live_manager() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
        let task_store = Arc::new(TaskStore::new(storage as Arc<dyn ThreadStore>));
        task_store
            .create_task(super::super::NewTaskSpec {
                task_id: "task-1".to_string(),
                owner_thread_id: "thread-1".to_string(),
                task_type: "shell".to_string(),
                description: "persisted only".to_string(),
                parent_task_id: None,
                supports_resume: false,
                metadata: json!({}),
            })
            .await
            .unwrap();
        task_store
            .persist_foreground_result(
                "task-1",
                TaskStatus::Completed,
                None,
                Some(json!({"stdout":"done"})),
            )
            .await
            .unwrap();

        let tool = TaskStatusTool::new(mgr).with_task_store(Some(task_store));
        let fix = fixture_with_thread("thread-1");
        let result = tool
            .execute(json!({"task_id": "task-1"}), &fix.ctx())
            .await
            .unwrap();

        assert!(result.is_success());
        assert_eq!(result.data["status"], "completed");
        assert_eq!(result.data["result"]["stdout"], "done");
    }

    #[tokio::test]
    async fn status_tool_does_not_read_cached_derived_view() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tool = TaskStatusTool::new(mgr);
        let mut fix = TestFixture::new_with_state(json!({
            "__derived": {
                "background_tasks": {
                    "tasks": {
                        "ghost": {
                            "task_type": "shell",
                            "description": "ghost task",
                            "status": "running"
                        }
                    },
                    "synced_at_ms": 1
                }
            }
        }));
        fix.run_config = {
            let mut rc = RunConfig::new();
            rc.set(THREAD_ID_KEY, "thread-1").unwrap();
            rc
        };

        let result = tool
            .execute(json!({"task_id": "ghost"}), &fix.ctx())
            .await
            .unwrap();
        assert!(!result.is_success());
        assert!(result
            .message
            .as_deref()
            .unwrap_or("")
            .contains("Unknown task_id"));
    }

    // -----------------------------------------------------------------------
    // TaskCancelTool execute() tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn cancel_tool_missing_thread_context_returns_error() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tool = TaskCancelTool::new(mgr);
        let fix = TestFixture::new();
        let result = tool
            .execute(json!({"task_id": "some"}), &fix.ctx())
            .await
            .unwrap();
        assert!(!result.is_success());
        assert!(result
            .message
            .as_deref()
            .unwrap_or("")
            .contains("Missing caller thread context"));
    }

    #[tokio::test]
    async fn cancel_tool_missing_task_id_param() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tool = TaskCancelTool::new(mgr);
        let fix = fixture_with_thread("thread-1");
        let err = tool.execute(json!({}), &fix.ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn cancel_tool_cancels_running_task() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tid = mgr
            .spawn("thread-1", "shell", "long", |cancel| async move {
                cancel.cancelled().await;
                super::super::types::TaskResult::Cancelled
            })
            .await;

        let tool = TaskCancelTool::new(mgr.clone());
        let fix = fixture_with_thread("thread-1");
        let result = tool
            .execute(json!({"task_id": tid}), &fix.ctx())
            .await
            .unwrap();
        assert!(result.is_success());
        let content: Value = result.data.clone();
        assert!(content["cancelled"].as_bool().unwrap());

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let summary = mgr.get("thread-1", &tid).await.unwrap();
        assert_eq!(summary.status, super::super::types::TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn cancel_tool_cancels_descendants_by_default() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let root_token = crate::loop_runtime::loop_runner::RunCancellationToken::new();
        let child_token = crate::loop_runtime::loop_runner::RunCancellationToken::new();

        mgr.spawn_with_id(
            SpawnParams {
                task_id: "root".to_string(),
                owner_thread_id: "thread-1".to_string(),
                task_type: "agent_run".to_string(),
                description: "agent:root".to_string(),
                parent_task_id: None,
                metadata: json!({}),
            },
            root_token,
            |cancel| async move {
                cancel.cancelled().await;
                super::super::types::TaskResult::Cancelled
            },
        )
        .await;

        mgr.spawn_with_id(
            SpawnParams {
                task_id: "child".to_string(),
                owner_thread_id: "thread-1".to_string(),
                task_type: "agent_run".to_string(),
                description: "agent:child".to_string(),
                parent_task_id: Some("root".to_string()),
                metadata: json!({}),
            },
            child_token,
            |cancel| async move {
                cancel.cancelled().await;
                super::super::types::TaskResult::Cancelled
            },
        )
        .await;

        let tool = TaskCancelTool::new(mgr.clone());
        let fix = fixture_with_thread("thread-1");
        let result = tool
            .execute(json!({"task_id": "root"}), &fix.ctx())
            .await
            .unwrap();

        assert!(result.is_success());
        assert_eq!(result.data["cancelled_count"], 2);
        assert!(result.data["cancelled_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "root"));
        assert!(result.data["cancelled_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "child"));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            mgr.get("thread-1", "root").await.unwrap().status,
            super::super::types::TaskStatus::Cancelled
        );
        assert_eq!(
            mgr.get("thread-1", "child").await.unwrap().status,
            super::super::types::TaskStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn cancel_tool_marks_cancel_requested_in_task_store() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
        let task_store = Arc::new(TaskStore::new(storage as Arc<dyn ThreadStore>));
        task_store
            .create_task(super::super::NewTaskSpec {
                task_id: "task-1".to_string(),
                owner_thread_id: "thread-1".to_string(),
                task_type: "shell".to_string(),
                description: "long task".to_string(),
                parent_task_id: None,
                supports_resume: false,
                metadata: json!({}),
            })
            .await
            .unwrap();

        mgr.spawn_with_id(
            SpawnParams {
                task_id: "task-1".to_string(),
                owner_thread_id: "thread-1".to_string(),
                task_type: "shell".to_string(),
                description: "long task".to_string(),
                parent_task_id: None,
                metadata: json!({}),
            },
            crate::loop_runtime::loop_runner::RunCancellationToken::new(),
            |cancel| async move {
                cancel.cancelled().await;
                super::super::types::TaskResult::Cancelled
            },
        )
        .await;

        let tool = TaskCancelTool::new(mgr).with_task_store(Some(task_store.clone()));
        let fix = fixture_with_thread("thread-1");
        let result = tool
            .execute(json!({"task_id": "task-1"}), &fix.ctx())
            .await
            .unwrap();
        assert!(result.is_success());

        let task = task_store
            .load_task("task-1")
            .await
            .unwrap()
            .expect("task should exist");
        assert!(task.cancel_requested_at_ms.is_some());
    }

    #[tokio::test]
    async fn cancel_tool_unknown_task_returns_error() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tool = TaskCancelTool::new(mgr);
        let fix = fixture_with_thread("thread-1");
        let result = tool
            .execute(json!({"task_id": "nope"}), &fix.ctx())
            .await
            .unwrap();
        assert!(!result.is_success());
        assert!(result
            .message
            .as_deref()
            .unwrap_or("")
            .contains("Unknown task_id"));
    }

    #[tokio::test]
    async fn cancel_tool_already_completed_returns_error() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tid = mgr
            .spawn("thread-1", "shell", "done", |_| async {
                super::super::types::TaskResult::Success(Value::Null)
            })
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let tool = TaskCancelTool::new(mgr);
        let fix = fixture_with_thread("thread-1");
        let result = tool
            .execute(json!({"task_id": tid}), &fix.ctx())
            .await
            .unwrap();
        assert!(!result.is_success());
        assert!(result
            .message
            .as_deref()
            .unwrap_or("")
            .contains("not running"));
    }

    #[tokio::test]
    async fn cancel_tool_wrong_owner_rejected() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tid = mgr
            .spawn("thread-1", "shell", "private", |cancel| async move {
                cancel.cancelled().await;
                super::super::types::TaskResult::Cancelled
            })
            .await;

        let tool = TaskCancelTool::new(mgr);
        let fix = fixture_with_thread("thread-2");
        let result = tool
            .execute(json!({"task_id": tid}), &fix.ctx())
            .await
            .unwrap();
        assert!(!result.is_success());
    }

    // -----------------------------------------------------------------------
    // TaskOutputTool execute() tests
    // -----------------------------------------------------------------------

    #[test]
    fn output_tool_descriptor_requires_task_id() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tool = TaskOutputTool::new(mgr, None);
        let desc = tool.descriptor();
        assert_eq!(desc.id, TASK_OUTPUT_TOOL_ID);
        let required = desc.parameters["required"].as_array().unwrap();
        assert!(required.contains(&json!("task_id")));
    }

    #[tokio::test]
    async fn output_tool_missing_task_id_returns_error() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tool = TaskOutputTool::new(mgr, None);
        let fix = fixture_with_thread("thread-1");
        let err = tool.execute(json!({}), &fix.ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn output_tool_unknown_task_returns_error() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tool = TaskOutputTool::new(mgr, None);
        let fix = fixture_with_thread("thread-1");
        let result = tool
            .execute(json!({"task_id": "nonexistent"}), &fix.ctx())
            .await
            .unwrap();
        assert!(!result.is_success());
        assert!(result
            .message
            .as_deref()
            .unwrap_or("")
            .contains("Unknown task_id"));
    }

    #[tokio::test]
    async fn output_tool_returns_result_for_non_agent_task() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tid = mgr
            .spawn("thread-1", "shell", "echo hi", |_| async {
                super::super::types::TaskResult::Success(json!({"exit_code": 0, "stdout": "hi"}))
            })
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let tool = TaskOutputTool::new(mgr, None);
        let fix = fixture_with_thread("thread-1");
        let result = tool
            .execute(json!({"task_id": tid}), &fix.ctx())
            .await
            .unwrap();
        assert!(result.is_success());
        assert_eq!(result.data["task_type"], "shell");
        assert_eq!(result.data["status"], "completed");
        assert_eq!(result.data["output"]["exit_code"], 0);
        assert_eq!(result.data["output"]["stdout"], "hi");
    }

    #[tokio::test]
    async fn output_tool_reads_persisted_state_from_task_store() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
        let task_store = Arc::new(TaskStore::new(storage as Arc<dyn ThreadStore>));
        task_store
            .create_task(super::super::NewTaskSpec {
                task_id: "run-1".to_string(),
                owner_thread_id: "thread-1".to_string(),
                task_type: "shell".to_string(),
                description: "echo test".to_string(),
                parent_task_id: None,
                supports_resume: false,
                metadata: json!({}),
            })
            .await
            .unwrap();
        task_store
            .persist_foreground_result(
                "run-1",
                TaskStatus::Completed,
                None,
                Some(json!({"stdout":"test"})),
            )
            .await
            .unwrap();

        let tool = TaskOutputTool::new(mgr, None).with_task_store(Some(task_store));
        let fix = fixture_with_thread("thread-1");
        let result = tool
            .execute(json!({"task_id": "run-1"}), &fix.ctx())
            .await
            .unwrap();
        assert!(result.is_success());
        assert_eq!(result.data["task_type"], "shell");
        assert_eq!(result.data["status"], "completed");
        assert_eq!(result.data["output"]["stdout"], "test");
    }

    #[tokio::test]
    async fn output_tool_without_task_store_cannot_read_persisted_task() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tool = TaskOutputTool::new(mgr, None);
        let fix = fixture_with_thread("thread-1");
        let result = tool
            .execute(json!({"task_id": "run-1"}), &fix.ctx())
            .await
            .unwrap();
        assert!(!result.is_success());
        assert!(result
            .message
            .as_deref()
            .unwrap_or("")
            .contains("Unknown task_id"));
    }

    #[tokio::test]
    async fn output_tool_does_not_read_cached_derived_view() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tool = TaskOutputTool::new(mgr, None);
        let mut fix = TestFixture::new_with_state(json!({
            "__derived": {
                "background_tasks": {
                    "tasks": {
                        "ghost": {
                            "task_type": "shell",
                            "description": "ghost task",
                            "status": "running"
                        }
                    },
                    "synced_at_ms": 1
                }
            }
        }));
        fix.run_config = {
            let mut rc = RunConfig::new();
            rc.set(THREAD_ID_KEY, "thread-1").unwrap();
            rc
        };

        let result = tool
            .execute(json!({"task_id": "ghost"}), &fix.ctx())
            .await
            .unwrap();
        assert!(!result.is_success());
        assert!(result
            .message
            .as_deref()
            .unwrap_or("")
            .contains("Unknown task_id"));
    }

    #[tokio::test]
    async fn output_tool_thread_isolation() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let tid = mgr
            .spawn("thread-A", "shell", "private", |_| async {
                super::super::types::TaskResult::Success(json!("secret"))
            })
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let tool = TaskOutputTool::new(mgr, None);

        // Thread-B cannot see thread-A's task.
        let fix_b = fixture_with_thread("thread-B");
        let result = tool
            .execute(json!({"task_id": tid}), &fix_b.ctx())
            .await
            .unwrap();
        assert!(!result.is_success());

        // Thread-A can see it.
        let fix_a = fixture_with_thread("thread-A");
        let result = tool
            .execute(json!({"task_id": tid}), &fix_a.ctx())
            .await
            .unwrap();
        assert!(result.is_success());
    }
}
