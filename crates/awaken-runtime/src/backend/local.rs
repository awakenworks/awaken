//! In-process execution backend backed by the standard loop runner.

use async_trait::async_trait;
use awaken_contract::contract::identity::{RunIdentity, RunOrigin};
use awaken_contract::contract::lifecycle::TerminationReason;

use crate::loop_runner::{AgentLoopParams, prepare_resume, run_agent_loop};
use crate::registry::ResolvedAgent;
use crate::state::StateStore;

use super::{
    BackendCapabilities, BackendRunRequest, BackendRunResult, BackendRunStatus, ExecutionBackend,
    ExecutionBackendError,
};

/// Local runtime backend for executing the standard loop in-process.
pub struct LocalBackend;

impl LocalBackend {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for LocalBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutionBackend for LocalBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::full()
    }

    async fn execute(
        &self,
        request: BackendRunRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError> {
        let phase_runtime = if let Some(runtime) = request.phase_runtime {
            runtime
        } else {
            return self.execute_delegate(request).await;
        };

        let run_identity = request.run_identity.clone().ok_or_else(|| {
            ExecutionBackendError::ExecutionFailed(
                "local root execution requires run identity".into(),
            )
        })?;
        let run_id = run_identity.run_id.clone();
        if !request.decisions.is_empty() {
            prepare_resume(phase_runtime.store(), request.decisions, None)
                .map_err(crate::loop_runner::AgentLoopError::PhaseError)
                .map_err(ExecutionBackendError::Loop)?;
        }

        let result = run_agent_loop(AgentLoopParams {
            resolver: request.resolver,
            agent_id: request.agent_id,
            runtime: phase_runtime,
            sink: request.sink,
            checkpoint_store: request.checkpoint_store,
            messages: request.messages,
            run_identity,
            cancellation_token: request.control.cancellation_token,
            decision_rx: request.control.decision_rx,
            overrides: request.overrides,
            frontend_tools: request.frontend_tools,
            inbox: request.inbox,
            is_continuation: request.is_continuation,
        })
        .await
        .map_err(ExecutionBackendError::Loop)?;

        Ok(BackendRunResult {
            agent_id: request.agent_id.to_string(),
            status: map_termination(&result.termination),
            termination: result.termination,
            response: if result.response.is_empty() {
                None
            } else {
                Some(result.response)
            },
            steps: result.steps,
            run_id: Some(run_id),
            inbox: None,
        })
    }
}

impl LocalBackend {
    pub(crate) fn bind_local_execution_env(
        store: &StateStore,
        resolved: &ResolvedAgent,
        owner_inbox: Option<&crate::inbox::InboxSender>,
    ) -> Result<(), awaken_contract::StateError> {
        if !resolved.env.key_registrations.is_empty() {
            store.register_keys(&resolved.env.key_registrations)?;
        }
        for plugin in &resolved.env.plugins {
            plugin.bind_runtime_context(store, owner_inbox);
        }
        Ok(())
    }

