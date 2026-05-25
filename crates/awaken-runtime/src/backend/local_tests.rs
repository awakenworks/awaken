use super::*;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::event_sink::NullEventSink;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::message::{Message, ToolCall};
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use serde_json::{Value, json};

use crate::backend::{
    BackendControl, BackendDelegatePolicy, BackendDelegateRunRequest, BackendParentContext,
    BackendRunStatus, ExecutionBackendError,
};
#[cfg(feature = "background")]
use crate::extensions::background::{
    BackgroundTaskManager, BackgroundTaskPlugin, TaskParentContext, TaskResult as BgTaskResult,
    TaskStatus,
};
use crate::loop_runner::build_agent_env;
use crate::plugins::{Plugin, PluginDescriptor};
use crate::registry::AgentResolver;
#[cfg(feature = "background")]
use crate::state::StateStore;

struct ScriptedLlm {
    responses: Mutex<Vec<StreamResult>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<StreamResult>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LlmExecutor for ScriptedLlm {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let mut responses = self.responses.lock().unwrap();
        assert!(!responses.is_empty(), "scripted LLM exhausted");
        Ok(responses.remove(0))
    }

    fn name(&self) -> &str {
        "scripted"
    }
}

fn text_response(text: &str) -> StreamResult {
    StreamResult {
        content: vec![ContentBlock::text(text)],
        tool_calls: vec![],
        usage: Some(TokenUsage::default()),
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }
}

fn tool_call_response(text: &str, tool_name: &str, call_id: &str, args: Value) -> StreamResult {
    StreamResult {
        content: vec![ContentBlock::text(text)],
        tool_calls: vec![ToolCall::new(call_id, tool_name, args)],
        usage: Some(TokenUsage::default()),
        stop_reason: Some(StopReason::ToolUse),
        has_incomplete_tool_calls: false,
    }
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("echo", "echo", "Echoes input back")
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolResult::success_with_message("echo", args, "tool result should not win").into())
    }
}

#[cfg(feature = "background")]
struct CustomCancelTool {
    called: Arc<AtomicUsize>,
}

#[cfg(feature = "background")]
#[async_trait]
impl Tool for CustomCancelTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("cancel_task", "cancel_task", "custom cancel task tool")
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        self.called.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::success_with_message("cancel_task", args, "custom cancel handled").into())
    }
}

struct BindingPlugin {
    bind_count: Arc<AtomicUsize>,
}

impl Plugin for BindingPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "binding-plugin",
        }
    }

    fn bind_runtime_context(
        &self,
        _store: &crate::state::StateStore,
        _owner_inbox: Option<&crate::inbox::InboxSender>,
    ) {
        self.bind_count.fetch_add(1, Ordering::SeqCst);
    }
}

struct FixedResolver {
    agent: ResolvedAgent,
    plugins: Vec<Arc<dyn Plugin>>,
}

impl AgentResolver for FixedResolver {
    fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, crate::RuntimeError> {
        let mut agent = self.agent.clone();
        agent.env = build_agent_env(&self.plugins, &agent).expect("build env");
        Ok(agent)
    }
}

struct FailingResolver;

impl AgentResolver for FailingResolver {
    fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, crate::RuntimeError> {
        Err(crate::RuntimeError::ResolveFailed {
            message: "resolver storage unavailable".into(),
        })
    }
}

#[tokio::test]
async fn execute_delegate_preserves_non_missing_resolver_errors() {
    let err = LocalBackend::new()
        .execute_delegate(BackendDelegateRunRequest {
            agent_id: "delegate",
            messages: vec![Message::user("hello")],
            new_messages: vec![Message::user("hello")],
            sink: Arc::new(NullEventSink),
            resolver: &FailingResolver,
            parent: BackendParentContext::default(),
            control: BackendControl::default(),
            policy: BackendDelegatePolicy::default(),
            state_seed: None,
        })
        .await
        .expect_err("resolver infrastructure failure should surface");

    match err {
        ExecutionBackendError::ExecutionFailed(message) => {
            assert!(
                message.contains("resolver storage unavailable"),
                "error should preserve resolver failure: {message}"
            );
        }
        other => panic!("non-missing resolver error must not become AgentNotFound: {other:?}"),
    }
}

