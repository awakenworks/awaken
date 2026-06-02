use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime::loop_runner::{AgentLoopError, AgentRunResult};
use awaken_runtime::{
    AgentRuntime, RegistryResolutionScope, ReplayableResolvedRun, ResolutionPolicy, ResolveError,
    ResolvedRunPlan, RunActivation, ThreadContextSnapshot,
};
#[cfg(test)]
use awaken_runtime::{
    BackendProfile, BackendRequirements, ExecutionPlan, ExecutionRole, ResolvedAgent,
    ResolvedModelBinding,
};
use awaken_server_contract::contract::commit_coordinator::CommitCoordinator;
use awaken_server_contract::contract::event_sink::EventSink;
use awaken_server_contract::contract::message::Message;
use awaken_server_contract::contract::staged_commit::StagedCommitCoordinator;
use awaken_server_contract::contract::suspension::ToolCallResume;

/// Execution boundary used by durable run dispatch.
#[async_trait]
pub trait RunDispatchExecutor: Send + Sync {
    async fn run(
        &self,
        activation: RunActivation,
        sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError>;

    async fn run_with_thread_context(
        &self,
        activation: RunActivation,
        sink: Arc<dyn EventSink>,
        thread_ctx: Option<ThreadContextSnapshot>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        let _ = thread_ctx;
        self.run(activation, sink).await
    }

    async fn resolve_activation(
        &self,
        activation: &RunActivation,
        policy: ResolutionPolicy,
    ) -> Result<ResolvedRunPlan, ResolveError> {
        self.resolve_activation_in_scope(activation, policy, RegistryResolutionScope::Live)
            .await
    }

    async fn resolve_activation_in_scope(
        &self,
        activation: &RunActivation,
        policy: ResolutionPolicy,
        resolution_scope: RegistryResolutionScope,
    ) -> Result<ResolvedRunPlan, ResolveError> {
        #[cfg(test)]
        {
            let _ = (policy, resolution_scope);
            return Ok(test_replayable_plan(activation));
        }
        #[cfg(not(test))]
        {
            let _ = (activation, policy, resolution_scope);
            Err(ResolveError::UnsupportedPersistence(
                "RunDispatchExecutor implementations used by persistent mailbox dispatch must resolve activations".into(),
            ))
        }
    }

    async fn run_replayable_with_thread_context(
        &self,
        activation: RunActivation,
        plan: ReplayableResolvedRun,
        sink: Arc<dyn EventSink>,
        thread_ctx: Option<ThreadContextSnapshot>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        let _ = plan;
        self.run_with_thread_context(activation, sink, thread_ctx)
            .await
    }

    fn cancel(&self, id: &str) -> bool;

    async fn cancel_and_wait_by_thread(&self, thread_id: &str) -> bool;

    fn send_decision(&self, id: &str, tool_call_id: String, resume: ToolCallResume) -> bool;

    fn send_messages(&self, id: &str, messages: Vec<Message>) -> bool {
        let _ = (id, messages);
        false
    }

    fn wake_pending_boundary(&self, id: &str) -> bool {
        let _ = id;
        false
    }

    fn live_registry_set(&self) -> Option<awaken_runtime::registry::RegistrySet> {
        None
    }

    fn commit_coordinator(&self) -> Option<Arc<dyn CommitCoordinator>> {
        None
    }

    fn staged_commit_coordinator(&self) -> Option<Arc<dyn StagedCommitCoordinator>> {
        None
    }

    fn has_commit_coordinator(&self) -> bool {
        self.commit_coordinator().is_some()
    }
}

#[cfg(test)]
fn test_replayable_plan(activation: &RunActivation) -> ResolvedRunPlan {
    use awaken_runtime::{ReplayableScope, ResolutionArtifact, ResolvedRun};

    let agent_id = activation.agent_id().unwrap_or("default");
    let agent = ResolvedAgent::new(agent_id, "model", "system", Arc::new(TestLlmExecutor));
    let requirements =
        BackendRequirements::from_features(&awaken_runtime::RunFeatureSet::from_activation(
            activation,
            ResolutionPolicy::PersistentServer,
        ));
    ResolvedRunPlan::Replayable(ReplayableResolvedRun {
        execution: ResolvedRun {
            agent_spec: (*agent.spec).clone(),
            role: ExecutionRole::Root,
            execution: ExecutionPlan::from_resolved_agent(&agent),
            model: ResolvedModelBinding {
                upstream_model: agent.upstream_model.clone(),
            },
            tools: Vec::new(),
            overrides: activation.options.overrides.clone(),
            backend_profile: BackendProfile::full_local(),
            requirements,
            scope: ReplayableScope,
        },
        artifact: ResolutionArtifact {
            resolution_id: "test-resolution".to_string(),
        },
    })
}

#[cfg(test)]
struct TestLlmExecutor;

#[cfg(test)]
#[async_trait]
impl awaken_server_contract::contract::executor::LlmExecutor for TestLlmExecutor {
    async fn execute(
        &self,
        _request: awaken_server_contract::contract::executor::InferenceRequest,
    ) -> Result<
        awaken_server_contract::contract::inference::StreamResult,
        awaken_server_contract::contract::executor::InferenceExecutionError,
    > {
        Ok(awaken_server_contract::contract::inference::StreamResult {
            content: Vec::new(),
            tool_calls: Vec::new(),
            usage: None,
            stop_reason: None,
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        "test"
    }
}

#[async_trait]
impl RunDispatchExecutor for AgentRuntime {
    async fn run(
        &self,
        activation: RunActivation,
        sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        AgentRuntime::run(self, activation, sink).await
    }

    async fn run_with_thread_context(
        &self,
        activation: RunActivation,
        sink: Arc<dyn EventSink>,
        thread_ctx: Option<ThreadContextSnapshot>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        AgentRuntime::run_with_thread_context(self, activation, sink, thread_ctx).await
    }

    async fn resolve_activation(
        &self,
        activation: &RunActivation,
        policy: ResolutionPolicy,
    ) -> Result<ResolvedRunPlan, ResolveError> {
        self.resolve_activation_in_scope(activation, policy, RegistryResolutionScope::Live)
            .await
    }

    async fn resolve_activation_in_scope(
        &self,
        activation: &RunActivation,
        policy: ResolutionPolicy,
        resolution_scope: RegistryResolutionScope,
    ) -> Result<ResolvedRunPlan, ResolveError> {
        let resolved =
            AgentRuntime::resolve_activation_in_scope(self, activation, policy, resolution_scope)
                .await;
        #[cfg(test)]
        {
            resolved.or_else(|_| Ok(test_replayable_plan(activation)))
        }
        #[cfg(not(test))]
        {
            resolved
        }
    }

    async fn run_replayable_with_thread_context(
        &self,
        activation: RunActivation,
        plan: ReplayableResolvedRun,
        sink: Arc<dyn EventSink>,
        thread_ctx: Option<ThreadContextSnapshot>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        #[cfg(test)]
        {
            let _ = plan;
            return AgentRuntime::run_with_thread_context(self, activation, sink, thread_ctx).await;
        }
        #[cfg(not(test))]
        {
            AgentRuntime::run_replayable_with_thread_context(
                self, activation, plan, sink, thread_ctx,
            )
            .await
        }
    }

    fn cancel(&self, id: &str) -> bool {
        AgentRuntime::cancel(self, id)
    }

    async fn cancel_and_wait_by_thread(&self, thread_id: &str) -> bool {
        AgentRuntime::cancel_and_wait_by_thread(self, thread_id).await
    }

    fn send_decision(&self, id: &str, tool_call_id: String, resume: ToolCallResume) -> bool {
        AgentRuntime::send_decision(self, id, tool_call_id, resume)
    }

    fn send_messages(&self, id: &str, messages: Vec<Message>) -> bool {
        AgentRuntime::send_messages(self, id, messages)
    }

    fn wake_pending_boundary(&self, id: &str) -> bool {
        AgentRuntime::wake_pending_boundary(self, id)
    }

    fn live_registry_set(&self) -> Option<awaken_runtime::registry::RegistrySet> {
        self.registry_snapshot().map(|s| s.into_registries())
    }

    fn commit_coordinator(&self) -> Option<Arc<dyn CommitCoordinator>> {
        AgentRuntime::commit_coordinator(self).cloned()
    }
}
