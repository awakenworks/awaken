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
}

impl RunExecutionRequest {
    pub fn new(activation: RunActivation) -> Self {
        Self { activation }
    }

    pub fn run_id(&self) -> &RunId {
        &self.activation.run_id
    }

    pub fn thread_id(&self) -> &ThreadId {
        &self.activation.thread_id
    }
}
