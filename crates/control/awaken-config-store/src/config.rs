//! The config domain's authoring aggregate.

use std::collections::BTreeMap;

use awaken_runtime_contract::resolved::{ContextPolicy, ModelBinding};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// How an agent config's model is chosen at authoring time (ADR-0052 D5). This is
/// the *source* selection, distinct from the resolved concrete [`ModelBinding`] the
/// runtime consumes: the resolver collapses [`Auto`](ModelSelection::Auto) to a
/// first-offering at publish, and passes a [`Pinned`](ModelSelection::Pinned)
/// binding through untouched. The two variants *name* the two intents at the type
/// level, so no reader has to know that an absent value carries behavior.
///
/// Wire compatibility is deliberate: a `Pinned` binding serializes as the bare flat
/// triple it always was (`{provider_instance_ref, model_ref, backend_ref}`), so
/// every config authored before this type — and its content-address fingerprint —
/// is byte-identical. Only `Auto` is new, serialized as `{"mode":"auto"}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSelection {
    /// Resolve to a first provider-backed offering at publish (the default). The
    /// reconciler re-resolves these on a model-catalog change (ADR-0052 D5).
    Auto,
    /// The operator's explicit concrete binding — never overwritten by resolution.
    Pinned(ModelBinding),
}

impl Default for ModelSelection {
    fn default() -> Self {
        ModelSelection::Auto
    }
}

impl ModelSelection {
    /// A pinned binding from its three refs (ergonomic constructor for the many
    /// call sites that authored a concrete `ModelBinding::new(...)`).
    pub fn pinned(
        provider_instance_ref: impl Into<String>,
        model_ref: impl Into<String>,
        backend_ref: impl Into<String>,
    ) -> Self {
        ModelSelection::Pinned(ModelBinding::new(
            provider_instance_ref,
            model_ref,
            backend_ref,
        ))
    }

    /// The concrete binding if pinned, `None` if still `Auto`. `compile` requires a
    /// resolved binding, so `None` here is the fail-closed "resolve me first" signal.
    #[must_use]
    pub fn resolved(&self) -> Option<&ModelBinding> {
        match self {
            ModelSelection::Pinned(binding) => Some(binding),
            ModelSelection::Auto => None,
        }
    }

    /// Whether the selection is still `Auto` (the reconciler re-resolves these).
    #[must_use]
    pub fn is_auto(&self) -> bool {
        matches!(self, ModelSelection::Auto)
    }
}

impl From<ModelBinding> for ModelSelection {
    fn from(binding: ModelBinding) -> Self {
        ModelSelection::Pinned(binding)
    }
}

impl Serialize for ModelSelection {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            // Byte-identical to the historic flat triple, so existing configs and
            // their fingerprints are unchanged.
            ModelSelection::Pinned(binding) => binding.serialize(serializer),
            ModelSelection::Auto => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("mode", "auto")?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ModelSelection {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Peek the shape: `{"mode":"auto"}` is Auto; anything else is the flat
        // triple (back-compat) or a `mode:"pinned"` triple, both decoding to Pinned.
        let value = serde_json::Value::deserialize(deserializer)?;
        if value.get("mode").and_then(serde_json::Value::as_str) == Some("auto") {
            return Ok(ModelSelection::Auto);
        }
        let binding =
            serde_json::from_value::<ModelBinding>(value).map_err(serde::de::Error::custom)?;
        Ok(ModelSelection::Pinned(binding))
    }
}

/// A declarative agent configuration, identified by `id`. This is the config
/// domain's source of truth; the runtime never edits it — it consumes only the
/// compiled snapshot (ADR-0031). Field order is the canonical serialization order
/// used for the publication fingerprint, so it must stay stable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentConfig {
    pub id: String,
    pub instructions: String,
    pub max_steps: usize,
    /// The model selection (ADR-0052 D5): `Auto` (resolve at publish) or a
    /// `Pinned` concrete binding. Serializes wire-identically to the historic flat
    /// triple when pinned, so the field name and the publication fingerprint of
    /// every pre-existing config are unchanged.
    pub model_binding: ModelSelection,
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
    /// Glob patterns (`*` wildcard) selecting additional tools from the catalog by
    /// id at compile — a permissive selector that complements the exact `tool_ids`.
    /// Unlike a `tool_id`, a pattern that matches nothing is not an error (it is a
    /// filter, not a reference). Appended last with `skip_serializing_if` so an
    /// empty set serializes to nothing and keeps prior fingerprints byte-identical;
    /// a non-empty set enters the content address like any other config field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_patterns: Vec<String>,
    /// Ordered model-pool fallbacks (#1): tried after `model_binding` when a
    /// candidate fails cleanly, so an agent survives a model outage. Appended last
    /// with `skip_serializing_if` so a single-model config's fingerprint stays
    /// byte-identical; a non-empty pool enters the content address like any field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_candidates: Vec<ModelBinding>,
}
