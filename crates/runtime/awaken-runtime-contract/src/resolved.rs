use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

fn is_default_delegation_limits(
    limits: &awaken_agent_contract::agent::delegation::DelegationLimits,
) -> bool {
    limits == &awaken_agent_contract::agent::delegation::DelegationLimits::default()
}

/// The content address of a resolved catalog: `sha256` of the canonical config.
/// It is **derived, not chosen** — a producer (`awaken-config-store::compile`)
/// computes it and stamps it into the snapshot and the install; the runtime only
/// re-checks the parts agree (fail-closed). The public field exists for transport
/// and deserialization, not for authoring: never hand-pick a value here — compile
/// a config instead, or the runtime's resolution will reject the mismatch.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CatalogFingerprint(pub String);

/// Adapter-native ACP Session intent frozen with one BackendOwned candidate.
/// Omission preserves backend defaults; no discovered schema is copied here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AcpSessionConfiguration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
}

impl AcpSessionConfiguration {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mode.is_none() && self.options.is_empty()
    }
}

/// The one typed codec for the ACP-owned section of an Agent publication.
///
/// This is authoring intent carried across the config/runtime boundary, not
/// discovered capability and not launch policy. Keeping the codec in the
/// neutral runtime contract prevents the ACP executor and Runtime Host from
/// maintaining parallel JSON readers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpSpec {
    /// The external CLI's own compaction window in tokens. Omission preserves
    /// the adapter default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_window: Option<u64>,
    /// Legacy publication projection of MCP routes. New authoring owns MCP in
    /// `AgentBindings`; this field remains the single compatibility codec until
    /// every persisted publication has crossed that typed boundary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<AcpMcpServer>,
}

impl AcpSpec {
    /// Decode `plugin_config["acp"]` once for every consuming bounded context.
    ///
    /// Historical publications were fail-soft per field, so one malformed MCP
    /// list must not hide an independently valid compaction window.
    #[must_use]
    pub fn from_plugin_config(plugin_config: &BTreeMap<String, serde_json::Value>) -> Self {
        let Some(acp) = plugin_config.get("acp") else {
            return Self::default();
        };
        Self {
            compact_window: acp
                .get("compact_window")
                .and_then(serde_json::Value::as_u64),
            mcp_servers: acp
                .get("mcp_servers")
                .and_then(|value| serde_json::from_value::<Vec<AcpMcpServer>>(value.clone()).ok())
                .unwrap_or_default(),
        }
    }
}

/// A secret-free MCP route delivered to an external ACP adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpMcpServer {
    pub name: String,
    pub transport: AcpMcpTransport,
}

/// The transport coordinates for one ACP-visible MCP route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AcpMcpTransport {
    Stdio { command: String, args: Vec<String> },
    Http { url: String },
}

