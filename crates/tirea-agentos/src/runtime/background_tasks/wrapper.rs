//! `BackgroundCapable<T>` — decorator that adds `run_in_background` support to any tool.
//!
//! Tools that support background execution must implement [`BackgroundExecutable`]
//! in addition to [`Tool`]. The trait provides a context-free execution path
//! that can safely cross `tokio::spawn` boundaries.
//!
//! The wrapper handles the full task lifecycle:
//! - Persistence: create/resume tasks in [`TaskStore`]
//! - Background spawning via [`BackgroundTaskManager`]
//! - Resume: pass existing `task_id` to re-execute stopped tasks
//! - Status queries: return current status for running/terminal tasks

use super::manager::BackgroundTaskManager;
use super::types::*;
use super::{new_task_id, NewTaskSpec, TaskStore, TaskStoreError};
use crate::contracts::runtime::tool_call::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult,
};
use crate::runtime::loop_runner::RunCancellationToken;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

const RUN_IN_BACKGROUND_PARAM: &str = "run_in_background";

/// Typed runtime context passed into background executions after the spawn boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackgroundExecutionContext {
    pub owner_thread_id: String,
    pub parent_task_id: Option<String>,
    pub is_resume: bool,
}

/// Context-free background execution trait.
///
/// Tools implement this to opt into background execution support via
/// [`BackgroundCapable`]. The wrapper handles persistence, resume, and
/// background spawning — tools only implement execution logic.
#[async_trait]
pub trait BackgroundExecutable: Tool {
    /// Background-only prepared input computed before crossing the spawn boundary.
    type PreparedBackground: Send + 'static;

    /// Task type label for persistence (e.g. `"agent_run"`, `"shell"`).
    fn task_type(&self) -> &str;

    /// Whether stopped tasks can be resumed with the same task_id.
    fn supports_resume(&self) -> bool {
        false
    }

    /// Metadata to persist alongside the task (e.g. agent_id, thread_id).
    fn task_metadata(&self, _args: &Value) -> Value {
        json!({})
    }

    /// Extract an existing task_id from args (for resume/status queries).
    /// Return `None` for new tasks.
    fn task_id_from_args(&self, _args: &Value) -> Option<String> {
        None
    }

    /// Inject a generated task_id into the args before passing to execute.
    /// Default implementation sets `args["task_id"]`.
    fn set_task_id_in_args(&self, args: &mut Value, task_id: &str) {
        if let Some(obj) = args.as_object_mut() {
            obj.insert("task_id".to_string(), json!(task_id));
        }
    }

    /// Human-readable task description for the background task listing.
    fn task_description(&self, args: &Value) -> String {
        let tool_name = self.descriptor().name.clone();
        format!(
            "{} (background)",
            args.get("description")
                .or_else(|| args.get("command"))
                .and_then(Value::as_str)
                .unwrap_or(&tool_name)
        )
    }

    /// Validate and normalize arguments for background execution before any
    /// task state is persisted or spawned.
    ///
    /// Tools can use this to perform the same semantic checks they enforce in
    /// foreground mode and to inject hidden, precomputed execution inputs that
    /// can safely cross the spawn boundary.
    #[allow(clippy::result_large_err)]
    fn prepare_background(
        &self,
        _args: &mut Value,
        _ctx: &ToolCallContext<'_>,
        _is_resume: bool,
    ) -> Result<Self::PreparedBackground, ToolResult>;

    /// Execute the tool logic in foreground mode.
    ///
    /// Tools can override this when foreground execution needs typed control
    /// data (for example, resume state) that should not be smuggled through
    /// hidden JSON fields.
    async fn execute_foreground(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
        _is_resume: bool,
    ) -> Result<ToolResult, ToolError> {
        self.execute(args, ctx).await
    }

    /// Derive the durable terminal task status for a foreground execution.
    ///
    /// The default implementation treats tool-level errors as failed tasks and
    /// every other result as completed.
    fn foreground_task_status(&self, result: &ToolResult) -> (TaskStatus, Option<String>) {
        if result.is_error() {
            (TaskStatus::Failed, result.message.clone())
        } else {
            (TaskStatus::Completed, None)
        }
    }

