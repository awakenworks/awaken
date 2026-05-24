//! In-process execution backend backed by the standard loop runner.

use async_trait::async_trait;
use awaken_contract::contract::identity::{RunIdentity, RunOrigin};
use awaken_contract::contract::lifecycle::TerminationReason;

use crate::loop_runner::{AgentLoopParams, CommitWiring, prepare_resume, run_agent_loop};
use crate::registry::ResolvedAgent;
use crate::state::StateStore;

use super::{
    BackendDelegateContinuation, BackendDelegatePersistence, BackendDelegateRunRequest,
    BackendRootRunRequest, BackendRunOutput, BackendRunResult, BackendRunStatus, ExecutionBackend,
    ExecutionBackendError,
};

#[cfg(feature = "background")]
struct DelegateResolver<'a> {
    inner: &'a dyn crate::registry::AgentResolver,
    agent_id: &'a str,
    resolved: ResolvedAgent,
    context: Option<crate::extensions::background::BackgroundTaskExecutionContext>,
}

#[cfg(not(feature = "background"))]
struct DelegateResolver<'a> {
    inner: &'a dyn crate::registry::AgentResolver,
    agent_id: &'a str,
    resolved: ResolvedAgent,
}

#[cfg(feature = "background")]
impl<'a> DelegateResolver<'a> {
    fn new(
        inner: &'a dyn crate::registry::AgentResolver,
        agent_id: &'a str,
        resolved: ResolvedAgent,
        context: Option<crate::extensions::background::BackgroundTaskExecutionContext>,
    ) -> Self {
        Self {
            inner,
            agent_id,
            resolved,
            context,
        }
    }

    fn with_background_control(&self, mut resolved: ResolvedAgent) -> ResolvedAgent {
        if let Some(context) = &self.context {
            LocalBackend::ensure_background_cancel_tool(&mut resolved, context);
        }
        resolved
    }
}

#[cfg(feature = "background")]
impl crate::registry::AgentResolver for DelegateResolver<'_> {
    fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, crate::RuntimeError> {
        if agent_id == self.agent_id {
            return Ok(self.resolved.clone());
        }
        self.inner
            .resolve(agent_id)
            .map(|resolved| self.with_background_control(resolved))
    }

    fn resolve_execution(
        &self,
        agent_id: &str,
    ) -> Result<crate::resolution::ExecutionPlan, crate::RuntimeError> {
        if agent_id == self.agent_id {
            return Ok(crate::resolution::ExecutionPlan::from_resolved_agent(
                &self.resolved,
            ));
        }
        match self.inner.resolve_execution(agent_id)? {
            crate::resolution::ExecutionPlan::Local(agent) => {
                Ok(crate::resolution::ExecutionPlan::Local(Box::new(
                    self.with_background_control(*agent),
                )))
            }
            other => Ok(other),
        }
    }

    fn agent_ids(&self) -> Vec<String> {
        self.inner.agent_ids()
    }
}

#[cfg(not(feature = "background"))]
impl<'a> DelegateResolver<'a> {
    fn new(
        inner: &'a dyn crate::registry::AgentResolver,
        agent_id: &'a str,
        resolved: ResolvedAgent,
    ) -> Self {
        Self {
            inner,
            agent_id,
            resolved,
        }
    }
}

#[cfg(not(feature = "background"))]
impl crate::registry::AgentResolver for DelegateResolver<'_> {
    fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, crate::RuntimeError> {
        if agent_id == self.agent_id {
            return Ok(self.resolved.clone());
        }
        self.inner.resolve(agent_id)
    }

    fn resolve_execution(
        &self,
        agent_id: &str,
    ) -> Result<crate::resolution::ExecutionPlan, crate::RuntimeError> {
        if agent_id == self.agent_id {
            return Ok(crate::resolution::ExecutionPlan::from_resolved_agent(
                &self.resolved,
            ));
        }
        self.inner.resolve_execution(agent_id)
    }

    fn agent_ids(&self) -> Vec<String> {
        self.inner.agent_ids()
    }
}
/// Local runtime backend for executing the standard loop in-process.
pub struct LocalBackend;

impl LocalBackend {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    pub(crate) async fn execute_resolved(
        resolved: &ResolvedAgent,
        request: BackendDelegateRunRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError> {
        Self::new()
            .execute_resolved_delegate(resolved, request)
            .await
    }
}

impl Default for LocalBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutionBackend for LocalBackend {
    fn capabilities(&self) -> crate::resolution::BackendProfile {
        crate::resolution::BackendProfile::full_local()
    }