/// Provider-facing endpoint facts frozen into a complete model candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceEndpoint {
    pub adapter_kind: String,
    /// Exact catalog protocol. Empty only for legacy snapshots that predate
    /// protocol pinning; modern provider realization must not infer it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_dialect: String,
    pub base_url: String,
    pub upstream_model: String,
}

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
    /// Run-scoped depth, parallelism, and total child limits. The target Agent's
    /// own resolved value applies when that child delegates again.
    #[serde(default, skip_serializing_if = "is_default_delegation_limits")]
    pub delegation_limits: awaken_agent_contract::agent::delegation::DelegationLimits,
    pub model_binding: ResolvedModelCandidate,
    /// Ordered pool fallbacks tried *after* [`model_binding`](Self::model_binding)
    /// when a candidate fails cleanly (retryable-exhausted or its circuit is open)
    /// and no partial has been committed for the step. Empty for a single-model
    /// agent — unchanged behavior. `#[serde(default)]` keeps older snapshots and
    /// the 40+ existing constructions loadable without carrying the field.
    /// Part of the resolved decision surface (data-only, G3): a pool change is a
    /// config change, so it flows through resolution and the catalog fingerprint.
    #[serde(default)]
    pub model_candidates: Vec<ResolvedModelCandidate>,
    pub tool_descriptors: Vec<ToolDescriptor>,
    pub plugin_ids: Vec<String>,
    /// Per-plugin configuration, keyed by plugin id. Raw JSON so the runtime
    /// carries it across the config→runtime edge without naming any plugin's
    /// config type (data-only, G3). A plugin whose id is absent runs with its
    /// defaults; a plugin reads only its own section at resolve.
    #[serde(default)]
    pub plugin_config: crate::agent_bindings::ResolvedConfiguration,
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
    /// Ordered published candidates eligible for one execution request. A
    /// nonblank override scopes by model while retaining every published route
    /// for that model; absent/blank retains primary plus all fallbacks.
    #[must_use]
    pub fn execution_candidates(
        &self,
        model_ref_override: Option<&str>,
    ) -> Vec<&ResolvedModelCandidate> {
        let selected = model_ref_override.filter(|model| !model.is_empty());
        std::iter::once(&self.model_binding)
            .chain(self.model_candidates.iter())
            .filter(|candidate| selected.is_none_or(|model| candidate.binding.model_ref == model))
            .collect()
    }

    /// The ordered model bindings this run may use: the primary
    /// [`model_binding`](Self::model_binding) first, then any pool fallbacks in
    /// [`model_candidates`](Self::model_candidates). A single-model agent yields
    /// exactly one. The engine tries them in order, failing over to the next only
    /// on a clean pre-commit failure of the current one (never mid-stream).
    #[must_use]
    pub fn candidate_bindings(&self) -> Vec<&ModelBinding> {
        std::iter::once(&self.model_binding.binding)
            .chain(
                self.model_candidates
                    .iter()
                    .map(|candidate| &candidate.binding),
            )
            .collect()
    }

    /// The complete publication-pinned candidate for `binding`. Provisioning uses
    /// this exact lookup so two routes for the same upstream model cannot share
    /// credential or endpoint state accidentally.
    #[must_use]
    pub fn candidate_for_binding(&self, binding: &ModelBinding) -> Option<&ResolvedModelCandidate> {
        std::iter::once(&self.model_binding)
            .chain(self.model_candidates.iter())
            .find(|candidate| &candidate.binding == binding)
    }

    /// The first complete published candidate whose model id matches an explicit
    /// run override. The override is a model selector, never a route selector.
    #[must_use]
    pub fn candidate_for_model(&self, model_ref: &str) -> Option<&ResolvedModelCandidate> {
        std::iter::once(&self.model_binding)
            .chain(self.model_candidates.iter())
            .find(|candidate| candidate.binding.model_ref == model_ref)
    }

    /// Select the ordered subset for one model from this ephemeral resolved view.
    ///
    /// The durable, content-addressed snapshot remains untouched. Selection is
    /// fail-closed: `false` means the requested model was not published and no
    /// state changed. A successful explicit selection retains every published
    /// candidate for that model in publication order. This preserves account/route
    /// failover without allowing execution to leave the model selected for this run.
    pub fn select_execution_model(&mut self, model_ref: &str) -> bool {
        let mut selected = std::iter::once(&self.model_binding)
            .chain(self.model_candidates.iter())
            .filter(|candidate| candidate.binding.model_ref == model_ref)
            .cloned()
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return false;
        }
        self.model_binding = selected.remove(0);
        self.model_candidates = selected;
        true
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
/// later replace old Steps with a summary — this is the cheap, lossy alternative
/// that just drops them from the request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ContextPolicy {
    /// Send the whole transcript on every inference Step (no bound).
    #[default]
    KeepAll,
    /// Rolling window: keep every leading system message, then only the last
    /// `keep_last` non-system messages; older non-system messages are dropped
    /// from the request view. `keep_last == 0` keeps only the system prefix.
    KeepLast { keep_last: usize },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

/// Whether an external backend keeps its own default model or receives the exact
/// model id frozen in the publication. This remains explicit in the immutable
/// candidate; an empty model string is never interpreted as policy by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendModelSelection {
    Default,
    Exact,
}

/// The provisioning facts for one published model candidate. This is snapshot
/// data, not secret material and not an IAM decision. Local endpoints, gateways,
/// and provider SaaS use `Provider`; a trusted local ACP agent uses
/// `BackendOwned`; only an explicitly installed in-process executor uses
/// `HostExecutor`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelProvisioning {
    /// Direct/test composition with an executor installed by the host. Published
    /// provider configurations never fall back to this variant.
    #[default]
    HostExecutor,
    /// An external backend owns endpoint, account, refresh, and credential
    /// material. Awaken pins only the exact Worker-local liveness reference and
    /// the model-selection policy; it never materializes this credential.
    BackendOwned {
        credential: crate::CredentialRef,
        model_selection: BackendModelSelection,
        /// Exact live ACP capability profile validated at publication. Empty
        /// legacy values fail closed in placement and launch.
        #[serde(default)]
        capability_fingerprint: String,
        #[serde(default)]
        capability_adapter_version: String,
        #[serde(default, skip_serializing_if = "AcpSessionConfiguration::is_empty")]
        session_configuration: AcpSessionConfiguration,
    },
    /// Exact provider route and credential delivery frozen by publication.
    Provider {
        provider_ref: String,
        route_ref: String,
        /// Opaque ownership coordinate for the pinned credential. It denotes a
        /// Workspace today but deliberately carries no action/capability: scope
        /// range and authorization function remain orthogonal.
        scope_id: awaken_tenancy::ScopeId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential: Option<Box<crate::CredentialAccess>>,
        endpoint: Box<crate::InferenceEndpoint>,
    },
}

