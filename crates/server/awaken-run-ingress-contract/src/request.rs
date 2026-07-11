//! The serializable durable-run instruction.
//!
//! [`RunExecutionRequest`] is exactly the data a durable dispatch queue persists
//! and replays — it carries no live handles (G3/G4), so a crash loses nothing the
//! queue cannot rebuild. The per-attempt live wiring (`RunExecutionContext`) stays
//! in the `awaken-run-ingress` host.

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::activation::RunActivation;
use serde::{Deserialize, Serialize};

/// The durable, serializable record of an accepted run. It holds no `Arc<dyn ...>`,
/// registry, or live handle (G3); the runtime builds live execution objects from
/// the activation's pinned snapshot on each attempt (G4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunExecutionRequest {
    pub activation: RunActivation,
    /// W3C `traceparent` captured when the run was admitted, so a durably-dispatched
    /// execution continues the admitting request's distributed trace across the
    /// queue boundary. Absent when admitted without an active trace (or by an older
    /// writer): a pre-existing queue row simply deserializes it as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
}

impl RunExecutionRequest {
    pub fn new(activation: RunActivation) -> Self {
        Self {
            activation,
            traceparent: None,
        }
    }

    /// Attach the admitting request's W3C `traceparent` (see the field docs).
    pub fn with_traceparent(mut self, traceparent: Option<String>) -> Self {
        self.traceparent = traceparent;
        self
    }

    pub fn run_id(&self) -> &RunId {
        &self.activation.run_id
    }

    pub fn thread_id(&self) -> &ThreadId {
        &self.activation.thread_id
    }
}
