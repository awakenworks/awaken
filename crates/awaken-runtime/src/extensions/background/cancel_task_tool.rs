//! `cancel_task` tool for self/child background task cancellation.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};

use super::manager::BackgroundTaskManager;

pub const CANCEL_TASK_TOOL_ID: &str = "cancel_task";

/// Structured cancel receipt returned to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelTaskReceipt {
    pub status: &'static str,
    pub root_task_id: String,
    pub cancelled_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelTaskError {
    CurrentTaskUnavailable,
    TaskNotFound,
}

impl std::fmt::Display for CancelTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CurrentTaskUnavailable => write!(f, "current_task_unavailable"),
            Self::TaskNotFound => write!(f, "task_not_found"),
        }
    }
}

/// Tool exposed to agents for cancelling their own background task tree.
pub struct CancelTaskTool {
    manager: Arc<BackgroundTaskManager>,
    current_task_id_override: Option<String>,
}

#[derive(Debug, Clone)]
enum CancelScope {
    Task(String),
    Run(String),
}

impl CancelTaskTool {
    pub fn new(manager: Arc<BackgroundTaskManager>) -> Self {
        Self {
            manager,
            current_task_id_override: None,
        }
    }

    pub(crate) fn with_current_task(
        manager: Arc<BackgroundTaskManager>,
        task_id: impl Into<String>,
    ) -> Self {
        Self {
            manager,
            current_task_id_override: Some(task_id.into()),
        }
    }

    fn current_task_id(&self, _ctx: &ToolCallContext) -> Option<String> {
        self.current_task_id_override
            .clone()
            .or_else(|| super::current_background_task_context().map(|context| context.task_id))
    }

    fn current_scope(&self, ctx: &ToolCallContext) -> Option<CancelScope> {
        if let Some(task_id) = self.current_task_id(ctx) {
            return Some(CancelScope::Task(task_id));
        }

        if ctx.cancellation_token.is_some() && !ctx.run_identity.run_id.trim().is_empty() {
            return Some(CancelScope::Run(ctx.run_identity.run_id.clone()));
        }

        None
    }

    fn resolve_child(&self, scope: &CancelScope, name: &str) -> Option<String> {
        match scope {
            CancelScope::Task(task_id) => self.manager.resolve_live_child_task(task_id, name),
            CancelScope::Run(run_id) => self.manager.resolve_live_child_run(run_id, name),
        }
    }

    fn make_receipt(root_task_id: String, cancelled_count: usize) -> CancelTaskReceipt {
        CancelTaskReceipt {
            status: "accepted",
            root_task_id,
            cancelled_count,
            error: None,
        }
    }

    fn make_error(root_task_id: String, error: CancelTaskError) -> CancelTaskReceipt {
        CancelTaskReceipt {
            status: "failed",
            root_task_id,
            cancelled_count: 0,
            error: Some(error.to_string()),
        }
    }
}