    async fn execute_delegate(
        &self,
        request: BackendRunRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError> {
        let resolved = request
            .resolver
            .resolve(request.agent_id)
            .map_err(|error| {
                ExecutionBackendError::AgentNotFound(format!(
                    "failed to resolve agent '{}': {error}",
                    request.agent_id
                ))
            })?;

        let store = crate::state::StateStore::new();
        store
            .install_plugin(crate::loop_runner::LoopStatePlugin)
            .map_err(|error| ExecutionBackendError::ExecutionFailed(error.to_string()))?;

        let phase_runtime = crate::phase::PhaseRuntime::new(store.clone())
            .map_err(|error| ExecutionBackendError::ExecutionFailed(error.to_string()))?;
        if !request.decisions.is_empty() {
            prepare_resume(phase_runtime.store(), request.decisions, None)
                .map_err(crate::loop_runner::AgentLoopError::PhaseError)
                .map_err(ExecutionBackendError::Loop)?;
        }

        let (owner_inbox, inbox_receiver) = match request.inbox {
            Some(receiver) => (None, receiver),
            None => {
                let (sender, receiver) = crate::inbox::inbox_channel();
                (Some(sender), receiver)
            }
        };

        Self::bind_local_execution_env(&store, &resolved, owner_inbox.as_ref())
            .map_err(|error| ExecutionBackendError::ExecutionFailed(error.to_string()))?;

        #[cfg(feature = "background")]
        let bg_manager = if resolved
            .env
            .plugins
            .iter()
            .any(|plugin| plugin.descriptor().name == "background_tasks")
        {
            None
        } else {
            let manager = crate::extensions::background::BackgroundTaskManager::new();
            let manager = std::sync::Arc::new(manager);
            manager.set_store(store.clone());
            Some(manager)
        };

        #[cfg(feature = "background")]
        if let Some(manager) = &bg_manager {
            if let Some(sender) = owner_inbox.clone() {
                manager.set_owner_inbox(sender);
            }
            store
                .install_plugin(crate::extensions::background::BackgroundTaskPlugin::new(
                    manager.clone(),
                ))
                .map_err(|error| ExecutionBackendError::ExecutionFailed(error.to_string()))?;
        }

        let sub_run_id = uuid::Uuid::now_v7().to_string();
        let mut run_identity = RunIdentity::new(
            sub_run_id.clone(),
            request
                .parent
                .as_ref()
                .and_then(|parent| parent.parent_thread_id.clone()),
            sub_run_id.clone(),
            request
                .parent
                .as_ref()
                .and_then(|parent| parent.parent_run_id.clone()),
            request.agent_id.to_string(),
            RunOrigin::Subagent,
        );
        if let Some(parent_tool_call_id) = request
            .parent
            .as_ref()
            .and_then(|parent| parent.parent_tool_call_id.clone())
        {
            run_identity = run_identity.with_parent_tool_call_id(parent_tool_call_id);
        }

        let result = run_agent_loop(AgentLoopParams {
            resolver: request.resolver,
            agent_id: request.agent_id,
            runtime: &phase_runtime,
            sink: request.sink,
            checkpoint_store: None,
            messages: request.messages,
            run_identity,
            cancellation_token: request.control.cancellation_token,
            decision_rx: request.control.decision_rx,
            overrides: request.overrides,
            frontend_tools: request.frontend_tools,
            inbox: Some(inbox_receiver),
            is_continuation: false,
        })
        .await
        .map_err(ExecutionBackendError::Loop)?;

        Ok(BackendRunResult {
            agent_id: request.agent_id.to_string(),
            status: map_termination(&result.termination),
            termination: result.termination,
            response: if result.response.is_empty() {
                None
            } else {
                Some(result.response)
            },
            steps: result.steps,
            run_id: Some(sub_run_id),
            inbox: owner_inbox,
        })
    }
}

fn map_termination(termination: &TerminationReason) -> BackendRunStatus {
    match termination {
        TerminationReason::NaturalEnd | TerminationReason::BehaviorRequested => {
            BackendRunStatus::Completed
        }
        TerminationReason::Cancelled => BackendRunStatus::Cancelled,
        TerminationReason::Stopped(reason) => {
            BackendRunStatus::Failed(format!("stopped: {reason:?}"))
        }
        TerminationReason::Blocked(message) => {
            BackendRunStatus::Failed(format!("blocked: {message}"))
        }
        TerminationReason::Suspended => BackendRunStatus::Failed("suspended".into()),
        TerminationReason::Error(message) => BackendRunStatus::Failed(message.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use awaken_contract::contract::content::ContentBlock;
    use awaken_contract::contract::event_sink::NullEventSink;
    use awaken_contract::contract::executor::{
        InferenceExecutionError, InferenceRequest, LlmExecutor,
    };
    use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    use awaken_contract::contract::message::Message;

    use crate::backend::{BackendControl, BackendParentContext};
    use crate::loop_runner::build_agent_env;
    use crate::plugins::{Plugin, PluginDescriptor};
    use crate::registry::{AgentResolver, ExecutionResolver, ResolvedExecution};

    struct ScriptedLlm;

    #[async_trait]
    impl LlmExecutor for ScriptedLlm {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            Ok(StreamResult {
                content: vec![ContentBlock::text("delegated response")],
                tool_calls: vec![],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "scripted"
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

    impl ExecutionResolver for FixedResolver {
        fn resolve_execution(
            &self,
            agent_id: &str,
        ) -> Result<ResolvedExecution, crate::RuntimeError> {
            self.resolve(agent_id).map(ResolvedExecution::Local)
        }
    }

    #[tokio::test]
    async fn execute_delegate_binds_plugin_runtime_context() {
        let bind_count = Arc::new(AtomicUsize::new(0));
        let plugin: Arc<dyn Plugin> = Arc::new(BindingPlugin {
            bind_count: bind_count.clone(),
        });
        let resolver = FixedResolver {
            agent: ResolvedAgent::new("delegate", "m", "sys", Arc::new(ScriptedLlm)),
            plugins: vec![plugin],
        };

        let result = LocalBackend::new()
            .execute(BackendRunRequest {
                agent_id: "delegate",
                messages: vec![Message::user("hello")],
                sink: Arc::new(NullEventSink),
                resolver: &resolver,
                run_identity: None,
                parent: Some(BackendParentContext {
                    parent_run_id: Some("parent-run".into()),
                    parent_thread_id: Some("parent-thread".into()),
                    parent_tool_call_id: Some("tool-1".into()),
                }),
                phase_runtime: None,
                checkpoint_store: None,
                control: BackendControl::default(),
                decisions: Vec::new(),
                overrides: None,
                frontend_tools: Vec::new(),
                inbox: None,
                is_continuation: false,
            })
            .await
            .expect("delegate execution should succeed");

        assert!(matches!(result.status, BackendRunStatus::Completed));
        assert_eq!(bind_count.load(Ordering::SeqCst), 1);
    }
}
