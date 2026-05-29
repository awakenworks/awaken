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
use awaken_server_contract::contract::suspension::ToolCallResume;

/// Execution boundary used by mailbox dispatch.
///
/// Mailbox owns delivery, leasing, retry, and recovery. The executor behind
/// this trait owns actual run execution and live-run control. It intentionally
/// does not expose storage so mailbox scheduling stays orthogonal to the main
/// runtime implementation. The optional commit coordinator is exposed only
/// as the durable write boundary for mailbox-authored checkpoints.
#[async_trait]
pub trait RunDispatchExecutor: Send + Sync {
    /// Execute a run request and stream events into the provided sink.
    async fn run(
        &self,
        activation: RunActivation,
        sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError>;

    /// Execute a run with an optional mailbox-provided thread cache.
    async fn run_with_thread_context(
        &self,
        activation: RunActivation,
        sink: Arc<dyn EventSink>,
        thread_ctx: Option<ThreadContextSnapshot>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        let _ = thread_ctx;
        self.run(activation, sink).await
    }

    /// Resolve an activation before dispatch-side execution. Persistent
    /// mailbox paths require a replayable plan and reject live-only plans
    /// before runtime side effects begin.
    async fn resolve_activation(
        &self,
        activation: &RunActivation,
        policy: ResolutionPolicy,
    ) -> Result<ResolvedRunPlan, ResolveError> {
        self.resolve_activation_in_scope(activation, policy, RegistryResolutionScope::Live)
            .await
    }

    /// Resolve an activation with explicit resolver-scope input supplied by
    /// the server persistence layer. This keeps pinned registry manifests out
    /// of `RunActivation` while preserving replayable dispatch semantics.
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

    /// Execute a persistent run with the replayable plan rebuilt from the
    /// activation's pinned registry scope.
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

    /// Cancel an active run by run id or thread id.
    fn cancel(&self, id: &str) -> bool;

    /// Cancel an active run by thread id and wait for it to unregister.
    async fn cancel_and_wait_by_thread(&self, thread_id: &str) -> bool;

    /// Forward one human/tool decision to an active run.
    fn send_decision(&self, id: &str, tool_call_id: String, resume: ToolCallResume) -> bool;

    /// Forward direct input messages to an active run.
    fn send_messages(&self, id: &str, messages: Vec<Message>) -> bool {
        let _ = (id, messages);
        false
    }

    /// Wake an active run so it consumes durable pending messages.
    fn wake_pending_boundary(&self, id: &str) -> bool {
        let _ = id;
        false
    }

    /// Snapshot the live RegistrySet from the underlying runtime. Used by
    /// the mailbox to overlay a pinned `RegistrySet` (ADR-0035 D9) on top
    /// of the live runtime objects (tools / providers / plugins). Returns
    /// `None` when the executor has no live registry available.
    fn live_registry_set(&self) -> Option<awaken_runtime::registry::RegistrySet> {
        None
    }

    /// Durable checkpoint boundary wired into the executor, when available.
    fn commit_coordinator(&self) -> Option<Arc<dyn CommitCoordinator>> {
        None
    }

    /// Whether the executor has a `CommitCoordinator` wired (ADR-0036 D9).
    fn has_commit_coordinator(&self) -> bool {
        self.commit_coordinator().is_some()
    }
}

#[cfg(test)]
fn test_replayable_plan(activation: &RunActivation) -> ResolvedRunPlan {
    use awaken_runtime::{ReplayableScope, ResolutionArtifact, ResolvedRun};
    use awaken_server_contract::contract::versioned_registry::PinnedRegistryManifest;

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
            registry_manifest: PinnedRegistryManifest {
                publication_id: Some("test-publication".to_string()),
                registry_snapshot_version: Some(1),
                entries: Vec::new(),
            },
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