#[async_trait]
impl Tool for CancelTaskTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            CANCEL_TASK_TOOL_ID,
            CANCEL_TASK_TOOL_ID,
            "Cancel the current background task or one of its child tasks. Descendant tasks are cancelled together.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "target": {
                    "oneOf": [
                        {
                            "type": "object",
                            "properties": {
                                "relation": { "const": "self" }
                            },
                            "required": ["relation"]
                        },
                        {
                            "type": "object",
                            "properties": {
                                "relation": { "const": "child" },
                                "name": { "type": "string", "description": "Direct child task name or task_id" }
                            },
                            "required": ["relation", "name"]
                        }
                    ]
                }
            },
            "required": ["target"]
        }))
    }

    fn validate_args(&self, args: &Value) -> Result<(), ToolError> {
        let target = args
            .get("target")
            .ok_or_else(|| ToolError::InvalidArguments("missing 'target'".into()))?;
        let relation = target
            .get("relation")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("missing 'target.relation'".into()))?;

        match relation {
            "self" => Ok(()),
            "child" => {
                if target.get("name").and_then(Value::as_str).is_none() {
                    Err(ToolError::InvalidArguments("child requires 'name'".into()))
                } else {
                    Ok(())
                }
            }
            other => Err(ToolError::InvalidArguments(format!(
                "unknown relation '{other}'"
            ))),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let current_scope = self.current_scope(ctx);
        let relation = args["target"]["relation"].as_str().unwrap_or_default();
        let (root_task_id, cancelled_count) = match relation {
            "self" => {
                let scope = current_scope.clone().ok_or_else(|| {
                    ToolError::ExecutionFailed(CancelTaskError::CurrentTaskUnavailable.to_string())
                })?;
                let cancelled_count = match &scope {
                    CancelScope::Task(task_id) => self.manager.cancel_tree(task_id).await,
                    CancelScope::Run(run_id) => {
                        let descendant_count =
                            self.manager.cancel_descendants_for_run(run_id).await;
                        descendant_count + usize::from(ctx.cancellation_token.is_some())
                    }
                };
                let root_id = match scope {
                    CancelScope::Task(task_id) => task_id,
                    CancelScope::Run(run_id) => run_id,
                };

                if let Some(cancellation_token) = &ctx.cancellation_token {
                    cancellation_token.cancel();
                }

                (root_id, cancelled_count)
            }
            "child" => {
                let scope = current_scope.clone().ok_or_else(|| {
                    ToolError::ExecutionFailed(CancelTaskError::CurrentTaskUnavailable.to_string())
                })?;
                let name = args["target"]["name"].as_str().unwrap_or_default();
                let Some(child_task_id) = self.resolve_child(&scope, name) else {
                    let receipt = Self::make_error(name.to_string(), CancelTaskError::TaskNotFound);
                    return Ok(ToolResult::success(
                        CANCEL_TASK_TOOL_ID,
                        serde_json::to_value(receipt).unwrap_or_default(),
                    )
                    .into());
                };
                let cancelled_count = self.manager.cancel_tree(&child_task_id).await;
                (child_task_id, cancelled_count)
            }
            _ => {
                return Err(ToolError::InvalidArguments(
                    "unknown cancellation relation".into(),
                ));
            }
        };

        let receipt = if cancelled_count == 0 {
            Self::make_error(root_task_id, CancelTaskError::TaskNotFound)
        } else {
            Self::make_receipt(root_task_id, cancelled_count)
        };

        Ok(ToolResult::success(
            CANCEL_TASK_TOOL_ID,
            serde_json::to_value(receipt).unwrap_or_default(),
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::background::{BackgroundTaskPlugin, TaskParentContext, TaskResult};
    use crate::phase::ExecutionEnv;
    use crate::plugins::Plugin;
    use crate::state::StateStore;
    use awaken_contract::contract::identity::RunIdentity;
    use awaken_contract::registry_spec::AgentSpec;

    fn make_manager_and_store() -> (Arc<BackgroundTaskManager>, StateStore) {
        let store = StateStore::new();
        let manager = Arc::new(BackgroundTaskManager::new());
        manager.set_store(store.clone());
        let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::new(manager.clone()));
        let env = ExecutionEnv::from_plugins(&[plugin], &Default::default()).unwrap();
        store.register_keys(&env.key_registrations).unwrap();
        (manager, store)
    }

    fn make_ctx_with_store_and_task(
        thread_id: &str,
        agent_id: &str,
        store: &StateStore,
        task_id: Option<&str>,
    ) -> ToolCallContext {
        make_ctx_with_store_and_task_and_token(
            thread_id,
            agent_id,
            store,
            task_id,
            Some(crate::cancellation::CancellationToken::new()),
        )
    }

    fn make_ctx_with_store_and_task_and_token(
        thread_id: &str,
        agent_id: &str,
        store: &StateStore,
        _task_id: Option<&str>,
        cancellation_token: Option<crate::cancellation::CancellationToken>,
    ) -> ToolCallContext {
        ToolCallContext {
            call_id: "call-1".into(),
            tool_name: CANCEL_TASK_TOOL_ID.into(),
            run_identity: RunIdentity::new(
                thread_id.to_string(),
                None,
                "run-1".to_string(),
                None,
                agent_id.to_string(),
                awaken_contract::contract::identity::RunOrigin::Subagent,
            ),
            agent_spec: Arc::new(AgentSpec::default()),
            snapshot: store.snapshot(),
            activity_sink: None,
            cancellation_token,
            resume_input: None,
            suspension_id: None,
            suspension_reason: None,
        }
    }

    #[test]
    fn accepts_self_target() {
        let (manager, _store) = make_manager_and_store();
        let tool = CancelTaskTool::new(manager);
        assert!(
            tool.validate_args(&json!({"target": {"relation": "self"}}))
                .is_ok()
        );
    }

    #[test]
    fn rejects_child_without_name() {
        let (manager, _store) = make_manager_and_store();
        let tool = CancelTaskTool::new(manager);
        assert!(
            tool.validate_args(&json!({"target": {"relation": "child"}}))
                .is_err()
        );
    }

    #[tokio::test]
    async fn self_cancel_cascades_to_descendants() {
        let (manager, store) = make_manager_and_store();

        let parent_id = manager
            .spawn(
                "thread-1",
                "root_task",
                Some("root-task"),
                "parent task",
                TaskParentContext::default(),
                |ctx| async move {
                    ctx.cancelled().await;
                    TaskResult::Cancelled
                },
            )
            .await
            .unwrap();

        let child_id = manager
            .spawn(
                "thread-1",
                "child",
                Some("child"),
                "child task",
                TaskParentContext {
                    task_id: Some(parent_id.clone()),
                    ..TaskParentContext::default()
                },
                |ctx| async move {
                    ctx.cancelled().await;
                    TaskResult::Cancelled
                },
            )
            .await
            .unwrap();

        let grandchild_id = manager
            .spawn(
                "thread-1",
                "grandchild",
                Some("grandchild"),
                "grandchild task",
                TaskParentContext {
                    task_id: Some(child_id.clone()),
                    ..TaskParentContext::default()
                },
                |ctx| async move {
                    ctx.cancelled().await;
                    TaskResult::Cancelled
                },
            )
            .await
            .unwrap();

        let tool = CancelTaskTool::with_current_task(manager.clone(), parent_id.clone());
        let ctx = make_ctx_with_store_and_task("thread-1", "agent", &store, None);
        let result = tool
            .execute(json!({"target": {"relation": "self"}}), &ctx)
            .await
            .unwrap();

        assert_eq!(result.result.data["status"], "accepted");
        assert_eq!(result.result.data["root_task_id"], parent_id);
        assert_eq!(result.result.data["cancelled_count"], 3);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            manager.get(&child_id).await.unwrap().status,
            super::super::types::TaskStatus::Cancelled
        );
        assert_eq!(
            manager.get(&grandchild_id).await.unwrap().status,
            super::super::types::TaskStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn self_cancel_from_run_context_cascades_run_children() {
        let (manager, store) = make_manager_and_store();

        let child_id = manager
            .spawn(
                "thread-1",
                "child",
                Some("child"),
                "child task",
                TaskParentContext {
                    run_id: Some("run-1".into()),
                    ..TaskParentContext::default()
                },
                |ctx| async move {
                    ctx.cancelled().await;
                    TaskResult::Cancelled
                },
            )
            .await
            .unwrap();

        let grandchild_id = manager
            .spawn(
                "thread-1",
                "grandchild",
                Some("grandchild"),
                "grandchild task",
                TaskParentContext {
                    task_id: Some(child_id.clone()),
                    ..TaskParentContext::default()
                },
                |ctx| async move {
                    ctx.cancelled().await;
                    TaskResult::Cancelled
                },
            )
            .await
            .unwrap();

        let tool = CancelTaskTool::new(manager.clone());
        let cancellation_token = crate::cancellation::CancellationToken::new();
        let ctx = make_ctx_with_store_and_task_and_token(
            "thread-1",
            "agent",
            &store,
            None,
            Some(cancellation_token.clone()),
        );
        let result = tool
            .execute(json!({"target": {"relation": "self"}}), &ctx)
            .await
            .unwrap();

        assert_eq!(result.result.data["status"], "accepted");
        assert_eq!(result.result.data["root_task_id"], "run-1");
        assert_eq!(result.result.data["cancelled_count"], 3);
        assert!(cancellation_token.is_cancelled());

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            manager.get(&child_id).await.unwrap().status,
            super::super::types::TaskStatus::Cancelled
        );
        assert_eq!(
            manager.get(&grandchild_id).await.unwrap().status,
            super::super::types::TaskStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn self_cancel_without_task_context_fails() {
        let (manager, store) = make_manager_and_store();
        let tool = CancelTaskTool::new(manager);
        let ctx = make_ctx_with_store_and_task_and_token("thread-1", "agent", &store, None, None);
        let err = tool
            .execute(json!({"target": {"relation": "self"}}), &ctx)
            .await
            .expect_err("missing current task context should fail");
        assert!(
            err.to_string()
                .contains(CancelTaskError::CurrentTaskUnavailable.to_string().as_str())
        );
    }
}