impl ModelProvisioning {
    fn is_host_executor(&self) -> bool {
        matches!(self, Self::HostExecutor)
    }
}

/// One complete, immutable model candidate in an executable publication.
///
/// `binding` is the small runtime identity copied into [`crate::ChatRequest`].
/// `provisioning` is consumed before the runtime loop to create an executor and
/// therefore never needs to enter each model request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedModelCandidate {
    #[serde(flatten)]
    pub binding: ModelBinding,
    #[serde(default, skip_serializing_if = "ModelProvisioning::is_host_executor")]
    pub provisioning: ModelProvisioning,
}

impl ResolvedModelCandidate {
    #[must_use]
    pub fn host(binding: ModelBinding) -> Self {
        Self {
            binding,
            provisioning: ModelProvisioning::HostExecutor,
        }
    }

    #[must_use]
    pub fn provider(
        binding: ModelBinding,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        scope_id: impl Into<awaken_tenancy::ScopeId>,
        credential: Option<crate::CredentialAccess>,
        endpoint: crate::InferenceEndpoint,
    ) -> Self {
        Self {
            binding,
            provisioning: ModelProvisioning::Provider {
                provider_ref: provider_ref.into(),
                route_ref: route_ref.into(),
                scope_id: scope_id.into(),
                credential: credential.map(Box::new),
                endpoint: Box::new(endpoint),
            },
        }
    }

    #[must_use]
    pub fn backend_owned(
        binding: ModelBinding,
        credential: crate::CredentialRef,
        model_selection: BackendModelSelection,
        capability_adapter_version: impl Into<String>,
        capability_fingerprint: impl Into<String>,
        session_configuration: AcpSessionConfiguration,
    ) -> Self {
        Self {
            binding,
            provisioning: ModelProvisioning::BackendOwned {
                credential,
                model_selection,
                capability_adapter_version: capability_adapter_version.into(),
                capability_fingerprint: capability_fingerprint.into(),
                session_configuration,
            },
        }
    }
}

impl std::ops::Deref for ResolvedModelCandidate {
    type Target = ModelBinding;

    fn deref(&self) -> &Self::Target {
        &self.binding
    }
}

impl std::ops::DerefMut for ResolvedModelCandidate {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.binding
    }
}

impl AsRef<ModelBinding> for ResolvedModelCandidate {
    fn as_ref(&self) -> &ModelBinding {
        &self.binding
    }
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
    /// Strong semantic role used by configuration compilation. The compiler can
    /// select a delegation capability without naming a concrete builtin tool id.
    #[serde(default, skip_serializing_if = "ToolKind::is_regular")]
    pub kind: ToolKind,
    /// Execution-only recovery policy. It is never projected into the model's
    /// tool schema; the runtime validates it against the executable tool's
    /// trusted capability before any recovery action.
    #[serde(default, skip_serializing_if = "is_default_tool_recovery")]
    pub recovery_policy: crate::tool::ToolRecoveryPolicy,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    #[default]
    Regular,
    /// Declared by a protocol client. The runtime advertises it to the model but
    /// never invokes a host executor; it awaits an exact externally supplied
    /// result through the normal durable resume ticket.
    ClientExecuted,
    AgentDelegation,
}

impl ToolKind {
    const fn is_regular(&self) -> bool {
        matches!(self, Self::Regular)
    }
}