    async fn execute_delegate(
        &self,
        request: BackendDelegateRunRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError> {
        Self::execute_delegate(self, request).await
    }

    async fn execute_root(
        &self,
        request: BackendRootRunRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError> {
        self.execute_root_with_thread_context(request, None).await
    }
}

impl LocalBackend {
    pub(crate) async fn execute_root_with_thread_context(
        &self,
        request: BackendRootRunRequest<'_>,
        thread_ctx: Option<crate::ThreadContextSnapshot>,
    ) -> Result<BackendRunResult, ExecutionBackendError> {
        let phase_runtime = request
            .local
            .as_ref()
            .map(|context| context.phase_runtime)
            .ok_or_else(|| {
                ExecutionBackendError::ExecutionFailed(
                    "local root execution requires a phase runtime context".into(),
                )
            })?;
        let run_identity = request.run_identity.clone();
        let run_id = run_identity.run_id.clone();
        if !request.decisions.is_empty() {
            prepare_resume(phase_runtime.store(), request.decisions, None)
                .map_err(crate::loop_runner::AgentLoopError::PhaseError)
                .map_err(ExecutionBackendError::Loop)?;
        }

        let result = crate::loop_runner::run_agent_loop_with_thread_context(
            AgentLoopParams {
                resolver: request.resolver,
                agent_id: request.agent_id,
                runtime: phase_runtime,
                sink: request.sink,
                checkpoint_store: request.checkpoint_store,
                commit: request.commit,
                messages: request.messages,
                run_identity,
                cancellation_token: request.control.cancellation_token,
                decision_rx: request.control.decision_rx,
                overrides: request.overrides,
                frontend_tools: request.frontend_tools,
                inbox: request.inbox,
                is_continuation: request.is_continuation,
                initial_state_seed: None,
            },
            thread_ctx,
        )
        .await
        .map_err(ExecutionBackendError::Loop)?;

        let response = if result.response.is_empty() {
            None
        } else {
            Some(result.response)
        };
        Ok(BackendRunResult {
            agent_id: request.agent_id.to_string(),
            status: map_termination(&result.termination),
            termination: result.termination,
            status_reason: None,
            output: BackendRunOutput::from_text(response.clone()),
            response,
            steps: result.steps,
            run_id: Some(run_id),
            inbox: None,
            state: None,
        })
    }

    pub async fn execute_delegate(
        &self,
        request: BackendDelegateRunRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError> {
        let resolved = crate::registry::AgentResolver::resolve(request.resolver, request.agent_id)
            .map_err(|error| map_resolve_error(request.agent_id, error))?;

        self.execute_resolved_delegate(&resolved, request).await
    }

    pub(crate) async fn execute_resolved_delegate(
        &self,
        resolved: &ResolvedAgent,
        request: BackendDelegateRunRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError> {
        match (request.policy.persistence, request.policy.continuation) {
            (BackendDelegatePersistence::Ephemeral, BackendDelegateContinuation::Disabled) => {}
        }
        #[cfg(feature = "background")]
        let background_context = crate::extensions::background::current_background_task_context();
        #[cfg(feature = "background")]
        let mut initial_resolved = resolved.clone();
        #[cfg(not(feature = "background"))]
        let initial_resolved = resolved.clone();
        #[cfg(feature = "background")]
        if let Some(context) = &background_context {
            Self::ensure_background_cancel_tool(&mut initial_resolved, context);
        }

        let store = crate::state::StateStore::new();
        store
            .install_plugin(crate::loop_runner::LoopStatePlugin)
            .map_err(|error| ExecutionBackendError::ExecutionFailed(error.to_string()))?;
        store
            .install_plugin(crate::loop_runner::LoopActionHandlersPlugin)
            .map_err(|error| ExecutionBackendError::ExecutionFailed(error.to_string()))?;

        let phase_runtime = crate::phase::PhaseRuntime::new(store.clone())
            .map_err(|error| ExecutionBackendError::ExecutionFailed(error.to_string()))?;

        let (owner_inbox, inbox_receiver) = {
            let (sender, receiver) = crate::inbox::inbox_channel();
            (Some(sender), receiver)
        };

        Self::bind_local_execution_env(&store, &initial_resolved, owner_inbox.as_ref())
            .map_err(|error| ExecutionBackendError::ExecutionFailed(error.to_string()))?;

        #[cfg(feature = "background")]
        let bg_manager = if initial_resolved
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

        #[cfg(feature = "background")]
        let background_cancel_managers = {
            let mut managers =
                crate::extensions::background::managers_for_resolved_agent(&initial_resolved);
            if let Some(manager) = &bg_manager {
                managers.push(manager.clone());
            }
            crate::extensions::background::dedup_managers(managers)
        };

        #[cfg(feature = "background")]
        let delegate_resolver = DelegateResolver::new(
            request.resolver,
            request.agent_id,
            initial_resolved.clone(),
            background_context.clone(),
        );
        #[cfg(not(feature = "background"))]
        let delegate_resolver =
            DelegateResolver::new(request.resolver, request.agent_id, initial_resolved.clone());
        let child_resolver: &dyn crate::registry::AgentResolver = &delegate_resolver;

        let sub_run_id = uuid::Uuid::now_v7().to_string();
        let mut run_identity = RunIdentity::new(
            sub_run_id.clone(),
            request.parent.parent_thread_id.clone(),
            sub_run_id.clone(),
            request.parent.parent_run_id.clone(),
            request.agent_id.to_string(),
            RunOrigin::Subagent,
        );
        if let Some(parent_tool_call_id) = request.parent.parent_tool_call_id.clone() {
            run_identity = run_identity.with_parent_tool_call_id(parent_tool_call_id);
        }
        #[cfg(feature = "background")]
        let _background_cancel_guard = crate::extensions::background::spawn_run_cancellation_guard(
            sub_run_id.clone(),
            request.control.cancellation_token.clone(),
            background_cancel_managers,
        );

        let result = run_agent_loop(AgentLoopParams {
            resolver: child_resolver,
            agent_id: request.agent_id,
            runtime: &phase_runtime,
            sink: request.sink,
            checkpoint_store: None,
            commit: CommitWiring::default(),
            messages: request.messages,
            run_identity,
            cancellation_token: request.control.cancellation_token,
            decision_rx: request.control.decision_rx,
            overrides: None,
            frontend_tools: Vec::new(),
            inbox: Some(inbox_receiver),
            is_continuation: false,
            initial_state_seed: request.state_seed,
        })
        .await
        .map_err(ExecutionBackendError::Loop)?;

        let final_state = store
            .export_persisted()
            .map_err(|error| ExecutionBackendError::ExecutionFailed(error.to_string()))?;

        let response = if result.response.is_empty() {
            None
        } else {
            Some(result.response)
        };
        Ok(BackendRunResult {
            agent_id: request.agent_id.to_string(),
            status: map_termination(&result.termination),
            termination: result.termination,
            status_reason: None,
            output: BackendRunOutput::from_text(response.clone()),
            response,
            steps: result.steps,
            run_id: Some(sub_run_id),
            inbox: owner_inbox,
            state: Some(final_state),
        })
    }

    #[cfg(feature = "background")]
    fn ensure_background_cancel_tool(
        resolved: &mut ResolvedAgent,
        context: &crate::extensions::background::BackgroundTaskExecutionContext,
    ) {
        if resolved
            .tools
            .contains_key(crate::extensions::background::CANCEL_TASK_TOOL_ID)
        {
            return;
        }

        let tool: std::sync::Arc<dyn awaken_contract::contract::tool::Tool> = std::sync::Arc::new(
            crate::extensions::background::CancelTaskTool::with_current_task(
                context.manager.clone(),
                context.task_id.clone(),
            ),
        );
        resolved.tools.insert(
            crate::extensions::background::CANCEL_TASK_TOOL_ID.into(),
            tool.clone(),
        );
        resolved.env.tools.insert(
            crate::extensions::background::CANCEL_TASK_TOOL_ID.into(),
            tool,
        );
    }

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
        TerminationReason::Suspended => BackendRunStatus::Suspended(None),
        TerminationReason::Error(message) => BackendRunStatus::Failed(message.clone()),
    }
}

fn map_resolve_error(agent_id: &str, error: crate::RuntimeError) -> ExecutionBackendError {
    match error {
        crate::RuntimeError::AgentNotFound { agent_id } => {
            ExecutionBackendError::AgentNotFound(agent_id)
        }
        other => ExecutionBackendError::ExecutionFailed(format!(
            "failed to resolve agent '{agent_id}': {other}"
        )),
    }
}

#[cfg(test)]
#[path = "local_tests.rs"]
mod tests;