#[tokio::test]
async fn execute_delegate_binds_plugin_runtime_context() {
    let bind_count = Arc::new(AtomicUsize::new(0));
    let plugin: Arc<dyn Plugin> = Arc::new(BindingPlugin {
        bind_count: bind_count.clone(),
    });
    let resolver = FixedResolver {
        agent: ResolvedAgent::new(
            "delegate",
            "m",
            "sys",
            Arc::new(ScriptedLlm::new(vec![text_response("delegated response")])),
        ),
        plugins: vec![plugin],
    };

    let result = LocalBackend::new()
        .execute_delegate(BackendDelegateRunRequest {
            agent_id: "delegate",
            messages: vec![Message::user("hello")],
            new_messages: vec![Message::user("hello")],
            sink: Arc::new(NullEventSink),
            resolver: &resolver,
            parent: BackendParentContext {
                parent_run_id: Some("parent-run".into()),
                parent_thread_id: Some("parent-thread".into()),
                parent_tool_call_id: Some("tool-1".into()),
            },
            control: BackendControl::default(),
            policy: BackendDelegatePolicy::default(),
            state_seed: None,
        })
        .await
        .expect("delegate execution should succeed");

    assert!(matches!(result.status, BackendRunStatus::Completed));
    assert_eq!(bind_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn execute_delegate_returns_final_non_tool_message_after_tool_output() {
    let resolver = FixedResolver {
        agent: ResolvedAgent::new(
            "delegate",
            "m",
            "sys",
            Arc::new(ScriptedLlm::new(vec![
                tool_call_response(
                    "checking",
                    "echo",
                    "call-1",
                    json!({"message": "tool result should not win"}),
                ),
                text_response("final child answer"),
            ])),
        )
        .with_tool(Arc::new(EchoTool)),
        plugins: Vec::new(),
    };

    let result = LocalBackend::new()
        .execute_delegate(BackendDelegateRunRequest {
            agent_id: "delegate",
            messages: vec![Message::user("delegate with a tool")],
            new_messages: vec![Message::user("delegate with a tool")],
            sink: Arc::new(NullEventSink),
            resolver: &resolver,
            parent: BackendParentContext {
                parent_run_id: Some("parent-run".into()),
                parent_thread_id: Some("parent-thread".into()),
                parent_tool_call_id: Some("tool-1".into()),
            },
            control: BackendControl::default(),
            policy: BackendDelegatePolicy::default(),
            state_seed: None,
        })
        .await
        .expect("delegate execution should succeed");

    assert!(matches!(result.status, BackendRunStatus::Completed));
    assert_eq!(result.response.as_deref(), Some("final child answer"));
    assert_eq!(result.output.text.as_deref(), Some("final child answer"));
    assert_eq!(result.steps, 2);
}

#[cfg(feature = "background")]
#[tokio::test]
async fn execute_delegate_in_background_task_can_self_cancel_and_cascade() {
    let store = StateStore::new();
    let manager = Arc::new(BackgroundTaskManager::new());
    manager.set_store(store.clone());
    let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::new(manager.clone()));
    let env = crate::phase::ExecutionEnv::from_plugins(&[plugin], &Default::default())
        .expect("background plugin env");
    store
        .register_keys(&env.key_registrations)
        .expect("background keys should register");

    let resolver = FixedResolver {
        agent: ResolvedAgent::new(
            "delegate",
            "m",
            "sys",
            Arc::new(ScriptedLlm::new(vec![
                tool_call_response(
                    "cancel self",
                    "cancel_task",
                    "call-1",
                    json!({"target": {"relation": "self"}}),
                ),
                text_response("should not be reached after cancellation"),
            ])),
        ),
        plugins: Vec::new(),
    };

    let child_task_id = Arc::new(tokio::sync::Mutex::new(None::<String>));
    let child_task_id_seen = child_task_id.clone();
    let resolver = Arc::new(resolver);
    let task_id = manager
        .spawn_agent_with_context(
            "thread-1",
            Some("worker"),
            "worker agent",
            TaskParentContext::default(),
            {
                let manager = manager.clone();
                move |ctx| {
                    let manager = manager.clone();
                    let child_task_id_seen = child_task_id_seen.clone();
                    let resolver = resolver.clone();
                    async move {
                        assert!(
                            crate::extensions::background::current_background_task_context()
                                .is_some(),
                            "background task context should be visible inside spawned task"
                        );
                        let child_id = manager
                            .spawn(
                                "thread-1",
                                "child",
                                Some("leaf"),
                                "child task",
                                TaskParentContext {
                                    task_id: Some(ctx.task_id.clone()),
                                    ..TaskParentContext::default()
                                },
                                |child_ctx| async move {
                                    child_ctx.cancelled().await;
                                    BgTaskResult::Cancelled
                                },
                            )
                            .await
                            .expect("child task should spawn");
                        *child_task_id_seen.lock().await = Some(child_id);

                        let result = LocalBackend::new()
                            .execute_delegate(BackendDelegateRunRequest {
                                agent_id: "delegate",
                                messages: vec![Message::user("cancel yourself")],
                                new_messages: vec![Message::user("cancel yourself")],
                                sink: Arc::new(NullEventSink),
                                resolver: resolver.as_ref(),
                                parent: BackendParentContext {
                                    parent_run_id: Some("parent-run".into()),
                                    parent_thread_id: Some("parent-thread".into()),
                                    parent_tool_call_id: Some("tool-1".into()),
                                },
                                control: BackendControl {
                                    cancellation_token: Some(ctx.cancel_token.clone()),
                                    decision_rx: None,
                                    pending_boundary: None,
                                },
                                policy: BackendDelegatePolicy::default(),
                                state_seed: None,
                            })
                            .await
                            .expect("delegate should run");

                        match result.status {
                            BackendRunStatus::Cancelled => BgTaskResult::Cancelled,
                            other => BgTaskResult::Failed(format!(
                                "expected cancelled delegate status, got {other}; response={:?}; output={:?}",
                                result.response,
                                result.output
                            )),
                        }
                    }
                }
            },
        )
        .await
        .expect("background sub-agent should spawn");

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let child_id = child_task_id
        .lock()
        .await
        .clone()
        .expect("child task id should be recorded");
    let task_summary = manager
        .get(&task_id)
        .await
        .expect("root task should still be queryable");
    let child_summary = manager
        .get(&child_id)
        .await
        .expect("child task should still be queryable");

    assert_eq!(
        task_summary.status,
        TaskStatus::Cancelled,
        "task={task_summary:?} child={child_summary:?}"
    );
    assert_eq!(
        child_summary.status,
        TaskStatus::Cancelled,
        "task={task_summary:?} child={child_summary:?}"
    );
}

#[cfg(feature = "background")]
#[tokio::test]
async fn execute_delegate_preserves_existing_cancel_task_tool_in_background_context() {
    let store = StateStore::new();
    let manager = Arc::new(BackgroundTaskManager::new());
    manager.set_store(store.clone());
    let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::new(manager.clone()));
    let env = crate::phase::ExecutionEnv::from_plugins(&[plugin], &Default::default())
        .expect("background plugin env");
    store
        .register_keys(&env.key_registrations)
        .expect("background keys should register");

    let cancel_calls = Arc::new(AtomicUsize::new(0));
    let resolver = Arc::new(FixedResolver {
        agent: ResolvedAgent::new(
            "delegate",
            "m",
            "sys",
            Arc::new(ScriptedLlm::new(vec![
                tool_call_response(
                    "use custom cancel",
                    "cancel_task",
                    "call-1",
                    json!({"target": {"relation": "self"}}),
                ),
                text_response("custom tool won"),
            ])),
        )
        .with_tool(Arc::new(CustomCancelTool {
            called: cancel_calls.clone(),
        })),
        plugins: Vec::new(),
    });

    let task_id = manager
        .spawn_agent_with_context(
            "thread-1",
            Some("worker"),
            "worker agent",
            TaskParentContext::default(),
            move |ctx| {
                let resolver = resolver.clone();
                async move {
                    let result = LocalBackend::new()
                        .execute_delegate(BackendDelegateRunRequest {
                            agent_id: "delegate",
                            messages: vec![Message::user("use custom cancel")],
                            new_messages: vec![Message::user("use custom cancel")],
                            sink: Arc::new(NullEventSink),
                            resolver: resolver.as_ref(),
                            parent: BackendParentContext {
                                parent_run_id: Some("parent-run".into()),
                                parent_thread_id: Some("parent-thread".into()),
                                parent_tool_call_id: Some("tool-1".into()),
                            },
                            control: BackendControl {
                                cancellation_token: Some(ctx.cancel_token),
                                decision_rx: None,
                                pending_boundary: None,
                            },
                            policy: BackendDelegatePolicy::default(),
                            state_seed: None,
                        })
                        .await
                        .expect("delegate should run");

                    match result.status {
                        BackendRunStatus::Completed
                            if result.response.as_deref() == Some("custom tool won") =>
                        {
                            BgTaskResult::Success(json!({
                                "response": result.response,
                                "steps": result.steps,
                            }))
                        }
                        other => BgTaskResult::Failed(format!(
                            "expected completed delegate status, got {other}; response={:?}; output={:?}",
                            result.response,
                            result.output
                        )),
                    }
                }
            },
        )
        .await
        .expect("background sub-agent should spawn");

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert_eq!(cancel_calls.load(Ordering::SeqCst), 1);
    let summary = manager
        .get(&task_id)
        .await
        .expect("root task should be queryable");
    assert_eq!(summary.status, TaskStatus::Completed, "task={summary:?}");
}
