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
    /// [`InferenceExecutorMaterializer`]: awaken_credential_materializer::InferenceExecutorMaterializer
    pub(crate) llm: Arc<dyn LlmExecutor>,
    pub(crate) model_ref: String,
    /// Per-thread model→executor routing (R1/R2). See [`crate::inference_routing`].
    pub(crate) inference_routing: crate::inference_routing::InferenceRouting,
    /// Remote attempt adapter injected by the process startup. The neutral host
    /// owns only the `RunAttemptExecutor` contract and never names the A2A protocol.
    pub(crate) remote_attempt_executor:
        Option<Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor>>,
    pub(crate) remote_credential_realization:
        awaken_credential_contract::CredentialRealizationCapabilities,
    pub(crate) judge_snapshot: Option<ExecutableAgentSnapshot>,
    /// Runtime-only view of immutable Agent publications.
    pub(crate) agent_publications:
        Option<Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>>,
}

/// The inference and Agent-publication dependencies a composition selects
/// before the Host exists. Consumer: [`crate::HostDependencies`], which turns it
/// into the Host's [`AgentExecution`].
pub(crate) struct AgentSelection {
    pub(crate) llm: Arc<dyn LlmExecutor>,
    pub(crate) model_ref: String,
    pub(crate) remote_attempt_executor:
        Option<Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor>>,
    pub(crate) remote_credential_realization:
        awaken_runtime_contract::CredentialRealizationCapabilities,
    pub(crate) judge_snapshot: Option<ExecutableAgentSnapshot>,
    pub(crate) agent_publications:
        Option<Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>>,
    /// Runtime-only inference materialization shared by per-thread routing
    /// and Memory extraction.
    pub(crate) inference_materializer:
        Option<Arc<dyn awaken_credential_materializer::InferenceExecutorMaterializer>>,
}

impl AgentSelection {
    /// The default executor/model alone: no remote adapter, Judge override,
    /// publication source, or materializer yet.
    pub(in crate::host) fn new(llm: Arc<dyn LlmExecutor>, model_ref: String) -> Self {
        Self {
            llm,
            model_ref,
            remote_attempt_executor: None,
            remote_credential_realization: Default::default(),
            judge_snapshot: None,
            agent_publications: None,
            inference_materializer: None,
        }
    }
}

impl AgentExecution {
    /// The selected executor/model with routing over the Host's Session slots.
    pub(in crate::host) fn new(
        selection: AgentSelection,
        session_slots: crate::session_slot::SessionRuntimeSlots,
    ) -> Self {
        Self {
            llm: selection.llm,
            model_ref: selection.model_ref,
            inference_routing: crate::inference_routing::InferenceRouting::new(
                session_slots,
                selection.inference_materializer,
            ),
            remote_attempt_executor: selection.remote_attempt_executor,
            remote_credential_realization: selection.remote_credential_realization,
            judge_snapshot: selection.judge_snapshot,
            agent_publications: selection.agent_publications,
        }
    }
}

impl SharedHost {
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
