//! The config domain's authoring aggregate.

use std::collections::BTreeMap;

use awaken_runtime_contract::resolved::{ContextPolicy, ModelBinding};
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
    /// Plugins active for this agent, by id. A plugin installed on the runtime
    /// contributes only when listed here (G30).
    #[serde(default)]
    pub plugin_ids: Vec<String>,
    /// Per-plugin configuration sections, keyed by plugin id. `BTreeMap` keeps the
    /// serialization deterministic for the publication fingerprint. Each active
    /// plugin reads its own section at resolve; an absent section means defaults.
    #[serde(default)]
    pub plugin_config: BTreeMap<String, serde_json::Value>,
    /// How the model-visible context window is bounded (default
    /// [`ContextPolicy::KeepAll`]). Appended last so it does not reorder the
    /// existing canonical serialization; `#[serde(default)]` keeps configs
    /// authored before this field loadable.
    #[serde(default)]
    pub context_policy: ContextPolicy,
}
