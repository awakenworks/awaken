use serde::{Deserialize, Serialize};

use crate::model_access::ModelAccessGrant;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunActivation {
    pub run_id: awaken_agent_contract::agent::run::Id,
    pub thread_id: awaken_agent_contract::agent::thread::Id,
    pub snapshot: crate::snapshot::ExecutableAgentSnapshot,
    pub input: Vec<awaken_agent_contract::agent::message::Message>,
    pub trace: TraceContext,
    /// How this run is authorized to reach its model (ADR-0004). A per-run,
    /// secret-free grant carried on the activation envelope (never on the
    /// fingerprinted spec, so an ephemeral lease token cannot pollute catalog
    /// identity). Absent on the wire ⇒ the explicit local-self-credentialed
    /// default, so a self-hosted run with no gateway works unchanged.
    #[serde(default)]
    pub model_access: ModelAccessGrant,
}

impl RunActivation {
    /// A fresh activation with the default (local self-credentialed) model access
    /// and an empty trace context. Use the `with_*` builders to set either. This is
    /// the single construction seam so a new activation field never reopens every
    /// call site.
    #[must_use]
    pub fn new(
        run_id: awaken_agent_contract::agent::run::Id,
        thread_id: awaken_agent_contract::agent::thread::Id,
        snapshot: crate::snapshot::ExecutableAgentSnapshot,
        input: Vec<awaken_agent_contract::agent::message::Message>,
    ) -> Self {
        Self {
            run_id,
            thread_id,
            snapshot,
            input,
            trace: TraceContext::default(),
            model_access: ModelAccessGrant::default(),
        }
    }

    /// Set the model-access grant (placement/adapter wiring).
    #[must_use]
    pub fn with_model_access(mut self, grant: ModelAccessGrant) -> Self {
        self.model_access = grant;
        self
    }

    /// Set the trace context.
    #[must_use]
    pub fn with_trace(mut self, trace: TraceContext) -> Self {
        self.trace = trace;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TraceContext {
    pub trace_id: Option<String>,
}
