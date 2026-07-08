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
    /// How the model-visible context window is bounded before each inference.
    /// Part of the resolved decision surface (data-only, G3). Defaults to
    /// [`ContextPolicy::KeepAll`] so an unset config sends the whole transcript
    /// (unchanged behavior); `#[serde(default)]` keeps older snapshots loadable.
    #[serde(default)]
    pub context_policy: ContextPolicy,
}

impl ResolvedSpec {
    /// The execution adapter this run binds to (R3): `"awaken"` for the native
    /// runtime, or `"acp:<cli>"` for an external ACP agent (Claude Code / Codex).
    /// Derived by convention from the model binding's `backend_ref` — an `acp:*`
    /// backend selects that ACP CLI — so runtime selection is first-class without
    /// churning the resolved-spec shape (30+ existing constructions).
    #[must_use]
    pub fn runtime_adapter(&self) -> &str {
        let backend = self.model_binding.backend_ref.as_str();
        if backend == "acp" || backend.starts_with("acp:") {
            backend
        } else {
            "awaken"
        }
    }
}

/// The execution backend a resolved agent binds to (R3/R4): the in-process awaken
/// runtime, or a launched external ACP CLI. A *typed view* over the model binding's
/// runtime-selection axis — the string `backend_ref` (`"genai"`, `"acp:claude"`)
/// stays the wire/storage encoding, but every routing decision matches this sum so
/// the choice is exhaustive and each variant carries only its own data (the ACP
/// profile). `Backend` owns *only* the runtime axis, never the native provider
/// (`backend_ref: "genai"` routes a provider inside `Native`, not a fourth kind).
///
/// A `Remote` (A2A) variant is intentionally absent until an executor consumes it
/// — a variant no dispatch arm reads would be a stub (G30/no-stubs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// The in-process awaken model+tool loop. Any non-`acp:` backend.
    Native,
    /// A launched external ACP CLI (Claude Code, Codex, …). `cli` is the catalog id
    /// (`AcpCli::id`) parsed from `acp:<cli>` (empty for a bare `acp`).
    Acp { cli: String },
}

impl Backend {
    /// Parse the typed backend from a `backend_ref` string. Total: any non-`acp`
    /// value is [`Backend::Native`] (it names a provider inside the native runtime),
    /// `acp` / `acp:<profile>` is [`Backend::Acp`].
    #[must_use]
    pub fn from_ref(backend_ref: &str) -> Self {
        if backend_ref == "acp" {
            Backend::Acp { cli: String::new() }
        } else if let Some(cli) = backend_ref.strip_prefix("acp:") {
            Backend::Acp {
                cli: cli.to_string(),
            }
        } else {
            Backend::Native
        }
    }

    /// Whether this run is served by an external ACP CLI rather than the native loop.
    #[must_use]
    pub fn is_acp(&self) -> bool {
        matches!(self, Backend::Acp { .. })
    }
}

/// How the model-visible context window is bounded before each inference.
///
/// The policy trims a *view* of the transcript that goes to the model; the
/// committed history stays whole (G13). A separate summarizing compactor may
/// later replace old turns with a summary — this is the cheap, lossy alternative
/// that just drops them from the request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ContextPolicy {
    /// Send the whole transcript every turn (no bound).
    #[default]
    KeepAll,
    /// Rolling window: keep every leading system message, then only the last
    /// `keep_last` non-system messages; older non-system messages are dropped
    /// from the request view. `keep_last == 0` keeps only the system prefix.
    KeepLast { keep_last: usize },
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
    use super::{Backend, ModelBinding, ToolDescriptor};

    #[test]
    fn backend_typed_view_distinguishes_native_and_acp() {
        // Any non-`acp` ref is Native — including the provider axis "genai", which
        // Backend must NOT absorb as a fourth kind.
        assert_eq!(Backend::from_ref("genai"), Backend::Native);
        assert_eq!(Backend::from_ref("default"), Backend::Native);
        assert!(!Backend::from_ref("genai").is_acp());

        // `acp` / `acp:<profile>` carry the launched-CLI profile the string used to
        // smuggle — now a typed field.
        assert_eq!(
            Backend::from_ref("acp"),
            Backend::Acp { cli: String::new() }
        );
        assert_eq!(
            Backend::from_ref("acp:claude"),
            Backend::Acp {
                cli: "claude".to_string()
            }
        );
        assert!(Backend::from_ref("acp:codex").is_acp());

        // The binding's stored `backend_ref` parses to the same typed backend.
        assert_eq!(
            Backend::from_ref(&ModelBinding::new("p", "m", "acp:codex").backend_ref),
            Backend::Acp {
                cli: "codex".to_string()
            }
        );
    }

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
