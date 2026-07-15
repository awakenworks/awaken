use serde::{Deserialize, Serialize};

use crate::model_access::ModelAccessGrant;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunActivation {
    pub run_id: awaken_agent_contract::agent::run::Id,
    pub thread_id: awaken_agent_contract::agent::thread::Id,
    pub snapshot: crate::snapshot::ExecutableAgentSnapshot,
    pub input: Vec<awaken_agent_contract::agent::message::Message>,
    /// How this run is authorized to reach its model (ADR-0004). A per-run,
    /// secret-free grant carried on the activation envelope (never on the
    /// fingerprinted spec, so an ephemeral lease token cannot pollute catalog
    /// identity). Absent on the wire ⇒ the explicit local-self-credentialed
    /// default, so a self-hosted run with no gateway works unchanged.
    #[serde(default)]
    pub model_access: ModelAccessGrant,
}

impl RunActivation {
    /// A fresh activation with the default (local self-credentialed) model access.
    /// Use [`with_model_access`](Self::with_model_access) to set a gateway grant.
    ///
    /// Distributed *trace* propagation is NOT carried here: the admitting request's
    /// W3C `traceparent` rides the ingress envelope (`RunExecutionRequest`) across
    /// the durable queue and is restored as the `wake.dispatch` span's remote
    /// parent, so a durably-drained run still nests under the trace that submitted
    /// it. The runtime core never reads a trace field.
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
            model_access: ModelAccessGrant::default(),
        }
    }

    /// Set the model-access grant (placement/adapter wiring).
    #[must_use]
    pub fn with_model_access(mut self, grant: ModelAccessGrant) -> Self {
        self.model_access = grant;
        self
    }
}