    /// Execute the tool logic in background mode.
    ///
    /// Receives a `task_id` (new or resumed) and a cancellation token but NO
    /// `ToolCallContext`. Tools that need state access should capture what they
    /// need from args before the spawn boundary.
    async fn execute_background(
        &self,
        task_id: &str,
        args: Value,
        prepared: Self::PreparedBackground,
        execution: BackgroundExecutionContext,
        cancel_token: RunCancellationToken,
    ) -> TaskResult;
}

/// Wraps a tool that implements [`BackgroundExecutable`] to provide full task
/// lifecycle management:
///
/// - **New tasks**: generates `task_id`, persists, executes foreground or spawns background.
/// - **Resume**: if `task_id` is in args and the task is `Stopped`, re-executes.
/// - **Status query**: if `task_id` is in args and the task is running/terminal, returns status.
/// - **Orphan detection**: if a persisted `Running` task has no live handle, marks it `Stopped`.
pub struct BackgroundCapable<T: BackgroundExecutable> {
    inner: Arc<T>,
    manager: Arc<BackgroundTaskManager>,
    task_store: Option<Arc<TaskStore>>,
}

impl<T: BackgroundExecutable> BackgroundCapable<T> {
    pub fn new(inner: T, manager: Arc<BackgroundTaskManager>) -> Self {
        Self {
            inner: Arc::new(inner),
            manager,
            task_store: None,
        }
    }

    pub fn from_arc(inner: Arc<T>, manager: Arc<BackgroundTaskManager>) -> Self {
        Self {
            inner,
            manager,
            task_store: None,
        }
    }

    pub fn with_task_store(mut self, task_store: Option<Arc<TaskStore>>) -> Self {
        self.task_store = task_store;
        self
    }
}

/// Internal: result of looking up a task by id.
struct TaskLookup {
    summary: TaskSummary,
    /// Whether this summary came from a live in-memory handle (vs durable store only).
    is_live: bool,
}

/// Bundled execution parameters for `execute_task`.
struct ExecuteParams<'a> {
    task_id: &'a str,
    owner_thread_id: &'a str,
    background: bool,
    is_resume: bool,
    parent_task_id: Option<&'a str>,
}

