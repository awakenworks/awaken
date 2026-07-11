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
    /// Ordered pool fallbacks tried *after* [`model_binding`](Self::model_binding)
    /// when a candidate fails cleanly (retryable-exhausted or its circuit is open)
    /// and no partial has been committed for the step. Empty for a single-model
    /// agent — unchanged behavior. `#[serde(default)]` keeps older snapshots and
    /// the 40+ existing constructions loadable without carrying the field.
    /// Part of the resolved decision surface (data-only, G3): a pool change is a
    /// config change, so it flows through resolution and the catalog fingerprint.
    #[serde(default)]
    pub model_candidates: Vec<ModelBinding>,
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
    /// How this agent's tools are presented to the model (ADR-0053): per-tool alias /
    /// description override / defer, keyed by canonical id (catalog or MCP). Empty for
    /// an agent with no overrides — the tool face is then byte-identical to before, so
    /// `#[serde(default)]` keeps the 40+ existing snapshot constructions loadable.
    #[serde(default)]
    pub tool_presentation: ToolPresentation,
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

    /// The ordered model bindings this run may use: the primary
    /// [`model_binding`](Self::model_binding) first, then any pool fallbacks in
    /// [`model_candidates`](Self::model_candidates). A single-model agent yields
    /// exactly one. The engine tries them in order, failing over to the next only
    /// on a clean pre-commit failure of the current one (never mid-stream).
    #[must_use]
    pub fn candidate_bindings(&self) -> Vec<&ModelBinding> {
        std::iter::once(&self.model_binding)
            .chain(self.model_candidates.iter())
            .collect()
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// The in-process awaken model+tool loop. Any non-`acp:`/`a2a:` backend.
    Native,
    /// A launched external ACP CLI (Claude Code, Codex, …). `cli` is the catalog id
    /// (`AcpCli::id`) parsed from `acp:<cli>` (empty for a bare `acp`).
    Acp { cli: String },
    /// A remote agent reached over A2A HTTP (no local process). `endpoint` is the
    /// dial URL parsed from `a2a:<endpoint>`; the A2A executor consumes it.
    Remote { endpoint: String },
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
        } else if let Some(endpoint) = backend_ref.strip_prefix("a2a:") {
            Backend::Remote {
                endpoint: endpoint.to_string(),
            }
        } else {
            Backend::Native
        }
    }

    /// The remote dial endpoint if this is an A2A backend.
    #[must_use]
    pub fn remote_endpoint(&self) -> Option<&str> {
        match self {
            Backend::Remote { endpoint } => Some(endpoint),
            _ => None,
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
    /// The *provider identity* — the principal whose key and quota this attempt
    /// runs under. It is the cooldown / account-spread key: two candidates on the
    /// same model but different identities differ here, so each tracks its own
    /// circuit and quota. (Formerly `provider_instance_ref`; renamed to name the
    /// principal, aligning with awaken-next's `ProviderIdentity`.)
    pub provider_identity_ref: String,
    pub model_ref: String,
    pub backend_ref: String,
}

impl ModelBinding {
    /// The provider instance, model, and backend a run binds to.
    pub fn new(
        provider_identity_ref: impl Into<String>,
        model_ref: impl Into<String>,
        backend_ref: impl Into<String>,
    ) -> Self {
        Self {
            provider_identity_ref: provider_identity_ref.into(),
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

/// The model-facing presentation of an agent's tools (ADR-0053): a per-tool `alias`,
/// `description` override, and `defer` flag, keyed by the tool's **canonical** id — a
/// catalog id or an MCP `mcp__<server>__<tool>` id — so it applies uniformly to static
/// and MCP tools alike.
///
/// This value object owns the single invariant of tool aliasing: *an alias exists only
/// in the model-facing layer; every internal consumer (permission gate, state machine,
/// dispatch, metrics) sees the canonical id.* It exposes exactly the two directions that
/// invariant needs — [`present`](Self::present) (canonical descriptors → the model face)
/// and [`resolve`](Self::resolve) (a model-supplied id → its canonical id) — so no caller
/// re-implements the mapping. An empty presentation is inert: [`present`](Self::present)
/// returns its input unchanged and [`resolve`](Self::resolve) is the identity, so a
/// config with no overrides compiles to a byte-identical tool face.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolPresentation {
    /// canonical tool id → its facet. `BTreeMap` keeps serialization deterministic.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    facets: BTreeMap<String, ToolFacet>,
}

/// One tool's presentation facet: how it appears to the model, keyed in
/// [`ToolPresentation`] by canonical id. All fields optional/false so an entry that
/// only defers (no rename) is as valid as one that only renames.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolFacet {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub defer: bool,
}

/// The result of [`ToolPresentation::present`]: the descriptors the model sees this
/// step (`face`) and the ones withheld until opened (`deferred`), both already renamed
/// and re-described. `deferred` is empty unless some facet sets `defer`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PresentedTools {
    pub face: Vec<ToolDescriptor>,
    pub deferred: Vec<ToolDescriptor>,
}

impl ToolPresentation {
    /// Build from `(canonical_id, facet)` pairs; entries whose facet is entirely
    /// default (no alias, no description, not deferred) are dropped so an all-default
    /// presentation is [`is_empty`](Self::is_empty) and stays byte-identical.
    pub fn from_facets(facets: impl IntoIterator<Item = (String, ToolFacet)>) -> Self {
        let facets = facets
            .into_iter()
            .filter(|(_, f)| {
                f.alias.is_some() || f.description.is_some() || f.defer
            })
            .collect();
        Self { facets }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.facets.is_empty()
    }

    /// The canonical ids this presentation overrides (used at compile to validate each
    /// targets a selected tool).
    pub fn targets(&self) -> impl Iterator<Item = &str> {
        self.facets.keys().map(String::as_str)
    }

    /// Reverse a model-supplied tool id back to its canonical id (the identity when the
    /// id is not an alias). The single choke every internal consumer routes a tool call
    /// through, so the alias never leaks past the model-facing boundary.
    #[must_use]
    pub fn resolve<'a>(&'a self, model_id: &'a str) -> &'a str {
        self.facets
            .iter()
            .find(|(_, f)| f.alias.as_deref() == Some(model_id))
            .map_or(model_id, |(canonical, _)| canonical.as_str())
    }

    /// Split canonical descriptors into the model face (alias + description applied) and
    /// the deferred set (withheld until opened). A descriptor with no facet passes
    /// through to the face unchanged.
    #[must_use]
    pub fn present(&self, descriptors: &[ToolDescriptor]) -> PresentedTools {
        let mut out = PresentedTools::default();
        for d in descriptors {
            match self.facets.get(&d.id) {
                None => out.face.push(d.clone()),
                Some(f) => {
                    let mut shown = d.clone();
                    if let Some(alias) = &f.alias {
                        shown.id = alias.clone();
                    }
                    if let Some(desc) = &f.description {
                        shown.description = desc.clone();
                    }
                    if f.defer {
                        out.deferred.push(shown);
                    } else {
                        out.face.push(shown);
                    }
                }
            }
        }
        out
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
    use super::{Backend, ModelBinding, ToolDescriptor, ToolFacet, ToolPresentation};

    fn td(id: &str) -> ToolDescriptor {
        ToolDescriptor::pinned("t", id, format!("desc of {id}"), serde_json::json!({}))
    }

    #[test]
    fn empty_presentation_is_the_identity() {
        let p = ToolPresentation::default();
        assert!(p.is_empty());
        let tools = vec![td("a"), td("mcp__x__y")];
        let out = p.present(&tools);
        assert_eq!(out.face, tools, "no overrides ⇒ face unchanged");
        assert!(out.deferred.is_empty());
        assert_eq!(p.resolve("a"), "a", "no alias ⇒ resolve is identity");
    }

    #[test]
    fn present_renames_redescribes_and_defers_by_canonical_id() {
        // Works identically for a static id and an MCP id.
        let p = ToolPresentation::from_facets([
            ("a".to_string(), ToolFacet { alias: Some("say".into()), description: Some("Speak.".into()), defer: false }),
            ("mcp__x__y".to_string(), ToolFacet { alias: Some("y".into()), description: None, defer: true }),
            ("noop".to_string(), ToolFacet::default()), // all-default ⇒ dropped
        ]);
        assert!(!p.is_empty());
        let out = p.present(&[td("a"), td("mcp__x__y"), td("keep")]);
        // `a` renamed + redescribed and stays in the face; `keep` passes through.
        assert!(out.face.iter().any(|d| d.id == "say" && d.description == "Speak."));
        assert!(out.face.iter().any(|d| d.id == "keep"));
        // The MCP tool is deferred (renamed) — withheld from the face.
        assert!(out.face.iter().all(|d| d.id != "y"));
        assert!(out.deferred.iter().any(|d| d.id == "y"));
    }

    #[test]
    fn resolve_reverses_an_alias_to_its_canonical_id() {
        let p = ToolPresentation::from_facets([
            ("mcp__x__y".to_string(), ToolFacet { alias: Some("y".into()), ..Default::default() }),
        ]);
        // The single choke: a model call by alias reverses to the canonical id; a
        // non-alias (e.g. an un-renamed tool) passes through untouched.
        assert_eq!(p.resolve("y"), "mcp__x__y");
        assert_eq!(p.resolve("other"), "other");
    }

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

        // `a2a:<endpoint>` is a remote A2A backend.
        assert_eq!(
            Backend::from_ref("a2a:https://host/a2a"),
            Backend::Remote {
                endpoint: "https://host/a2a".to_string()
            }
        );
        assert_eq!(
            Backend::from_ref("a2a:https://host/a2a").remote_endpoint(),
            Some("https://host/a2a")
        );
        assert!(!Backend::from_ref("a2a:x").is_acp());
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
