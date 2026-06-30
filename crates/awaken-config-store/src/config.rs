//! The config domain's authoring aggregate.

use awaken_runtime_contract::resolved::ModelBinding;
use serde::{Deserialize, Serialize};

/// A declarative agent configuration, identified by `id`. This is the config
/// domain's source of truth; the runtime never edits it — it consumes only the
/// compiled snapshot (ADR-0031). Field order is the canonical serialization order
/// used for the publication fingerprint, so it must stay stable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentConfig {
    pub id: String,
    pub instructions: String,
    pub max_steps: usize,
    pub model_binding: ModelBinding,
    pub tool_ids: Vec<String>,
}