impl<T: BackgroundExecutable + 'static> BackgroundCapable<T> {
    /// Look up a task by id, merging live (in-memory) and durable (TaskStore) sources.
    /// Live takes precedence when both exist.
    async fn lookup_task(
        &self,
        owner_thread_id: &str,
        task_id: &str,
    ) -> Result<Option<TaskLookup>, TaskStoreError> {
        let live = self.manager.get(owner_thread_id, task_id).await;
        if let Some(summary) = live {
            return Ok(Some(TaskLookup {
                summary,
                is_live: true,
            }));
        }

        if let Some(store) = &self.task_store {
            if let Some(task) = store.load_task_for_owner(owner_thread_id, task_id).await? {
                return Ok(Some(TaskLookup {
                    summary: task.summary(),
                    is_live: false,
                }));
            }
        }

        Ok(None)
    }

    /// Persist task creation (new) or attempt increment (resume).
    async fn persist_start(
        &self,
        task_id: &str,
        owner_thread_id: &str,
        is_resume: bool,
        description: &str,
        parent_task_id: Option<&str>,
        metadata: &Value,
    ) -> Result<(), ToolError> {
        let Some(store) = &self.task_store else {
            return Ok(());
        };

        if is_resume {
            // Verify ownership and increment attempt.
            match store.load_task(task_id).await {
                Ok(Some(task)) => {
                    if task.owner_thread_id != owner_thread_id {
                        return Err(ToolError::ExecutionFailed(format!(
                            "task '{}' belongs to a different owner",
                            task_id
                        )));
                    }
                    store.start_task_attempt(task_id).await.map_err(|e| {
                        ToolError::ExecutionFailed(format!("task persist failed: {e}"))
                    })?;
                }
                Ok(None) => {
                    // Resuming a non-existent task — create it.
                    self.create_task(
                        store,
                        task_id,
                        owner_thread_id,
                        description,
                        parent_task_id,
                        metadata,
                    )
                    .await?;
                }
                Err(e) => {
                    return Err(ToolError::ExecutionFailed(format!(
                        "task persist failed: {e}"
                    )));
                }
            }
        } else {
            self.create_task(
                store,
                task_id,
                owner_thread_id,
                description,
                parent_task_id,
                metadata,
            )
            .await?;
        }

        Ok(())
    }

    async fn create_task(
        &self,
        store: &TaskStore,
        task_id: &str,
        owner_thread_id: &str,
        description: &str,
        parent_task_id: Option<&str>,
        metadata: &Value,
    ) -> Result<(), ToolError> {
        store
            .create_task(NewTaskSpec {
                task_id: task_id.to_string(),
                owner_thread_id: owner_thread_id.to_string(),
                task_type: self.inner.task_type().to_string(),
                description: description.to_string(),
                parent_task_id: parent_task_id.map(str::to_string),
                supports_resume: self.inner.supports_resume(),
                metadata: metadata.clone(),
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("task persist failed: {e}")))?;
        Ok(())
    }

    /// Persist terminal status after foreground completion.
    async fn persist_result(
        &self,
        task_id: &str,
        status: TaskStatus,
        error: Option<String>,
    ) -> Result<(), ToolError> {
        let Some(store) = &self.task_store else {
            return Ok(());
        };
        store
            .persist_foreground_result(task_id, status, error, None)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("task persist failed: {e}")))?;
        Ok(())
    }

    /// Mark an orphaned running task as stopped.
    async fn mark_orphan_stopped(&self, task_id: &str) -> Result<(), ToolError> {
        let Some(store) = &self.task_store else {
            return Ok(());
        };
        store
            .persist_foreground_result(
                task_id,
                TaskStatus::Stopped,
                Some("No live executor found in current process; marked stopped".to_string()),
                None,
            )
            .await
            .map_err(|e| {
                ToolError::ExecutionFailed(format!("failed to mark orphan stopped: {e}"))
            })?;
        Ok(())
    }

    fn status_result(&self, task_id: &str, summary: &TaskSummary) -> ToolResult {
        let tool_name = self.inner.descriptor().name.clone();
        let agent_id = summary
            .metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        ToolResult::success(
            &tool_name,
            json!({
                "task_id": task_id,
                "agent_id": agent_id,
                "status": summary.status.as_str(),
                "error": summary.error,
            }),
        )
    }

    fn extract_owner_thread_id(&self, ctx: &ToolCallContext<'_>) -> String {
        ctx.caller_context()
            .thread_id()
            .unwrap_or(ctx.source())
            .to_string()
    }

    /// Merge stored task metadata into args for resume (fills in agent_id etc.
    /// that the caller may not have supplied).
    fn enrich_args_for_resume(args: &mut Value, metadata: &Value) {
        if let (Some(obj), Some(meta)) = (args.as_object_mut(), metadata.as_object()) {
            for (k, v) in meta {
                if !obj.contains_key(k) {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
    }

    /// Handle the resume/status query path when a task_id is found in args.
    async fn handle_existing_task(
        &self,
        mut args: Value,
        ctx: &ToolCallContext<'_>,
        task_id: String,
        owner_thread_id: &str,
        background: bool,
        parent_task_id: Option<&str>,
    ) -> Result<ToolResult, ToolError> {
        let lookup = self
            .lookup_task(owner_thread_id, &task_id)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("task lookup failed: {e}")))?;

        let Some(lookup) = lookup else {
            return Ok(ToolResult::error(
                self.inner.descriptor().name,
                format!("Unknown task: {task_id}"),
            ));
        };

        match lookup.summary.status {
            // Running or terminal: return current status without executing.
            TaskStatus::Running
            | TaskStatus::Completed
            | TaskStatus::Failed
            | TaskStatus::Cancelled => {
                // Orphan detection: persisted Running but no live handle.
                if lookup.summary.status == TaskStatus::Running && !lookup.is_live {
                    self.mark_orphan_stopped(&task_id).await?;
                    // Return stopped status with orphan message.
                    let mut stopped_summary = lookup.summary.clone();
                    stopped_summary.status = TaskStatus::Stopped;
                    stopped_summary.error = Some(
                        "No live executor found in current process; marked stopped".to_string(),
                    );
                    if self.inner.supports_resume() {
                        Self::enrich_args_for_resume(&mut args, &lookup.summary.metadata);
                        return self
                            .execute_task(
                                args,
                                ctx,
                                &ExecuteParams {
                                    task_id: &task_id,
                                    owner_thread_id,
                                    background,
                                    is_resume: true,
                                    parent_task_id,
                                },
                            )
                            .await;
                    }
                    return Ok(self.status_result(&task_id, &stopped_summary));
                }
                Ok(self.status_result(&task_id, &lookup.summary))
            }
            TaskStatus::Stopped => {
                if !self.inner.supports_resume() {
                    return Ok(self.status_result(&task_id, &lookup.summary));
                }
                // Resume: re-execute with same task_id, enriched with stored metadata.
                Self::enrich_args_for_resume(&mut args, &lookup.summary.metadata);
                self.execute_task(
                    args,
                    ctx,
                    &ExecuteParams {
                        task_id: &task_id,
                        owner_thread_id,
                        background,
                        is_resume: true,
                        parent_task_id,
                    },
                )
                .await
            }
        }
    }

    /// Core execution path for both new and resumed tasks.
    async fn execute_task(
        &self,
        mut args: Value,
        ctx: &ToolCallContext<'_>,
        params: &ExecuteParams<'_>,
    ) -> Result<ToolResult, ToolError> {
        let prepared = if params.background {
            match self
                .inner
                .prepare_background(&mut args, ctx, params.is_resume)
            {
                Ok(prepared) => Some(prepared),
                Err(result) => return Ok(result),
            }
        } else {
            None
        };

        let description = self.inner.task_description(&args);
        let metadata = self.inner.task_metadata(&args);

        // Persist start (create or increment attempt).
        self.persist_start(
            params.task_id,
            params.owner_thread_id,
            params.is_resume,
            &description,
            params.parent_task_id,
            &metadata,
        )
        .await?;

        if params.background {
            let inner = self.inner.clone();
            let task_id_owned = params.task_id.to_string();
            let cancel_token = RunCancellationToken::new();
            let spawn_token = cancel_token.clone();
            let execution = BackgroundExecutionContext {
                owner_thread_id: params.owner_thread_id.to_string(),
                parent_task_id: params.parent_task_id.map(str::to_string),
                is_resume: params.is_resume,
            };
            let prepared = prepared.expect("background preparation should exist");

            self.manager
                .spawn_with_id(
                    SpawnParams {
                        task_id: params.task_id.to_string(),
                        owner_thread_id: params.owner_thread_id.to_string(),
                        task_type: self.inner.task_type().to_string(),
                        description,
                        parent_task_id: params.parent_task_id.map(str::to_string),
                        metadata,
                    },
                    cancel_token,
                    move |_cancel| async move {
                        inner
                            .execute_background(
                                &task_id_owned,
                                args,
                                prepared,
                                execution,
                                spawn_token,
                            )
                            .await
                    },
                )
                .await;

            let tool_name = self.inner.descriptor().name.clone();
            Ok(ToolResult::success(
                &tool_name,
                json!({
                    "task_id": params.task_id,
                    "status": "running_in_background",
                    "message": format!(
                        "Task started in background. Use task_status tool with task_id '{}' to check progress.",
                        params.task_id
                    ),
                }),
            ))
        } else {
            let result = self
                .inner
                .execute_foreground(args, ctx, params.is_resume)
                .await?;

            // Infer terminal status from ToolResult for persistence.
            let (status, error) = self.inner.foreground_task_status(&result);
            self.persist_result(params.task_id, status, error).await?;

            Ok(result)
        }
    }
}

