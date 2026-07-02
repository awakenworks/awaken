use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The content address of a resolved catalog: `sha256` of the canonical config.
/// It is **derived, not chosen** — a producer (`awaken-config-store::compile`)
/// computes it and stamps it into the snapshot and the install; the runtime only
/// re-checks the parts agree (fail-closed). The public field exists for transport
/// and deserialization, not for authoring: never hand-pick a value here — compile
/// a config instead, or the runtime's resolution will reject the mismatch.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CatalogFingerprint(pub String);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedSpec {
    pub catalog_fingerprint: CatalogFingerprint,
    /// The agent's instructions: the behavior text the runtime injects as the
    /// leading system message of every inference request. Part of the resolved
    /// decision surface (data-only, G3); empty means the run carries no
    /// agent-level system message.
    pub instructions: String,
    /// The agent's ceiling on model/tool loop steps for one run. The loop ends
    /// with `EndCause::MaxSteps` if it reaches this without a natural end. Part
    /// of the resolved decision surface (data-only, G3); the config side owns a
    /// sensible value, the runtime only honors it.
    pub max_steps: usize,
    pub model_binding: ModelBinding,
    pub tool_descriptors: Vec<ToolDescriptor>,
    pub plugin_ids: Vec<String>,
    /// Per-plugin configuration, keyed by plugin id. Raw JSON so the runtime
    /// carries it across the config→runtime edge without naming any plugin's
    /// config type (data-only, G3). A plugin whose id is absent runs with its
    /// defaults; a plugin reads only its own section at resolve.
    #[serde(default)]
    pub plugin_config: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelBinding {
    pub provider_instance_ref: String,
    pub model_ref: String,
    pub backend_ref: String,
}

impl ModelBinding {
    /// The provider instance, model, and backend a run binds to.
    pub fn new(
        provider_instance_ref: impl Into<String>,
        model_ref: impl Into<String>,
        backend_ref: impl Into<String>,
    ) -> Self {
        Self {
            provider_instance_ref: provider_instance_ref.into(),
            model_ref: model_ref.into(),
            backend_ref: backend_ref.into(),
        }
    }
}

/// Model-visible tool identity pinned in the resolved spec. The runtime projects
/// `id`/`description`/`parameters` into the inference request, and `content_hash`
/// covers all three so a schema change changes the hash (G3/G8). It carries no
/// executable handle — authority lives behind the gate and `ToolExecutor`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub id: String,
    /// Natural-language description shown to the model.
    pub description: String,
    /// JSON Schema for the tool arguments. The executing side validates calls
    /// against this; an empty object means "no declared parameters".
    pub parameters: serde_json::Value,
    pub content_hash: String,
}

impl ToolDescriptor {
    /// Build a descriptor whose `content_hash` is derived from the id,
    /// description, and parameter schema, so any of those changing changes the
    /// hash. `prefix` namespaces the owner (e.g. `builtin:hand`).
    pub fn pinned(
        prefix: &str,
        id: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        let id = id.into();
        let description = description.into();
        let content_hash = content_hash(prefix, &id, &description, &parameters);
        Self {
            id,
            description,
            parameters,
            content_hash,
        }
    }
}

/// Stable content hash over the model-visible descriptor surface. Uses a
/// canonical JSON encoding so equal schemas hash equally regardless of the
/// in-memory `Value` shape.
fn content_hash(
    prefix: &str,
    id: &str,
    description: &str,
    parameters: &serde_json::Value,
) -> String {
    use std::hash::{Hash, Hasher};
    let canonical = serde_json::to_string(parameters).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    description.hash(&mut hasher);
    canonical.hash(&mut hasher);
    format!("{prefix}:{id}:{:016x}", hasher.finish())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedRun {
    pub snapshot_id: crate::snapshot::ExecutableAgentSnapshotId,
    pub spec: ResolvedSpec,
}

#[cfg(test)]
mod tests {
    use super::ToolDescriptor;

    #[test]
    fn content_hash_covers_id_description_and_schema() {
        let base = ToolDescriptor::pinned("p", "t", "desc", serde_json::json!({"a": 1}));

        // Same inputs hash equally.
        let same = ToolDescriptor::pinned("p", "t", "desc", serde_json::json!({"a": 1}));
        assert_eq!(base.content_hash, same.content_hash);

        // Any surface change moves the hash.
        let schema_changed = ToolDescriptor::pinned("p", "t", "desc", serde_json::json!({"a": 2}));
        let desc_changed = ToolDescriptor::pinned("p", "t", "other", serde_json::json!({"a": 1}));
        let id_changed = ToolDescriptor::pinned("p", "u", "desc", serde_json::json!({"a": 1}));
        assert_ne!(base.content_hash, schema_changed.content_hash);
        assert_ne!(base.content_hash, desc_changed.content_hash);
        assert_ne!(base.content_hash, id_changed.content_hash);
        assert!(base.content_hash.starts_with("p:t:"));
    }
}
