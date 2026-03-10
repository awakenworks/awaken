//! `BackgroundCapable<T>` — decorator that adds `run_in_background` support to any tool.
//!
//! Tools that support background execution must implement [`BackgroundExecutable`]
//! in addition to [`Tool`]. The trait provides a context-free execution path
//! that can safely cross `tokio::spawn` boundaries.

use super::manager::BackgroundTaskManager;
use super::types::*;
use super::{new_task_id, NewTaskSpec, TaskStore};
use crate::contracts::runtime::tool_call::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult,
};
use crate::loop_runtime::loop_runner::RunCancellationToken;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

const RUN_IN_BACKGROUND_PARAM: &str = "run_in_background";

/// Context-free background execution trait.
///
/// Tools implement this to opt into background execution support.
/// Unlike [`Tool::execute`], this method receives only owned data
/// (no borrowed `ToolCallContext`) so it can be spawned across task boundaries.
#[async_trait]
pub trait BackgroundExecutable: Tool {
    /// Execute the tool logic in background mode.
    ///
    /// This receives the args and a cancellation token but NO `ToolCallContext`.
    /// Tools that need state access should capture what they need from args
    /// before the spawn boundary.
    async fn execute_background(
        &self,
        args: Value,
        cancel_token: RunCancellationToken,
    ) -> TaskResult;
}

/// Wraps a tool that implements [`BackgroundExecutable`] to support
/// `run_in_background: true` as a tool parameter.
///
/// - Foreground: delegates directly to inner `Tool::execute`.
/// - Background: calls `BackgroundExecutable::execute_background` in a spawned task,
///   returns immediately with a `task_id`.
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

        if !background {
            let clean_args = strip_background_param(args);
            return self.inner.execute(clean_args, ctx).await;
        }

        let tool_name = self.inner.descriptor().name.clone();
        let description = format!(
            "{} (background)",
            args.get("description")
                .or_else(|| args.get("command"))
                .and_then(Value::as_str)
                .unwrap_or(&tool_name)
        );

        let owner_thread_id = ctx
            .run_config()
            .value("__agent_tool_caller_thread_id")
            .and_then(Value::as_str)
            .unwrap_or(ctx.source())
            .to_string();

        let clean_args = strip_background_param(args);
        let inner = self.inner.clone();

        let task_id = new_task_id();
        if let Some(task_store) = &self.task_store {
            task_store
                .create_task(NewTaskSpec {
                    task_id: task_id.clone(),
                    owner_thread_id: owner_thread_id.clone(),
                    task_type: tool_name.clone(),
                    description: description.clone(),
                    parent_task_id: None,
                    supports_resume: false,
                    metadata: Value::Object(serde_json::Map::new()),
                })
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("task persistence failed: {e}")))?;
        }

        let task_id = self
            .manager
            .spawn_with_id(
                SpawnParams {
                    task_id,
                    owner_thread_id,
                    task_type: tool_name.clone(),
                    description,
                    parent_task_id: None,
                    metadata: Value::Object(serde_json::Map::new()),
                },
                RunCancellationToken::new(),
                move |cancel_token| async move {
                    inner.execute_background(clean_args, cancel_token).await
                },
            )
            .await;

        Ok(ToolResult::success(
            &tool_name,
            json!({
                "task_id": task_id,
                "status": "running_in_background",
                "message": format!(
                    "Task started in background. Use task_status tool with task_id '{}' to check progress.",
                    task_id
                ),
            }),
        ))
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
        async fn execute_background(
            &self,
            args: Value,
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
        fix.run_config = {
            let mut rc = crate::contracts::RunConfig::new();
            rc.set("__agent_tool_caller_thread_id", "thread-1".to_string())
                .unwrap();
            rc
        };

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
        fix.run_config = {
            let mut rc = crate::contracts::RunConfig::new();
            rc.set("__agent_tool_caller_thread_id", "thread-1".to_string())
                .unwrap();
            rc
        };

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
        async fn execute_background(
            &self,
            _args: Value,
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
        fix.run_config = {
            let mut rc = crate::contracts::RunConfig::new();
            rc.set("__agent_tool_caller_thread_id", "thread-1".to_string())
                .unwrap();
            rc
        };

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