#[async_trait]
impl<T: BackgroundExecutable + 'static> Tool for BackgroundCapable<T> {
    fn descriptor(&self) -> ToolDescriptor {
        let mut desc = self.inner.descriptor();
        inject_background_param(&mut desc.parameters);
        desc
    }

    fn validate_args(&self, args: &Value) -> Result<(), ToolError> {
        let mut stripped = args.clone();
        if let Some(obj) = stripped.as_object_mut() {
            obj.remove(RUN_IN_BACKGROUND_PARAM);
        }
        self.inner.validate_args(&stripped)
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let background = args
            .get(RUN_IN_BACKGROUND_PARAM)
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let mut clean_args = strip_background_param(args);
        let owner_thread_id = self.extract_owner_thread_id(ctx);

        // Check for existing task_id (resume/status query).
        let existing_task_id = self.inner.task_id_from_args(&clean_args);

        // Determine parent task_id from scope if available.
        let parent_task_id: Option<String> = ctx.run_identity().run_id_opt().map(str::to_string);

        if let Some(task_id) = existing_task_id {
            return self
                .handle_existing_task(
                    clean_args,
                    ctx,
                    task_id,
                    &owner_thread_id,
                    background,
                    parent_task_id.as_deref(),
                )
                .await;
        }

        // New task: generate task_id and inject into args.
        let task_id = new_task_id();
        self.inner.set_task_id_in_args(&mut clean_args, &task_id);

        self.execute_task(
            clean_args,
            ctx,
            &ExecuteParams {
                task_id: &task_id,
                owner_thread_id: &owner_thread_id,
                background,
                is_resume: false,
                parent_task_id: parent_task_id.as_deref(),
            },
        )
        .await
    }
}

