// @formal surface requirement=model.routing.exact_binding
// @formal reason: Installers of the default executor, per-thread model routing, remote attempts, and the Judge;
// @formal reason: classified against the routing product boundary; this link is not a formal-proof claim.
//! AGENT capability of [`SharedHost`]: the default inference executor and
//! model, per-thread model routing, the remote attempt adapter, the Outcome
//! Judge, and the immutable Agent publications the Host executes.

use crate::host::*;

/// Inference and Agent-publication wiring. Consumer: Session context
/// assembly, auxiliary sub-agents (judge, compactor, memory), delegation, and
/// the adapters' model projections.
pub(crate) struct AgentExecution {
    /// The host DEFAULT executor: used by auxiliary sub-agents (judge, compactor,
    /// memory) and by an explicitly local deployment with no
    /// [`InferenceExecutorMaterializer`]. Once a materializer is installed, a rejected
    /// publication pin fails closed instead of falling back to this executor.
    ///
    /// [`InferenceExecutorMaterializer`]: awaken_runtime_contract::inference::InferenceExecutorMaterializer
    pub(crate) llm: Arc<dyn LlmExecutor>,
    pub(crate) model_ref: String,
    /// Per-thread model→executor routing (R1/R2). See [`crate::inference_routing`].
    pub(crate) inference_routing: crate::inference_routing::InferenceRouting,
    /// Remote attempt adapter injected by the process startup. The neutral host
    /// owns only the `RunAttemptExecutor` contract and never names the A2A protocol.
    pub(crate) remote_attempt_executor:
        Option<Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor>>,
    pub(crate) remote_credential_realization:
        awaken_runtime_contract::CredentialRealizationCapabilities,
    pub(crate) judge_snapshot: Option<ExecutableAgentSnapshot>,
    /// Runtime-only view of immutable Agent publications.
    pub(crate) agent_publications:
        Option<Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>>,
}

impl AgentExecution {
    /// The default executor/model with routing over the Host's Session slots;
    /// no remote adapter, Judge override, or publication source yet.
    pub(in crate::host) fn new(
        llm: Arc<dyn LlmExecutor>,
        model_ref: String,
        session_slots: crate::session_slot::SessionRuntimeSlots,
    ) -> Self {
        Self {
            llm,
            model_ref,
            inference_routing: crate::inference_routing::InferenceRouting::new(session_slots),
            remote_attempt_executor: None,
            remote_credential_realization: Default::default(),
            judge_snapshot: None,
            agent_publications: None,
        }
    }
}

impl SharedHost {
    /// Install the adapter that drives snapshot-selected remote attempts. The
    /// host depends only on the neutral attempt port; A2A construction remains a
    /// process-startup responsibility.
    pub fn with_remote_attempt_executor(mut self, installation: RemoteAttemptInstallation) -> Self {
        self.agent.remote_attempt_executor = Some(installation.executor);
        self.agent.remote_credential_realization = installation.credential_realization;
        self
    }

    /// Supply immutable executable publications without mounting the authoring
    /// plane. Intended for embedded process startups and scenario fixtures.
    pub fn with_agent_publications(
        mut self,
        source: Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>,
    ) -> Self {
        self.agent.agent_publications = Some(source);
        self
    }

    /// Override the default tool-free Outcome Judge with the named Agent. The
    /// Judge runs through the same Run boundary in its own fresh context.
    /// Refused when the host's configured `model_ref` is not an executable
    /// binding, since the Judge snapshot pins it.
    pub fn with_judge(
        mut self,
        judge_agent_id: impl Into<String>,
    ) -> Result<Self, awaken_runtime_contract::resolved::InvalidModelBinding> {
        let id = judge_agent_id.into();
        let snapshot = default_judge_agent(&self.agent.model_ref, &id, DEFAULT_JUDGE_INSTRUCTIONS)?;
        self.agent.judge_snapshot = Some(snapshot);
        Ok(self)
    }

    /// Pin an arbitrary executable Agent snapshot as the Outcome Grader. Its
    /// backend may be Native or ACP; both execute through the same Run boundary.
    pub fn with_judge_snapshot(mut self, snapshot: ExecutableAgentSnapshot) -> Self {
        self.agent.judge_snapshot = Some(snapshot);
        self
    }

    /// Install runtime-only inference materialization. Remote workers use this
    /// without a configuration resolver because admission already pinned access.
    pub fn with_inference_materializer(
        mut self,
        materializer: Arc<dyn awaken_runtime_contract::inference::InferenceExecutorMaterializer>,
    ) -> Self {
        self.resources
            .memory
            .set_inference_materializer(materializer.clone());
        self.agent.inference_routing.set_materializer(materializer);
        self
    }

    /// Bind `model_ref` to `thread` (R2/R5), staged before its first Run.
    pub fn register_thread_model(&self, thread: &str, model_ref: impl Into<String>) {
        self.agent.inference_routing.register(thread, model_ref);
    }

    /// The model id echoed by adapters in their session/agent objects.
    pub fn model(&self) -> String {
        self.agent.model_ref.clone()
    }

    /// The primary model currently selected for `thread`, including a Managed
    /// per-Session/per-Run override. This is a read-only projection of the same
    /// inference-routing source that execution consumes.
    pub fn model_for_thread(&self, thread: &str) -> String {
        self.agent
            .inference_routing
            .model_ref(thread, &self.agent.model_ref)
    }
}