fn is_default_tool_recovery(policy: &crate::tool::ToolRecoveryPolicy) -> bool {
    policy == &crate::tool::ToolRecoveryPolicy::default()
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
            kind: ToolKind::Regular,
            recovery_policy: crate::tool::ToolRecoveryPolicy::default(),
        }
    }

    /// Build a tool whose result is owned by the calling protocol client. This
    /// is the single constructor for that execution ownership; adapters must not
    /// recreate its namespace/kind convention independently.
    pub fn client_executed(
        id: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self::pinned("client-executed", id, description, parameters)
            .with_kind(ToolKind::ClientExecuted)
    }

    /// Mark the descriptor as the one Agent-delegation capability. The role is
    /// part of its content identity even though it is not model-visible.
    #[must_use]
    pub fn with_kind(mut self, kind: ToolKind) -> Self {
        if self.kind != kind {
            self.kind = kind;
            self.content_hash = format!("{}:kind:{kind:?}", self.content_hash);
        }
        self
    }

    /// Pin an execution recovery policy without changing the model-visible
    /// schema. The policy still enters the content address so changing recovery
    /// semantics produces a different resolved snapshot.
    #[must_use]
    pub fn with_recovery(mut self, recovery: crate::tool::ToolRecoveryPolicy) -> Self {
        if self.recovery_policy == recovery {
            return self;
        }
        let encoded = serde_json::to_string(&recovery).unwrap_or_default();
        self.content_hash = format!("{}:recovery:{encoded}", self.content_hash);
        self.recovery_policy = recovery;
        self
    }
}

/// Reserved id of the meta-tool that loads a deferred tool (ADR-0053). Double-underscore
/// namespaced so it cannot collide with a catalog id or an MCP `mcp__…` id; the compile
/// alias-collision check keeps an author from minting the same facing id.
pub const TOOL_OPEN_ID: &str = "tool__open";