/// Inject `run_in_background` boolean parameter into a JSON Schema.
fn inject_background_param(schema: &mut Value) {
    if let Some(properties) = schema
        .as_object_mut()
        .and_then(|obj| obj.get_mut("properties"))
        .and_then(Value::as_object_mut)
    {
        properties.insert(
            RUN_IN_BACKGROUND_PARAM.to_string(),
            json!({
                "type": "boolean",
                "description": "If true, execute this tool in the background and return immediately with a task_id. Use task_status to check progress later."
            }),
        );
    }
}

fn strip_background_param(mut args: Value) -> Value {
    if let Some(obj) = args.as_object_mut() {
        obj.remove(RUN_IN_BACKGROUND_PARAM);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("echo", "echo", "Echo back the input").with_parameters(json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                },
                "required": ["message"]
            }))
        }

        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            let msg = args
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("(empty)");
            Ok(ToolResult::success("echo", json!({ "echoed": msg })))
        }
    }

    #[async_trait]
    impl BackgroundExecutable for EchoTool {
        type PreparedBackground = ();

        fn task_type(&self) -> &str {
            "echo"
        }

        fn prepare_background(
            &self,
            _args: &mut Value,
            _ctx: &ToolCallContext<'_>,
            _is_resume: bool,
        ) -> Result<Self::PreparedBackground, ToolResult> {
            Ok(())
        }

        async fn execute_background(
            &self,
            _task_id: &str,
            args: Value,
            _prepared: Self::PreparedBackground,
            _execution: BackgroundExecutionContext,
            _cancel_token: RunCancellationToken,
        ) -> TaskResult {
            let msg = args
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("(empty)");
            TaskResult::Success(json!({ "echoed": msg }))
        }
    }

    #[test]
    fn descriptor_includes_background_param() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let wrapped = BackgroundCapable::new(EchoTool, mgr);
        let desc = wrapped.descriptor();
        let props = desc.parameters["properties"].as_object().unwrap();
        assert!(props.contains_key(RUN_IN_BACKGROUND_PARAM));
    }

    #[test]
    fn inject_background_param_preserves_existing_properties() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "x": { "type": "string" }
            }
        });
        inject_background_param(&mut schema);
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("x"));
        assert!(props.contains_key(RUN_IN_BACKGROUND_PARAM));
    }

    #[test]
    fn strip_background_param_removes_it() {
        let args = json!({
            "message": "hello",
            "run_in_background": true
        });
        let cleaned = strip_background_param(args);
        assert!(cleaned.get("message").is_some());
        assert!(cleaned.get(RUN_IN_BACKGROUND_PARAM).is_none());
    }

    #[test]
    fn validate_args_strips_background_param_before_inner_check() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let wrapped = BackgroundCapable::new(EchoTool, mgr);
        // Inner EchoTool doesn't know about run_in_background.
        let args = json!({
            "message": "hello",
            "run_in_background": true
        });
        assert!(wrapped.validate_args(&args).is_ok());
    }

    #[tokio::test]
    async fn foreground_execution_delegates_to_inner_tool() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let wrapped = BackgroundCapable::new(EchoTool, mgr);

        let fix = tirea_contract::testing::TestFixture::new();
        let result = wrapped
            .execute(
                json!({ "message": "hello", "run_in_background": false }),
                &fix.ctx(),
            )
            .await
            .unwrap();
        assert!(result.is_success());
        let content: Value = result.data.clone();
        assert_eq!(content["echoed"], "hello");
    }

    #[tokio::test]
    async fn foreground_execution_when_param_absent() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let wrapped = BackgroundCapable::new(EchoTool, mgr);

        let fix = tirea_contract::testing::TestFixture::new();
        let result = wrapped
            .execute(json!({ "message": "hi" }), &fix.ctx())
            .await
            .unwrap();
        assert!(result.is_success());
        let content: Value = result.data.clone();
        assert_eq!(content["echoed"], "hi");
    }

    #[tokio::test]
    async fn background_execution_returns_task_id() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let wrapped = BackgroundCapable::new(EchoTool, mgr.clone());

        let mut fix = tirea_contract::testing::TestFixture::new();
        fix.caller_context = crate::contracts::runtime::tool_call::CallerContext::new(
            Some("thread-1".to_string()),
            Some("caller-run".to_string()),
            Some("caller".to_string()),
            vec![],
        );

        let result = wrapped
            .execute(
                json!({ "message": "bg-msg", "run_in_background": true }),
                &fix.ctx_with("call-1", "tool:echo"),
            )
            .await
            .unwrap();
        assert!(result.is_success());
        let content: Value = result.data.clone();
        assert!(content["task_id"].as_str().is_some());
        assert_eq!(content["status"], "running_in_background");

        // Wait for background task to complete and verify via manager.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let task_id = content["task_id"].as_str().unwrap();
        let summary = mgr.get("thread-1", task_id).await.unwrap();
        assert_eq!(summary.status, super::super::types::TaskStatus::Completed);
        assert_eq!(summary.result.unwrap()["echoed"], "bg-msg");
    }

    #[tokio::test]
    async fn background_execution_uses_description_from_args() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let wrapped = BackgroundCapable::new(EchoTool, mgr.clone());

        let mut fix = tirea_contract::testing::TestFixture::new();
        fix.caller_context = crate::contracts::runtime::tool_call::CallerContext::new(
            Some("thread-1".to_string()),
            Some("caller-run".to_string()),
            Some("caller".to_string()),
            vec![],
        );

        let result = wrapped
            .execute(
                json!({
                    "message": "bg",
                    "command": "echo hello",
                    "run_in_background": true
                }),
                &fix.ctx_with("c-1", "tool:echo"),
            )
            .await
            .unwrap();
        let content: Value = result.data.clone();
        let task_id = content["task_id"].as_str().unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let summary = mgr.get("thread-1", task_id).await.unwrap();
        assert!(
            summary.description.contains("echo hello"),
            "description should contain command: {}",
            summary.description
        );
    }

    /// Slow tool that respects cancellation.
    struct SlowTool;

    #[async_trait]
    impl Tool for SlowTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("slow", "slow", "A slow tool").with_parameters(json!({
                "type": "object",
                "properties": {}
            }))
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success("slow", json!("done")))
        }
    }

    #[async_trait]
    impl BackgroundExecutable for SlowTool {
        type PreparedBackground = ();

        fn task_type(&self) -> &str {
            "slow"
        }

        fn prepare_background(
            &self,
            _args: &mut Value,
            _ctx: &ToolCallContext<'_>,
            _is_resume: bool,
        ) -> Result<Self::PreparedBackground, ToolResult> {
            Ok(())
        }

        async fn execute_background(
            &self,
            _task_id: &str,
            _args: Value,
            _prepared: Self::PreparedBackground,
            _execution: BackgroundExecutionContext,
            cancel_token: RunCancellationToken,
        ) -> TaskResult {
            tokio::select! {
                _ = cancel_token.cancelled() => TaskResult::Cancelled,
                _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {
                    TaskResult::Success(json!("completed"))
                }
            }
        }
    }

    #[tokio::test]
    async fn background_cancellation_via_cancel_token() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let wrapped = BackgroundCapable::new(SlowTool, mgr.clone());

        let mut fix = tirea_contract::testing::TestFixture::new();
        fix.caller_context = crate::contracts::runtime::tool_call::CallerContext::new(
            Some("thread-1".to_string()),
            Some("caller-run".to_string()),
            Some("caller".to_string()),
            vec![],
        );

        let result = wrapped
            .execute(
                json!({ "run_in_background": true }),
                &fix.ctx_with("c-1", "tool:slow"),
            )
            .await
            .unwrap();
        let content: Value = result.data.clone();
        let task_id = content["task_id"].as_str().unwrap();

        // Should be running.
        let summary = mgr.get("thread-1", task_id).await.unwrap();
        assert_eq!(summary.status, super::super::types::TaskStatus::Running);

        // Cancel via manager.
        mgr.cancel("thread-1", task_id).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let summary = mgr.get("thread-1", task_id).await.unwrap();
        assert_eq!(summary.status, super::super::types::TaskStatus::Cancelled);
    }
}