/// Build the reserved `tool_open` descriptor from the still-deferred tools: its
/// description lists each deferred tool's model-facing name + description so the model
/// knows what it can load, and its one argument is the `name` to load.
fn tool_open_descriptor(deferred: &[ToolDescriptor]) -> ToolDescriptor {
    let list = deferred
        .iter()
        .map(|d| format!("- {}: {}", d.id, d.description))
        .collect::<Vec<_>>()
        .join("\n");
    ToolDescriptor::pinned(
        "builtin:presentation",
        TOOL_OPEN_ID,
        format!(
            "Load a tool's full definition before you can call it. Call this with the \
             `name` of the tool you need, then call that tool on the next step. Deferred \
             tools:\n{list}"
        ),
        serde_json::json!({
            "type": "object",
            "properties": { "name": { "type": "string", "description": "The deferred tool to load." } },
            "required": ["name"]
        }),
    )
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
            .filter(|(_, f)| f.alias.is_some() || f.description.is_some() || f.defer)
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

    /// Whether the tool with this canonical id is deferred (lazy-loaded, ADR-0053).
    #[must_use]
    pub fn is_deferred(&self, canonical: &str) -> bool {
        self.facets.get(canonical).is_some_and(|f| f.defer)
    }

    /// The model-facing tool list for one step: [`present`](Self::present) applied, then
    /// each deferred tool withheld *unless* its canonical id is in `opened` (the tools
    /// the model has loaded via `tool_open` this run). When any deferred tool is still
    /// withheld, the reserved [`tool_open`](TOOL_OPEN_ID) meta-tool is appended, listing
    /// them — so the model can load a tool's full schema on demand instead of paying its
    /// tokens every step. No deferred tools ⇒ the list is exactly `present().face`.
    #[must_use]
    pub fn model_tools(
        &self,
        descriptors: &[ToolDescriptor],
        opened: &std::collections::BTreeSet<String>,
    ) -> Vec<ToolDescriptor> {
        let presented = self.present(descriptors);
        let mut face = presented.face;
        let mut withheld: Vec<ToolDescriptor> = Vec::new();
        for d in presented.deferred {
            // `d.id` is the model-facing (possibly aliased) id; `opened` keys on canonical.
            if opened.contains(self.resolve(&d.id)) {
                face.push(d);
            } else {
                withheld.push(d);
            }
        }
        if !withheld.is_empty() {
            face.push(tool_open_descriptor(&withheld));
        }
        face
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
/// in-memory `Value` shape, and SHA-256 so the digest is portable across
/// processes and Rust versions — this value is persisted in the snapshot and
/// re-checked fail-closed, matching the `sha256` the catalog fingerprint uses.
fn content_hash(
    prefix: &str,
    id: &str,
    description: &str,
    parameters: &serde_json::Value,
) -> String {
    use sha2::{Digest, Sha256};
    let canonical = serde_json::to_string(parameters).unwrap_or_default();
    let mut hasher = Sha256::new();
    // Length-prefix each field so `(id, description)` and `(id+description, "")`
    // cannot collide by concatenation.
    for field in [id, description, canonical.as_str()] {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    let digest = hasher.finalize();
    // 16 hex chars (64 bits) keeps the id readable while a schema change still
    // moves the digest; the full prefix keeps owner namespacing.
    format!(
        "{prefix}:{id}:{:016x}",
        u64::from_le_bytes(digest[..8].try_into().unwrap())
    )
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedRun {
    pub snapshot_id: crate::snapshot::ExecutableAgentSnapshotId,
    pub agent_id: crate::snapshot::AgentId,
    pub spec: ResolvedSpec,
}

#[cfg(test)]
mod tests {
    use super::{
        Backend, BackendModelSelection, ContextPolicy, ModelBinding, ResolvedModelCandidate,
        ResolvedSpec, ToolDescriptor, ToolFacet, ToolPresentation,
    };

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
            (
                "a".to_string(),
                ToolFacet {
                    alias: Some("say".into()),
                    description: Some("Speak.".into()),
                    defer: false,
                },
            ),
            (
                "mcp__x__y".to_string(),
                ToolFacet {
                    alias: Some("y".into()),
                    description: None,
                    defer: true,
                },
            ),
            ("noop".to_string(), ToolFacet::default()), // all-default ⇒ dropped
        ]);
        assert!(!p.is_empty());
        let out = p.present(&[td("a"), td("mcp__x__y"), td("keep")]);
        // `a` renamed + redescribed and stays in the face; `keep` passes through.
        assert!(
            out.face
                .iter()
                .any(|d| d.id == "say" && d.description == "Speak.")
        );
        assert!(out.face.iter().any(|d| d.id == "keep"));
        // The MCP tool is deferred (renamed) — withheld from the face.
        assert!(out.face.iter().all(|d| d.id != "y"));
        assert!(out.deferred.iter().any(|d| d.id == "y"));
    }

    #[test]
    fn model_tools_withholds_a_deferred_tool_until_opened_and_offers_tool_open() {
        use super::TOOL_OPEN_ID;
        let p = ToolPresentation::from_facets([(
            "mcp__srv__a".to_string(),
            ToolFacet {
                alias: Some("create_issue".into()),
                description: None,
                defer: true,
            },
        )]);
        let tools = [td("mcp__srv__a"), td("keep")];

        // Nothing opened: the deferred tool is withheld; tool_open is offered.
        let closed = std::collections::BTreeSet::new();
        let face = p.model_tools(&tools, &closed);
        let ids: Vec<&str> = face.iter().map(|d| d.id.as_str()).collect();
        assert!(ids.contains(&"keep"));
        assert!(ids.contains(&TOOL_OPEN_ID));
        assert!(!ids.contains(&"create_issue"), "deferred tool withheld");

        // Opened (by canonical id): the tool appears, and tool_open is gone.
        let opened: std::collections::BTreeSet<String> = ["mcp__srv__a".to_string()].into();
        let ids2: Vec<String> = p
            .model_tools(&tools, &opened)
            .iter()
            .map(|d| d.id.clone())
            .collect();
        assert!(ids2.contains(&"create_issue".to_string()));
        assert!(
            !ids2.iter().any(|i| i == TOOL_OPEN_ID),
            "no deferred left ⇒ no tool_open"
        );
    }

    #[test]
    fn resolve_reverses_an_alias_to_its_canonical_id() {
        let p = ToolPresentation::from_facets([(
            "mcp__x__y".to_string(),
            ToolFacet {
                alias: Some("y".into()),
                ..Default::default()
            },
        )]);
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
    fn backend_owned_candidate_serializes_only_identity_and_model_policy() {
        // Cause graph: exact Worker-local reference + explicit model policy ->
        // immutable BackendOwned candidate. Endpoint and material have no fields.
        //
        // Decision table: Default and Exact both round-trip; changing the policy
        // changes the snapshot data without inventing a model-id sentinel.
        for selection in [BackendModelSelection::Default, BackendModelSelection::Exact] {
            let model = if selection == BackendModelSelection::Default {
                ""
            } else {
                "gpt-exact"
            };
            let candidate = ResolvedModelCandidate::backend_owned(
                ModelBinding::new("cred:local", model, "acp:codex"),
                crate::CredentialRef {
                    id: "cred:local".into(),
                    revision: 3,
                },
                selection,
                "test",
                "sha256:test-capability",
                Default::default(),
            );
            let wire = serde_json::to_string(&candidate).unwrap();
            assert!(!wire.contains("base_url"));
            assert!(!wire.contains("material"));
            assert_eq!(
                serde_json::from_str::<ResolvedModelCandidate>(&wire).unwrap(),
                candidate
            );
        }
    }

    #[test]
    fn execution_model_selection_is_limited_to_the_published_pool() {
        let mut spec = crate::snapshot::ExecutableAgentSnapshot::builder("agent")
            .model(ModelBinding::new("primary-id", "primary", "native"))
            .model_candidates([ModelBinding::new("fallback-id", "fallback", "native")])
            .build()
            .resolved_spec;
        let unchanged = spec.clone();

        assert!(!spec.select_execution_model("not-published"));
        assert_eq!(
            spec, unchanged,
            "a rejected selector cannot mutate the pool"
        );

        assert!(spec.select_execution_model("fallback"));
        assert_eq!(spec.model_binding.provider_identity_ref, "fallback-id");
        assert!(spec.model_candidates.is_empty());
    }

    #[test]
    fn a_legacy_spec_without_the_defaulted_fields_still_loads() {
        // The `#[serde(default)]` fields (model_candidates, plugin_config,
        // context_policy, tool_presentation) exist so a snapshot compiled before they
        // were added stays loadable. A JSON carrying only the required surface must
        // deserialize with each optional field at its documented default — the very
        // backward-compat promise those attributes make.
        let legacy = serde_json::json!({
            "catalog_fingerprint": "fp-1",
            "instructions": "be concise",
            "max_steps": 8,
            "model_binding": {
                "provider_identity_ref": "p",
                "model_ref": "m",
                "backend_ref": "genai"
            },
            "tool_descriptors": [],
            "plugin_ids": []
        });
        let spec: ResolvedSpec = serde_json::from_value(legacy).expect("legacy spec loads");
        assert!(spec.model_candidates.is_empty());
        assert!(spec.plugin_config.is_empty());
        assert!(spec.tool_presentation.is_empty());
        // An unset context policy sends the whole transcript (KeepAll), unchanged behavior.
        assert_eq!(spec.context_policy, ContextPolicy::KeepAll);
        // A single-model agent yields exactly the primary binding.
        assert_eq!(spec.candidate_bindings().len(), 1);
    }

    #[test]
    fn context_policy_wire_is_internally_tagged_snake_case_and_defaults_to_keep_all() {
        // `tag = "kind"`, `rename_all = "snake_case"`: the unit variant carries just its
        // tag, the struct variant its fields alongside.
        assert_eq!(
            serde_json::to_value(ContextPolicy::KeepAll).unwrap(),
            serde_json::json!({ "kind": "keep_all" })
        );
        assert_eq!(
            serde_json::to_value(ContextPolicy::KeepLast { keep_last: 3 }).unwrap(),
            serde_json::json!({ "kind": "keep_last", "keep_last": 3 })
        );
        // Both directions round-trip, and the derived default is KeepAll.
        let back: ContextPolicy =
            serde_json::from_value(serde_json::json!({ "kind": "keep_last", "keep_last": 0 }))
                .unwrap();
        assert_eq!(back, ContextPolicy::KeepLast { keep_last: 0 });
        assert_eq!(ContextPolicy::default(), ContextPolicy::KeepAll);
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

    #[test]
    fn content_hash_is_length_prefixed_against_field_concatenation_collisions() {
        // Without length-prefixing, ("ab","c") and ("a","bc") would concatenate to
        // the same byte stream and collide. The id is part of the readable prefix,
        // so vary the description/schema boundary where the digest actually matters.
        let a = ToolDescriptor::pinned("p", "t", "ab", serde_json::json!("c"));
        let b = ToolDescriptor::pinned("p", "t", "a", serde_json::json!("bc"));
        assert_ne!(a.content_hash, b.content_hash);
    }

    #[test]
    fn content_hash_is_deterministic_sha256_hex() {
        // Portable digest: the same inputs always yield the same 16 hex chars, and
        // the tail is valid lowercase hex (not a platform-dependent SipHash value).
        let h = ToolDescriptor::pinned("p", "t", "desc", serde_json::json!({"a": 1})).content_hash;
        let tail = h.rsplit(':').next().unwrap();
        assert_eq!(tail.len(), 16);
        assert!(
            tail.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }
}
