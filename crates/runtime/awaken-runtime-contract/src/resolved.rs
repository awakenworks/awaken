use std::collections::BTreeMap;

pub use awaken_agent_contract::AcpSessionConfiguration;
use serde::{Deserialize, Serialize};

use crate::tool_discovery::ToolSearchLimit;

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

/// Publication-pinned ACP execution evidence shared by provider-routed and
/// backend-owned launches. One shape owns both capability proof and native
/// Session intent so the two provisioning modes cannot drift.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpExecutionProfile {
    #[serde(default)]
    pub capability_fingerprint: String,
    #[serde(default)]
    pub capability_adapter_version: String,
    #[serde(default, skip_serializing_if = "AcpSessionConfiguration::is_empty")]
    pub session_configuration: AcpSessionConfiguration,
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

    /// Encode the ACP-owned fields back into an existing plugin configuration.
    ///
    /// Unknown ACP keys and every non-ACP plugin section are preserved exactly;
    /// only this codec's two owned keys are replaced or removed. This makes the
    /// typed view usable by authoring without creating a parallel settings wire.
    #[must_use]
    pub fn into_plugin_config(
        self,
        mut plugin_config: BTreeMap<String, serde_json::Value>,
    ) -> BTreeMap<String, serde_json::Value> {
        let mut acp = plugin_config
            .remove("acp")
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        match self.compact_window {
            Some(window) => {
                acp.insert("compact_window".into(), serde_json::Value::from(window));
            }
            None => {
                acp.remove("compact_window");
            }
        }
        if self.mcp_servers.is_empty() {
            // A historical or future route shape that this version cannot decode
            // remains owned by its original wire, not silently deleted. A valid
            // current list can be explicitly cleared.
            let existing_is_unknown = acp.get("mcp_servers").is_some_and(|value| {
                serde_json::from_value::<Vec<AcpMcpServer>>(value.clone()).is_err()
            });
            if !existing_is_unknown {
                acp.remove("mcp_servers");
            }
        } else {
            acp.insert(
                "mcp_servers".into(),
                serde_json::to_value(self.mcp_servers)
                    .expect("AcpMcpServer is an infallible JSON value"),
            );
        }
        if !acp.is_empty() {
            plugin_config.insert("acp".into(), serde_json::Value::Object(acp));
        }
        plugin_config
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
    /// Immutable provider realization of a processing-geography requirement.
    /// Absence means this route makes no additional processing guarantee.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processing_placement: Option<InferencePlacement>,
}

/// Closed provider behavior for an otherwise unspecified reasoning request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnspecifiedReasoning {
    #[default]
    ProviderDefault,
    Disabled,
}

impl UnspecifiedReasoning {
    #[must_use]
    const fn is_provider_default(&self) -> bool {
        matches!(self, Self::ProviderDefault)
    }
}

/// Provider-route execution facts that are orthogonal to endpoint and
/// credential selection. Grouping them prevents constructors from expressing
/// this one policy as an error-prone sequence of positional arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderExecutionProfile {
    pub unspecified_reasoning: UnspecifiedReasoning,
    pub acp: Option<AcpExecutionProfile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferencePlacement {
    pub geography: crate::agent_bindings::InferenceGeography,
    pub mechanism: InferencePlacementMechanism,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferencePlacementMechanism {
    /// The provider-native request body must carry the trusted geography.
    AnthropicRequestBody,
    /// The exact frozen endpoint, deployment, or inference-profile model id
    /// already enforces the geography, so the provider body stays unchanged.
    FrozenRegionalRoute,
}

impl InferencePlacementMechanism {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicRequestBody => "anthropic_request_body",
            Self::FrozenRegionalRoute => "frozen_regional_route",
        }
    }
}

impl std::str::FromStr for InferencePlacementMechanism {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "anthropic_request_body" => Ok(Self::AnthropicRequestBody),
            "frozen_regional_route" => Ok(Self::FrozenRegionalRoute),
            other => Err(format!(
                "unsupported inference placement mechanism `{other}`"
            )),
        }
    }
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
    /// appearance/exposure override, keyed by canonical id (catalog or MCP). Empty for
    /// an agent with no overrides — the tool face is then byte-identical to before, so
    /// `#[serde(default)]` keeps the 40+ existing snapshot constructions loadable.
    #[serde(default)]
    pub tool_presentation: ToolPresentation,
}

impl ResolvedSpec {
    /// Return one complete publication-pinned route by stable primary/fallback
    /// ordinal. The selector returns the original candidate by reference, so no
    /// binding or provisioning axis can be reconstructed from a different route.
    #[must_use]
    pub fn pinned_model_candidate_at(&self, ordinal: usize) -> Option<&ResolvedModelCandidate> {
        crate::model_routing::pinned_candidate_at(
            &self.model_binding,
            &self.model_candidates,
            ordinal,
        )
    }

    /// Ordered published candidates eligible for one execution request. A
    /// nonblank override scopes by model while retaining every published route
    /// for that model; absent/blank retains primary plus all fallbacks.
    #[must_use]
    pub fn execution_candidates(
        &self,
        model_ref_override: Option<&str>,
    ) -> Vec<&ResolvedModelCandidate> {
        let selected = model_ref_override.filter(|model| !model.is_empty());
        (0..=self.model_candidates.len())
            .filter_map(|ordinal| self.pinned_model_candidate_at(ordinal))
            .filter(|candidate| selected.is_none_or(|model| candidate.binding.model_ref == model))
            .collect()
    }

    /// Complete candidate set whose credentials and executor routes must be
    /// realized for one attempt. The advisor is not a model-pool fallback, but
    /// it executes inside the same attempt and therefore shares the attempt's
    /// claim fence and credential evidence. An identical advisor binding is
    /// de-duplicated; publication rejects a same-binding/different-route pair.
    #[must_use]
    pub fn attempt_candidates(
        &self,
        model_ref_override: Option<&str>,
    ) -> Vec<&ResolvedModelCandidate> {
        let mut candidates = self.execution_candidates(model_ref_override);
        let advisor = self
            .plugin_config
            .agent
            .advisor
            .as_ref()
            .map(|advisor| &advisor.candidate);
        let advisor_is_duplicate = advisor.is_some_and(|advisor| {
            candidates
                .iter()
                .any(|candidate| candidate.binding == advisor.binding)
        });
        if advisor_candidate_is_admitted(
            !candidates.is_empty(),
            advisor.is_some(),
            advisor_is_duplicate,
        ) {
            candidates.push(advisor.expect("advisor admission requires a candidate"));
        }
        candidates
    }

    /// The ordered model bindings this run may use: the primary
    /// [`model_binding`](Self::model_binding) first, then any pool fallbacks in
    /// [`model_candidates`](Self::model_candidates). A single-model agent yields
    /// exactly one. The engine tries them in order, failing over to the next only
    /// on a clean pre-commit failure of the current one (never mid-stream).
    #[must_use]
    pub fn candidate_bindings(&self) -> Vec<&ModelBinding> {
        (0..=self.model_candidates.len())
            .filter_map(|ordinal| self.pinned_model_candidate_at(ordinal))
            .map(|candidate| &candidate.binding)
            .collect()
    }

    /// The complete publication-pinned candidate for `binding`. Provisioning uses
    /// this exact lookup so two routes for the same upstream model cannot share
    /// credential or endpoint state accidentally.
    #[must_use]
    pub fn candidate_for_binding(&self, binding: &ModelBinding) -> Option<&ResolvedModelCandidate> {
        std::iter::once(&self.model_binding)
            .chain(self.model_candidates.iter())
            .chain(
                self.plugin_config
                    .agent
                    .advisor
                    .as_ref()
                    .map(|advisor| &advisor.candidate),
            )
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

/// Representation-free admission relation for the auxiliary Advisor route.
/// An Advisor never creates primary admission and an identical binding never
/// spends a second credential/claim slot.
#[must_use]
const fn advisor_candidate_is_admitted(
    primary_admitted: bool,
    advisor_present: bool,
    advisor_is_duplicate: bool,
) -> bool {
    primary_admitted && advisor_present && !advisor_is_duplicate
}

#[cfg(kani)]
#[kani::proof]
fn advisor_never_substitutes_for_primary_model_admission() {
    let primary_admitted = kani::any();
    let advisor_present = kani::any();
    let advisor_is_duplicate = kani::any();
    assert_eq!(
        advisor_candidate_is_admitted(primary_admitted, advisor_present, advisor_is_duplicate),
        primary_admitted && advisor_present && !advisor_is_duplicate
    );
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
    /// A launched external ACP CLI (Claude Code, Codex, …).
    Acp(AcpBackend),
    /// A remote agent reached over A2A HTTP (no local process). `endpoint` is the
    /// dial URL parsed from `a2a:<endpoint>`; the A2A executor consumes it.
    Remote(A2aBackend),
    /// A malformed or incomplete route. It is a parse outcome, never an
    /// executable backend, so every exhaustive routing decision must fail it.
    Invalid(InvalidBackendRef),
}

/// Exact ACP executor coordinate parsed from `acp:<cli>`.
/// Its payload is private, so an empty CLI cannot be constructed.
///
/// ```compile_fail
/// use awaken_runtime_contract::resolved::AcpBackend;
/// let _ = AcpBackend(String::new());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpBackend(String);

impl AcpBackend {
    pub fn parse(backend_ref: impl Into<String>) -> Result<Self, InvalidBackendRef> {
        let backend_ref = backend_ref.into();
        match Backend::from_ref(&backend_ref) {
            Backend::Acp(backend) => Ok(backend),
            _ => Err(InvalidBackendRef(backend_ref)),
        }
    }

    #[must_use]
    pub fn cli(&self) -> &str {
        &self.0["acp:".len()..]
    }

    #[must_use]
    pub fn backend_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AcpBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.cli())
    }
}

/// Exact HTTP(S) A2A endpoint parsed from `a2a:<url>`.
/// Its payload is private, so an empty or non-network endpoint cannot exist.
///
/// ```compile_fail
/// use awaken_runtime_contract::resolved::A2aBackend;
/// let _ = A2aBackend("relative/path".into());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aBackend(String);

impl A2aBackend {
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for A2aBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Exact rejected backend spelling. The payload is diagnostic-only and private.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidBackendRef(String);

impl InvalidBackendRef {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for InvalidBackendRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "invalid backend reference `{}`", self.0)
    }
}

impl std::error::Error for InvalidBackendRef {}

impl Backend {
    /// Classify and validate a `backend_ref`. The result is total so inspection
    /// code can retain the rejected spelling, while runnable variants themselves
    /// carry only valid payloads.
    #[must_use]
    pub fn from_ref(backend_ref: &str) -> Self {
        if let Some(cli) = backend_ref.strip_prefix("acp:") {
            if !cli.is_empty() && cli.trim() == cli {
                Backend::Acp(AcpBackend(backend_ref.to_string()))
            } else {
                Backend::Invalid(InvalidBackendRef(backend_ref.to_string()))
            }
        } else if let Some(endpoint) = backend_ref.strip_prefix("a2a:") {
            let valid = url::Url::parse(endpoint).is_ok_and(|url| {
                matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
            });
            if valid {
                Backend::Remote(A2aBackend(endpoint.to_string()))
            } else {
                Backend::Invalid(InvalidBackendRef(backend_ref.to_string()))
            }
        } else if backend_ref.is_empty()
            || backend_ref.trim() != backend_ref
            || backend_ref == "acp"
        {
            Backend::Invalid(InvalidBackendRef(backend_ref.to_string()))
        } else {
            Backend::Native
        }
    }

    /// The remote dial endpoint if this is an A2A backend.
    #[must_use]
    pub fn remote_endpoint(&self) -> Option<&str> {
        match self {
            Backend::Remote(endpoint) => Some(endpoint.endpoint()),
            _ => None,
        }
    }

    /// Whether this run is served by an external ACP CLI rather than the native loop.
    #[must_use]
    pub fn is_acp(&self) -> bool {
        matches!(self, Backend::Acp(_))
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

/// A model coordinate proven non-empty and free of accidental surrounding
/// whitespace. The private payload prevents an `Exact` policy from carrying no
/// exact model at all.
///
/// ```compile_fail
/// use awaken_runtime_contract::resolved::ExactModelRef;
/// let _ = ExactModelRef(String::new());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExactModelRef(String);

impl ExactModelRef {
    pub fn parse(model_ref: impl Into<String>) -> Result<Self, &'static str> {
        let model_ref = model_ref.into();
        if model_ref.is_empty() || model_ref.trim() != model_ref {
            return Err(
                "exact model reference must be non-empty and contain no surrounding whitespace",
            );
        }
        Ok(Self(model_ref))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ExactModelRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// How one exact published Provider candidate obtains access. This is a
/// provider-neutral execution fact: it is neither catalog provenance nor a
/// commercial funding decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAccessKind {
    /// Call the published endpoint directly, optionally using the exact
    /// Workspace credential frozen beside the candidate.
    Direct,
    /// Ask the installed broker to authorize and materialize the exact route.
    Brokered,
}

/// The provisioning facts for one published model candidate. This is snapshot
/// data, not secret material and not an IAM decision. Local endpoints, gateways,
/// and provider SaaS use `Provider`; a trusted local ACP agent uses
/// `BackendOwned`; a remote A2A agent uses `Remote`; only an explicitly
/// installed in-process executor uses `HostExecutor`.
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
        #[serde(flatten)]
        acp: AcpExecutionProfile,
    },
    /// Exact provider route and credential delivery frozen by publication.
    Provider {
        provider_ref: String,
        route_ref: String,
        /// Explicit access-source axis. Credential presence and route naming
        /// are never interpreted as access-source evidence.
        access_kind: ProviderAccessKind,
        /// Opaque ownership coordinate for the pinned credential. It denotes a
        /// Workspace today but deliberately carries no action/capability: scope
        /// range and authorization function remain orthogonal.
        scope_id: awaken_tenancy::ScopeId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential: Option<Box<crate::CredentialAccess>>,
        endpoint: Box<crate::InferenceEndpoint>,
        /// Explicit behavior when an Agent leaves reasoning effort unspecified.
        /// Provider-specific wire fields are derived by the transport adapter;
        /// raw JSON never crosses this immutable route boundary.
        #[serde(
            default,
            skip_serializing_if = "UnspecifiedReasoning::is_provider_default"
        )]
        unspecified_reasoning: UnspecifiedReasoning,
        /// Present only when an external ACP executor consumes this Provider
        /// route. Native execution leaves it absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        acp: Option<Box<AcpExecutionProfile>>,
    },
    /// Exact remote-counterparty credential delivery frozen by publication.
    ///
    /// The endpoint remains the executor-axis identity in `binding.backend_ref`;
    /// this variant owns only the transport-auth realization facts. An absent
    /// credential is valid only when publication proved that the discovered
    /// Agent Card permits anonymous access.
    Remote {
        /// Opaque ownership coordinate for the pinned credential.
        scope_id: awaken_tenancy::ScopeId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential: Option<Box<crate::CredentialAccess>>,
        /// Fingerprint of the Agent Card security declaration accepted at
        /// publication. Launch-time discovery must match it before dialing.
        security_fingerprint: String,
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
///
/// ```compile_fail
/// use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate};
/// let mut candidate = ResolvedModelCandidate::host(ModelBinding::new("p", "m", "native"));
/// candidate.binding.backend_ref = "acp:codex".into();
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedModelCandidate {
    #[serde(flatten)]
    binding: ModelBinding,
    #[serde(default, skip_serializing_if = "ModelProvisioning::is_host_executor")]
    provisioning: ModelProvisioning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidResolvedModelCandidate(&'static str);

impl std::fmt::Display for InvalidResolvedModelCandidate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for InvalidResolvedModelCandidate {}

#[derive(Deserialize)]
struct ResolvedModelCandidateWire {
    #[serde(flatten)]
    binding: ModelBinding,
    #[serde(default)]
    provisioning: ModelProvisioning,
}

impl<'de> Deserialize<'de> for ResolvedModelCandidate {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = ResolvedModelCandidateWire::deserialize(deserializer)?;
        Self::try_from_parts(wire.binding, wire.provisioning).map_err(serde::de::Error::custom)
    }
}

impl ResolvedModelCandidate {
    #[must_use]
    pub fn host(binding: ModelBinding) -> Self {
        Self {
            binding,
            provisioning: ModelProvisioning::HostExecutor,
        }
    }

    pub fn try_provider(
        binding: ModelBinding,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        scope_id: impl Into<awaken_tenancy::ScopeId>,
        credential: Option<crate::CredentialAccess>,
        endpoint: crate::InferenceEndpoint,
    ) -> Result<Self, InvalidResolvedModelCandidate> {
        Self::try_provider_with_reasoning(
            binding,
            provider_ref,
            route_ref,
            scope_id,
            credential,
            endpoint,
            UnspecifiedReasoning::ProviderDefault,
        )
    }

    pub fn try_provider_with_reasoning(
        binding: ModelBinding,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        scope_id: impl Into<awaken_tenancy::ScopeId>,
        credential: Option<crate::CredentialAccess>,
        endpoint: crate::InferenceEndpoint,
        unspecified_reasoning: UnspecifiedReasoning,
    ) -> Result<Self, InvalidResolvedModelCandidate> {
        Self::try_provider_with_profile(
            binding,
            provider_ref,
            route_ref,
            scope_id,
            credential,
            endpoint,
            ProviderExecutionProfile {
                unspecified_reasoning,
                acp: None,
            },
        )
    }

    /// Build one broker-authorized Provider candidate. Brokered publications
    /// never carry a Workspace credential; the installed broker freezes the
    /// exact downstream credential only while authorizing an attempt.
    pub fn try_brokered_provider(
        binding: ModelBinding,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        scope_id: impl Into<awaken_tenancy::ScopeId>,
        endpoint: crate::InferenceEndpoint,
    ) -> Result<Self, InvalidResolvedModelCandidate> {
        Self::try_brokered_provider_with_reasoning(
            binding,
            provider_ref,
            route_ref,
            scope_id,
            endpoint,
            UnspecifiedReasoning::ProviderDefault,
        )
    }

    pub fn try_brokered_provider_with_reasoning(
        binding: ModelBinding,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        scope_id: impl Into<awaken_tenancy::ScopeId>,
        endpoint: crate::InferenceEndpoint,
        unspecified_reasoning: UnspecifiedReasoning,
    ) -> Result<Self, InvalidResolvedModelCandidate> {
        Self::try_brokered_provider_with_profile(
            binding,
            provider_ref,
            route_ref,
            scope_id,
            endpoint,
            ProviderExecutionProfile {
                unspecified_reasoning,
                acp: None,
            },
        )
    }

    pub fn try_brokered_provider_with_acp(
        binding: ModelBinding,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        scope_id: impl Into<awaken_tenancy::ScopeId>,
        endpoint: crate::InferenceEndpoint,
        acp: AcpExecutionProfile,
    ) -> Result<Self, InvalidResolvedModelCandidate> {
        Self::try_brokered_provider_with_profile(
            binding,
            provider_ref,
            route_ref,
            scope_id,
            endpoint,
            ProviderExecutionProfile {
                unspecified_reasoning: UnspecifiedReasoning::ProviderDefault,
                acp: Some(acp),
            },
        )
    }

    pub fn try_brokered_provider_with_profile(
        binding: ModelBinding,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        scope_id: impl Into<awaken_tenancy::ScopeId>,
        endpoint: crate::InferenceEndpoint,
        profile: ProviderExecutionProfile,
    ) -> Result<Self, InvalidResolvedModelCandidate> {
        Self::try_from_parts(
            binding,
            ModelProvisioning::Provider {
                provider_ref: provider_ref.into(),
                route_ref: route_ref.into(),
                access_kind: ProviderAccessKind::Brokered,
                scope_id: scope_id.into(),
                credential: None,
                endpoint: Box::new(endpoint),
                unspecified_reasoning: profile.unspecified_reasoning,
                acp: profile.acp.map(Box::new),
            },
        )
    }

    pub fn try_provider_with_acp(
        binding: ModelBinding,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        scope_id: impl Into<awaken_tenancy::ScopeId>,
        credential: Option<crate::CredentialAccess>,
        endpoint: crate::InferenceEndpoint,
        acp: AcpExecutionProfile,
    ) -> Result<Self, InvalidResolvedModelCandidate> {
        Self::try_provider_with_profile(
            binding,
            provider_ref,
            route_ref,
            scope_id,
            credential,
            endpoint,
            ProviderExecutionProfile {
                unspecified_reasoning: UnspecifiedReasoning::ProviderDefault,
                acp: Some(acp),
            },
        )
    }

    pub fn try_provider_with_profile(
        binding: ModelBinding,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        scope_id: impl Into<awaken_tenancy::ScopeId>,
        credential: Option<crate::CredentialAccess>,
        endpoint: crate::InferenceEndpoint,
        profile: ProviderExecutionProfile,
    ) -> Result<Self, InvalidResolvedModelCandidate> {
        Self::try_from_parts(
            binding,
            ModelProvisioning::Provider {
                provider_ref: provider_ref.into(),
                route_ref: route_ref.into(),
                access_kind: ProviderAccessKind::Direct,
                scope_id: scope_id.into(),
                credential: credential.map(Box::new),
                endpoint: Box::new(endpoint),
                unspecified_reasoning: profile.unspecified_reasoning,
                acp: profile.acp.map(Box::new),
            },
        )
    }

    pub fn try_backend_owned(
        binding: ModelBinding,
        credential: crate::CredentialRef,
        model_selection: BackendModelSelection,
        capability_adapter_version: impl Into<String>,
        capability_fingerprint: impl Into<String>,
        session_configuration: AcpSessionConfiguration,
    ) -> Result<Self, InvalidResolvedModelCandidate> {
        Self::try_from_parts(
            binding,
            ModelProvisioning::BackendOwned {
                credential,
                model_selection,
                acp: AcpExecutionProfile {
                    capability_adapter_version: capability_adapter_version.into(),
                    capability_fingerprint: capability_fingerprint.into(),
                    session_configuration,
                },
            },
        )
    }

    /// Build one publication-pinned A2A transport demand.
    pub fn try_remote(
        binding: ModelBinding,
        scope_id: impl Into<awaken_tenancy::ScopeId>,
        credential: Option<crate::CredentialAccess>,
        security_fingerprint: impl Into<String>,
    ) -> Result<Self, InvalidResolvedModelCandidate> {
        Self::try_from_parts(
            binding,
            ModelProvisioning::Remote {
                scope_id: scope_id.into(),
                credential: credential.map(Box::new),
                security_fingerprint: security_fingerprint.into(),
            },
        )
    }

    pub fn try_from_parts(
        binding: ModelBinding,
        provisioning: ModelProvisioning,
    ) -> Result<Self, InvalidResolvedModelCandidate> {
        fn canonical_nonempty(value: &str) -> bool {
            !value.is_empty() && value.trim() == value
        }

        let backend = Backend::from_ref(&binding.backend_ref);
        match &provisioning {
            ModelProvisioning::HostExecutor => {}
            ModelProvisioning::BackendOwned {
                credential,
                model_selection,
                acp,
            } => {
                if !matches!(backend, Backend::Acp(_)) {
                    return Err(InvalidResolvedModelCandidate(
                        "backend-owned provisioning requires an exact ACP backend",
                    ));
                }
                if !canonical_nonempty(&binding.provider_identity_ref)
                    || !canonical_nonempty(&credential.id)
                {
                    return Err(InvalidResolvedModelCandidate(
                        "backend-owned provisioning requires exact identity and credential references",
                    ));
                }
                let coherent_model = match model_selection {
                    BackendModelSelection::Default => binding.model_ref.is_empty(),
                    BackendModelSelection::Exact => {
                        ExactModelRef::parse(binding.model_ref.clone()).is_ok()
                    }
                };
                if !coherent_model {
                    return Err(InvalidResolvedModelCandidate(
                        "backend model selection and model reference are incoherent",
                    ));
                }
                if !canonical_nonempty(&acp.capability_adapter_version)
                    || !canonical_nonempty(&acp.capability_fingerprint)
                {
                    return Err(InvalidResolvedModelCandidate(
                        "ACP provisioning requires an exact capability pin",
                    ));
                }
            }
            ModelProvisioning::Provider {
                provider_ref,
                route_ref,
                access_kind,
                credential,
                endpoint,
                acp,
                ..
            } => {
                if !canonical_nonempty(&binding.provider_identity_ref)
                    || !canonical_nonempty(&binding.model_ref)
                    || !canonical_nonempty(provider_ref)
                    || !canonical_nonempty(route_ref)
                    || !canonical_nonempty(&endpoint.adapter_kind)
                    || !canonical_nonempty(&endpoint.api_dialect)
                    || !canonical_nonempty(&endpoint.base_url)
                    || !canonical_nonempty(&endpoint.upstream_model)
                {
                    return Err(InvalidResolvedModelCandidate(
                        "provider provisioning requires complete canonical route coordinates",
                    ));
                }
                let backend_matches = matches!(
                    (&backend, acp),
                    (Backend::Native, None) | (Backend::Acp(_), Some(_))
                );
                if !backend_matches {
                    return Err(InvalidResolvedModelCandidate(
                        "provider provisioning and executor backend are incoherent",
                    ));
                }
                if let Some(acp) = acp
                    && (!canonical_nonempty(&acp.capability_adapter_version)
                        || !canonical_nonempty(&acp.capability_fingerprint))
                {
                    return Err(InvalidResolvedModelCandidate(
                        "ACP provider provisioning requires an exact capability pin",
                    ));
                }
                if *access_kind == ProviderAccessKind::Brokered && credential.is_some() {
                    return Err(InvalidResolvedModelCandidate(
                        "brokered provider provisioning cannot carry a Workspace credential",
                    ));
                }
            }
            ModelProvisioning::Remote {
                security_fingerprint,
                ..
            } => {
                if !matches!(backend, Backend::Remote(_)) || !binding.model_ref.is_empty() {
                    return Err(InvalidResolvedModelCandidate(
                        "remote provisioning requires an exact A2A backend and no local model",
                    ));
                }
                if !canonical_nonempty(security_fingerprint) {
                    return Err(InvalidResolvedModelCandidate(
                        "remote provisioning requires an exact security fingerprint",
                    ));
                }
            }
        }
        Ok(Self {
            binding,
            provisioning,
        })
    }

    #[must_use]
    pub fn binding(&self) -> &ModelBinding {
        &self.binding
    }

    #[must_use]
    pub fn provisioning(&self) -> &ModelProvisioning {
        &self.provisioning
    }
}

impl std::ops::Deref for ResolvedModelCandidate {
    type Target = ModelBinding;

    fn deref(&self) -> &Self::Target {
        &self.binding
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

/// A normalized model-visible tool input schema. Construction and deserialization
/// share one validation boundary, so an invalid root shape cannot enter a
/// [`ToolDescriptor`] and provider adapters need no defensive normalization.
///
/// ```compile_fail
/// use awaken_runtime_contract::resolved::ToolSchema;
///
/// let _ = ToolSchema(serde_json::json!({"type": "string"}));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ToolSchema(serde_json::Value);

impl<'de> Deserialize<'de> for ToolSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        normalize_model_tool_schema(value)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

impl std::ops::Deref for ToolSchema {
    type Target = serde_json::Value;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Model-visible tool identity pinned in the resolved spec. The runtime projects
/// `id`/`description`/`parameters` into the inference request. Content identity
/// is derived from the current complete state, never stored as a second mutable
/// fact. It carries no executable handle — authority lives behind the gate and
/// `ToolExecutor`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolDescriptor {
    content_namespace: String,
    pub id: String,
    /// Natural-language description shown to the model.
    pub description: String,
    /// JSON Schema for the tool arguments. The executing side validates calls
    /// against this; an empty object means "no declared parameters".
    pub parameters: ToolSchema,
    /// Strong semantic role used by configuration compilation. The compiler can
    /// select a delegation capability without naming a concrete builtin tool id.
    #[serde(default, skip_serializing_if = "ToolKind::is_regular")]
    pub kind: ToolKind,
    /// Execution-only recovery policy. It is never projected into the model's
    /// tool schema; the runtime validates it against the executable tool's
    /// trusted capability before any recovery action.
    #[serde(default, skip_serializing_if = "is_default_tool_recovery")]
    pub recovery_policy: crate::tool::ToolRecoveryPolicy,
    /// Optional provider-native realization of this same builtin capability.
    /// The canonical tool id remains the Agent-facing identity; an exact
    /// provider adapter may project it to this server-tool wire instead of a
    /// function. Absence always means ordinary host execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_server_tool: Option<ProviderServerTool>,
}

/// Exact provider-native server-tool projection selected during publication.
///
/// This is deliberately a closed capability rather than three free strings and
/// an arbitrary JSON object. Adding another provider-owned tool therefore
/// requires an explicit domain variant and an adapter projection; a compatible
/// endpoint can never acquire provider execution merely by choosing a spelling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderServerTool {
    OpenRouterToolSearch {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_results: Option<ToolSearchLimit>,
    },
    OpenRouterWebSearch {
        #[serde(
            default,
            skip_serializing_if = "OpenRouterWebSearchParameters::is_empty"
        )]
        parameters: OpenRouterWebSearchParameters,
    },
    OpenRouterWebFetch {
        #[serde(
            default,
            skip_serializing_if = "OpenRouterWebFetchParameters::is_empty"
        )]
        parameters: OpenRouterWebFetchParameters,
    },
}

impl ProviderServerTool {
    /// OpenRouter's provider-owned regex Tool Search. `None` preserves the
    /// provider default (currently five results).
    #[must_use]
    pub const fn openrouter_tool_search(max_results: Option<ToolSearchLimit>) -> Self {
        Self::OpenRouterToolSearch { max_results }
    }

    #[must_use]
    pub const fn openrouter_web_search(parameters: OpenRouterWebSearchParameters) -> Self {
        Self::OpenRouterWebSearch { parameters }
    }

    #[must_use]
    pub const fn openrouter_web_fetch(parameters: OpenRouterWebFetchParameters) -> Self {
        Self::OpenRouterWebFetch { parameters }
    }

    #[must_use]
    pub const fn provider_kind(&self) -> &'static str {
        match self {
            Self::OpenRouterToolSearch { .. }
            | Self::OpenRouterWebSearch { .. }
            | Self::OpenRouterWebFetch { .. } => "openrouter",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenRouterSearchEngine {
    Auto,
    Native,
    Exa,
    Firecrawl,
    Parallel,
    Perplexity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenRouterFetchEngine {
    Auto,
    Native,
    Exa,
    Openrouter,
    Firecrawl,
    Parallel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenRouterSearchContextSize {
    Low,
    Medium,
    High,
}

/// Strongly typed OpenRouter Web Search configuration. Unknown extension keys
/// fail at publication rather than crossing the domain as arbitrary JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenRouterWebSearchParameters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<OpenRouterSearchEngine>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<std::num::NonZeroU32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_results: Option<std::num::NonZeroU32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_context_size: Option<OpenRouterSearchContextSize>,
}

impl OpenRouterWebSearchParameters {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// Strongly typed OpenRouter Web Fetch configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenRouterWebFetchParameters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<OpenRouterFetchEngine>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<std::num::NonZeroU32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_content_tokens: Option<std::num::NonZeroU32>,
}

impl OpenRouterWebFetchParameters {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Deserialize)]
struct ToolDescriptorWire {
    #[serde(default)]
    content_namespace: Option<String>,
    #[serde(default)]
    content_hash: Option<String>,
    id: String,
    description: String,
    parameters: ToolSchema,
    #[serde(default)]
    kind: ToolKind,
    #[serde(default)]
    recovery_policy: crate::tool::ToolRecoveryPolicy,
    #[serde(default)]
    provider_server_tool: Option<ProviderServerTool>,
}

impl<'de> Deserialize<'de> for ToolDescriptor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ToolDescriptorWire::deserialize(deserializer)?;
        let content_namespace = match wire.content_namespace {
            Some(namespace) => namespace,
            None => legacy_tool_content_namespace(
                wire.content_hash.as_deref().unwrap_or_default(),
                &wire.id,
            )
            .ok_or_else(|| serde::de::Error::missing_field("content_namespace"))?,
        };
        Ok(Self {
            content_namespace,
            id: wire.id,
            description: wire.description,
            parameters: wire.parameters,
            kind: wire.kind,
            recovery_policy: wire.recovery_policy,
            provider_server_tool: wire.provider_server_tool,
        })
    }
}

fn legacy_tool_content_namespace(content_hash: &str, id: &str) -> Option<String> {
    let marker = format!(":{id}:");
    content_hash.match_indices(&marker).find_map(|(index, _)| {
        let identity = &content_hash[index + marker.len()..];
        let digest = identity.get(..16)?;
        let semantic_tail = identity.get(16..)?;
        (digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            && (semantic_tail.is_empty()
                || semantic_tail.starts_with(":kind:")
                || semantic_tail.starts_with(":recovery:")))
        .then(|| content_hash[..index].to_owned())
    })
}

#[derive(Deserialize)]
struct ToolDescriptorWire {
    #[serde(default)]
    content_namespace: Option<String>,
    #[serde(default)]
    content_hash: Option<String>,
    id: String,
    description: String,
    parameters: ToolSchema,
    #[serde(default)]
    kind: ToolKind,
    #[serde(default)]
    recovery_policy: crate::tool::ToolRecoveryPolicy,
}

impl<'de> Deserialize<'de> for ToolDescriptor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ToolDescriptorWire::deserialize(deserializer)?;
        let content_namespace = match wire.content_namespace {
            Some(namespace) => namespace,
            None => legacy_tool_content_namespace(
                wire.content_hash.as_deref().unwrap_or_default(),
                &wire.id,
            )
            .ok_or_else(|| serde::de::Error::missing_field("content_namespace"))?,
        };
        Ok(Self {
            content_namespace,
            id: wire.id,
            description: wire.description,
            parameters: wire.parameters,
            kind: wire.kind,
            recovery_policy: wire.recovery_policy,
        })
    }
}

fn legacy_tool_content_namespace(content_hash: &str, id: &str) -> Option<String> {
    let marker = format!(":{id}:");
    content_hash.match_indices(&marker).find_map(|(index, _)| {
        let identity = &content_hash[index + marker.len()..];
        let digest = identity.get(..16)?;
        let semantic_tail = identity.get(16..)?;
        (digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            && (semantic_tail.is_empty()
                || semantic_tail.starts_with(":kind:")
                || semantic_tail.starts_with(":recovery:")))
        .then(|| content_hash[..index].to_owned())
    })
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
    /// Model-facing consultation capability executed through the runtime's
    /// durable [`RunDelegationService`](crate::delegation::RunDelegationService)
    /// against the publication-pinned advisor candidate, never a host `RawTool`
    /// or a second inline model path.
    Advisor,
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
    /// Generate the descriptor of a compiled typed tool. `Tool::Args` is the
    /// only input-schema authority: callers provide neither JSON nor a repeated
    /// id/description, so the executable and advertised contracts cannot drift.
    pub fn for_tool<T: crate::tool::Tool>(prefix: &str) -> Self {
        let parameters = model_tool_schema::<T::Args>();
        Self::pinned(prefix, T::ID, T::DESCRIPTION, parameters)
    }

    /// Generate a descriptor for a typed model-visible capability that is not
    /// executed as a [`Tool`](crate::tool::Tool), such as Agent delegation.
    /// The capability still cannot supply a hand-written parameter schema.
    pub fn for_args<A: schemars::JsonSchema>(
        prefix: &str,
        id: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        let parameters = model_tool_schema::<A>();
        Self::pinned(prefix, id, description, parameters)
    }

    /// Build a descriptor whose content identity is derived from its complete
    /// current state. `prefix` namespaces the owner (e.g. `builtin:hand`).
    pub fn pinned(
        prefix: &str,
        id: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self::try_pinned(prefix, id, description, parameters)
            .expect("trusted tool descriptors must carry a valid object parameter schema")
    }

    /// Fallible constructor for descriptors originating outside the trusted
    /// process, such as MCP or protocol clients. It shares the exact canonical
    /// schema and content-hash path with [`Self::pinned`].
    pub fn try_pinned(
        prefix: &str,
        id: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Result<Self, ToolSchemaError> {
        let parameters = ToolSchema(normalize_model_tool_schema(parameters)?);
        let id = id.into();
        let description = description.into();
        Ok(Self {
            content_namespace: prefix.to_string(),
            id,
            description,
            parameters,
            kind: ToolKind::Regular,
            recovery_policy: crate::tool::ToolRecoveryPolicy::default(),
            provider_server_tool: None,
        })
    }

    /// Return the one provider-compatible projection. The schema was already
    /// normalized at construction or deserialization, so adapters cannot forget
    /// a validation step.
    pub fn model_parameters(&self) -> serde_json::Value {
        self.parameters.0.clone()
    }

    /// Stable content identity derived from every descriptor fact that changes
    /// model projection or execution semantics.
    #[must_use]
    pub fn content_hash(&self) -> String {
        content_hash(
            &self.content_namespace,
            &self.id,
            &self.description,
            &self.parameters,
            self.kind,
            &self.recovery_policy,
            self.provider_server_tool.as_ref(),
        )
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
        self.kind = kind;
        self
    }

    /// Pin an execution recovery policy without changing the model-visible
    /// schema. The policy still enters the content address so changing recovery
    /// semantics produces a different resolved snapshot.
    #[must_use]
    pub fn with_recovery(mut self, recovery: crate::tool::ToolRecoveryPolicy) -> Self {
        self.recovery_policy = recovery;
        self
    }

    /// Bind this canonical builtin to one exact provider-server realization.
    #[must_use]
    pub fn with_provider_server_tool(mut self, projection: ProviderServerTool) -> Self {
        self.provider_server_tool = Some(projection);
        self
    }
}

/// Generate the conservative JSON Schema dialect shared by model-provider tool
/// APIs. Definitions are inlined and the meta-schema/type-name annotations are
/// omitted: they do not describe model input, can make an internal Rust rename
/// alter a content hash, and are rejected by some compatible endpoints.
fn model_tool_schema<A: schemars::JsonSchema>() -> serde_json::Value {
    let settings = schemars::generate::SchemaSettings::draft07().with(|settings| {
        settings.meta_schema = None;
        settings.inline_subschemas = true;
    });
    let mut schema = serde_json::to_value(settings.into_generator().into_root_schema_for::<A>())
        .expect("a derived JSON Schema must serialize");
    if let Some(object) = schema.as_object_mut() {
        object.remove("title");
    }
    schema
}

/// Why a model-visible tool parameter schema cannot be projected safely.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid model tool schema at {path}: {reason}")]
pub struct ToolSchemaError {
    path: String,
    reason: String,
}

impl ToolSchemaError {
    fn new(path: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            reason: reason.into(),
        }
    }
}

/// Canonicalize one model-visible JSON Schema before hashing or provider
/// projection. Tool arguments are always an object. Missing object
/// `properties` and array `items` are compatibility-equivalent omissions and
/// receive explicit empty values; contradictory types fail closed.
pub fn normalize_model_tool_schema(
    mut schema: serde_json::Value,
) -> Result<serde_json::Value, ToolSchemaError> {
    let root = schema
        .as_object_mut()
        .ok_or_else(|| ToolSchemaError::new("$", "root must be a JSON object"))?;
    match root.get("type") {
        None => {
            root.insert("type".into(), serde_json::Value::String("object".into()));
        }
        Some(serde_json::Value::String(kind)) if kind == "object" => {}
        Some(_) => {
            return Err(ToolSchemaError::new(
                "$.type",
                "tool arguments must have type `object`",
            ));
        }
    }
    normalize_model_tool_schema_node(&mut schema, "$")?;
    Ok(schema)
}

fn normalize_model_tool_schema_node(
    schema: &mut serde_json::Value,
    path: &str,
) -> Result<(), ToolSchemaError> {
    let Some(object) = schema.as_object_mut() else {
        return Ok(());
    };
    match object.get("type").and_then(serde_json::Value::as_str) {
        Some("object") => match object.get("properties") {
            None => {
                object.insert(
                    "properties".into(),
                    serde_json::Value::Object(serde_json::Map::new()),
                );
            }
            Some(serde_json::Value::Object(_)) => {}
            Some(_) => {
                return Err(ToolSchemaError::new(
                    format!("{path}.properties"),
                    "`properties` must be a JSON object",
                ));
            }
        },
        Some("array") if !object.contains_key("items") => {
            object.insert(
                "items".into(),
                serde_json::Value::Object(serde_json::Map::new()),
            );
        }
        _ => {}
    }

    // Traverse only JSON Schema subschema keywords. Values under `default`,
    // `const`, `enum`, `examples`, and extension metadata are instance data,
    // even when an object in that data happens to contain a `type` field.
    for keyword in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
    ] {
        if let Some(entries) = object
            .get_mut(keyword)
            .and_then(serde_json::Value::as_object_mut)
        {
            for (key, value) in entries {
                normalize_model_tool_schema_node(value, &format!("{path}.{keyword}.{key}"))?;
            }
        }
    }
    for keyword in [
        "items",
        "contains",
        "additionalProperties",
        "unevaluatedProperties",
        "propertyNames",
        "not",
        "if",
        "then",
        "else",
    ] {
        if let Some(value) = object.get_mut(keyword) {
            normalize_model_tool_schema_node(value, &format!("{path}.{keyword}"))?;
        }
    }
    for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(items) = object
            .get_mut(keyword)
            .and_then(serde_json::Value::as_array_mut)
        {
            for (index, item) in items.iter_mut().enumerate() {
                normalize_model_tool_schema_node(item, &format!("{path}.{keyword}[{index}]"))?;
            }
        }
    }
    Ok(())
}

/// Reserved model-facing id of deferred-tool discovery (ADR-0053). The snake-case
/// spelling follows the ordinary client-tool style used by Anthropic, OpenRouter,
/// OpenAI-compatible, and Gemini-compatible APIs. The compiler prevents an author
/// from shadowing it with a catalog id or alias.
pub const TOOL_SEARCH_ID: &str = "tool_search";

/// Default number of definitions returned by one client-side discovery call. It is
/// deliberately small: discovery should expose the few tools needed for the next
/// step, not recreate the full catalog in the transcript.
pub const DEFAULT_TOOL_SEARCH_RESULTS: usize = 5;

/// Reserved model-facing name of the advisor service tool.
pub const ADVISOR_TOOL_ID: &str = "advisor";
/// Model-visible notice used when the configured Advisor capability is absent.
/// Runtime and Host share this value so fresh and recovered calls cannot expose
/// different failure text.
pub const ADVISOR_UNAVAILABLE_NOTICE: &str = "Advisor consultation unavailable.";
/// Model-visible notice for a terminal Advisor child failure or targeted
/// interruption. The underlying child failure remains in its own Run lifecycle;
/// only this generic text crosses back into the primary model transcript.
pub const ADVISOR_FAILURE_NOTICE: &str = "Advisor consultation failed.";

/// How deferred-tool guidance is added to the model request. This affects only the
/// request view; it is never committed into the Thread transcript.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ToolPromptInjection {
    /// Inject the runtime-owned provider-neutral discovery guidance.
    #[default]
    Automatic,
    /// Publish `tool_search`, but do not inject an additional system message.
    Disabled,
    /// Inject composition-owned guidance instead of the default wording.
    Custom { text: String },
}

/// Tool-discovery behavior compiled into an executable Agent revision. An absent
/// `max_results` means [`DEFAULT_TOOL_SEARCH_RESULTS`]; the constrained value
/// prevents zero and oversized limits from entering a resolved snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDiscoverySettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<crate::tool_discovery::ToolSearchLimit>,
    #[serde(default, skip_serializing_if = "ToolPromptInjection::is_automatic")]
    pub prompt: ToolPromptInjection,
}

impl ToolPromptInjection {
    #[must_use]
    pub const fn is_automatic(&self) -> bool {
        matches!(self, Self::Automatic)
    }
}

impl ToolDiscoverySettings {
    #[must_use]
    pub fn effective_max_results(&self) -> usize {
        self.max_results
            .map_or(DEFAULT_TOOL_SEARCH_RESULTS, |limit| {
                usize::from(limit.get())
            })
    }
}

/// Build the reserved client-side `tool_search` descriptor from the still-deferred
/// tools. Only names and short descriptions are listed here; full schemas are returned
/// by the search result and become visible on the following step.
fn tool_search_descriptor(settings: &ToolDiscoverySettings) -> ToolDescriptor {
    let mut parameters = model_tool_schema::<crate::tool_discovery::ToolSearchInput>();
    if let Some(maximum) = parameters.pointer_mut("/properties/max_results/maximum") {
        debug_assert_eq!(
            maximum.as_u64(),
            Some(u64::from(crate::tool_discovery::ToolSearchLimit::MAX))
        );
    }
    parameters["properties"]["max_results"]["maximum"] =
        serde_json::json!(settings.effective_max_results());
    ToolDescriptor::pinned(
        "builtin:presentation",
        TOOL_SEARCH_ID,
        "Search tools whose definitions are available on demand. Search by capability, tool \
         name, or argument purpose; use `select:name_a,name_b` for exact selection. Matching \
         tools become callable on the next step.",
        parameters,
    )
}

/// Whether one authorized tool definition is sent eagerly or exposed through
/// provider-neutral discovery.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExposure {
    #[default]
    Eager,
    OnDemand,
}

/// Closed selector vocabulary for static and live tool ids. Exact ids cover one
/// tool; prefixes cover dynamic namespaces such as `mcp__docs__`. Regex/glob text
/// is deliberately excluded so malformed selectors cannot enter a snapshot and
/// the contract need not depend on an extension-owned matcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ToolSelector {
    Exact(String),
    Prefix(String),
}

impl ToolSelector {
    #[must_use]
    pub fn matches(&self, canonical_id: &str) -> bool {
        match self {
            Self::Exact(id) => canonical_id == id,
            Self::Prefix(prefix) => canonical_id.starts_with(prefix),
        }
    }
}

/// One ordered selector rule over canonical tool ids. Rules are evaluated
/// first-match wins; an exact presentation override has final precedence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExposureRule {
    pub selector: ToolSelector,
    pub exposure: ToolExposure,
}

/// Catalog-wide exposure policy. It contains selectors only, never descriptors,
/// so dynamic MCP tools and static tools remain in one authoritative catalog.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExposurePolicy {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<ToolExposureRule>,
    #[serde(default, skip_serializing_if = "ToolExposure::is_eager")]
    pub default: ToolExposure,
}

impl ToolExposure {
    #[must_use]
    pub const fn is_eager(&self) -> bool {
        matches!(self, Self::Eager)
    }
}

impl ToolExposurePolicy {
    #[must_use]
    pub fn resolve(&self, canonical_id: &str) -> ToolExposure {
        self.rules
            .iter()
            .find(|rule| rule.selector.matches(canonical_id))
            .map_or(self.default, |rule| rule.exposure)
    }

    #[must_use]
    pub fn is_default(&self) -> bool {
        self.rules.is_empty() && self.default == ToolExposure::Eager
    }
}

/// The model-facing presentation of an agent's tools (ADR-0053): a per-tool `alias`,
/// `description`, and optional exposure override, keyed by the tool's **canonical** id — a
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
    /// Canonical tool id → its exact presentation override.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    overrides: BTreeMap<String, ToolPresentationOverride>,
    #[serde(default, skip_serializing_if = "ToolExposurePolicy::is_default")]
    exposure: ToolExposurePolicy,
    #[serde(default, skip_serializing_if = "ToolDiscoverySettings::is_default")]
    discovery: ToolDiscoverySettings,
}

/// One exact tool's optional appearance and exposure override.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolPresentationOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exposure: Option<ToolExposure>,
}

/// Appearance/exposure projection before Run-scoped discovery is applied.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PresentedToolCatalog {
    pub visible: Vec<ToolDescriptor>,
    pub discoverable: Vec<ToolDescriptor>,
}

/// The one complete model projection for a Step. Tools and request-only guidance
/// are derived from the same presented catalog so visibility cannot drift from
/// the prompt and descriptors are traversed only once per request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolModelProjection {
    pub tools: Vec<ToolDescriptor>,
    pub prompt: Option<String>,
}

impl ToolDiscoverySettings {
    #[must_use]
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

impl ToolPresentation {
    /// Build from `(canonical_id, override)` pairs; entirely default entries
    /// (no alias, description, or exposure change) are dropped so an all-default
    /// presentation is [`is_empty`](Self::is_empty) and stays byte-identical.
    pub fn from_overrides(
        overrides: impl IntoIterator<Item = (String, ToolPresentationOverride)>,
    ) -> Self {
        let overrides = overrides
            .into_iter()
            .filter(|(_, value)| {
                value.alias.is_some() || value.description.is_some() || value.exposure.is_some()
            })
            .collect();
        Self {
            overrides,
            exposure: ToolExposurePolicy::default(),
            discovery: ToolDiscoverySettings::default(),
        }
    }

    #[must_use]
    pub fn with_exposure_policy(mut self, policy: ToolExposurePolicy) -> Self {
        self.exposure = policy;
        self
    }

    #[must_use]
    pub fn with_discovery(mut self, settings: ToolDiscoverySettings) -> Self {
        self.discovery = settings;
        self
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.overrides.is_empty() && self.exposure.is_default()
    }

    /// The canonical ids this presentation overrides (used at compile to validate each
    /// targets a selected tool).
    pub fn targets(&self) -> impl Iterator<Item = &str> {
        self.overrides.keys().map(String::as_str)
    }

    /// Reverse a model-supplied tool id back to its canonical id (the identity when the
    /// id is not an alias). The single choke every internal consumer routes a tool call
    /// through, so the alias never leaks past the model-facing boundary.
    #[must_use]
    pub fn resolve<'a>(&'a self, model_id: &'a str) -> &'a str {
        self.overrides
            .iter()
            .find(|(_, f)| f.alias.as_deref() == Some(model_id))
            .map_or(model_id, |(canonical, _)| canonical.as_str())
    }

    /// Effective exposure for one canonical id. An exact override wins over the
    /// ordered catalog-wide policy.
    #[must_use]
    pub fn exposure(&self, canonical: &str) -> ToolExposure {
        self.overrides
            .get(canonical)
            .and_then(|value| value.exposure)
            .unwrap_or_else(|| self.exposure.resolve(canonical))
    }

    #[must_use]
    pub fn discovery(&self) -> &ToolDiscoverySettings {
        &self.discovery
    }

    /// Project the complete model-facing view for one Step in one pass: appearance,
    /// Run-scoped reveals, `tool_search`, and request-only guidance.
    #[must_use]
    pub fn model_projection(
        &self,
        descriptors: &[ToolDescriptor],
        is_revealed: impl Fn(&str, &str) -> bool,
    ) -> ToolModelProjection {
        let presented = self.present(descriptors);
        let mut tools = presented.visible;
        let mut discoverable = Vec::new();
        for descriptor in presented.discoverable {
            if is_revealed(self.resolve(&descriptor.id), &descriptor.content_hash()) {
                tools.push(descriptor);
            } else {
                discoverable.push(descriptor);
            }
        }
        if discoverable.is_empty() {
            return ToolModelProjection {
                tools,
                prompt: None,
            };
        }
        tools.push(tool_search_descriptor(&self.discovery));
        let prompt = match &self.discovery.prompt {
            ToolPromptInjection::Disabled => None,
            ToolPromptInjection::Custom { text } => Some(text.clone()),
            ToolPromptInjection::Automatic => {
                let names = discoverable
                    .iter()
                    .map(|descriptor| descriptor.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                Some(format!(
                    "Some tool definitions are available on demand to reduce context. Use `{TOOL_SEARCH_ID}` \
                     when a needed capability is not visible; search by capability or exact name \
                     with `select:name`. Returned tools become callable on the next step. \
                     On-demand tool names: {names}."
                ))
            }
        };
        ToolModelProjection { tools, prompt }
    }

    /// Split canonical descriptors into the model face (alias + description applied) and
    /// the on-demand set (withheld until revealed). A descriptor with no override passes
    /// through to the face unchanged.
    #[must_use]
    pub fn present(&self, descriptors: &[ToolDescriptor]) -> PresentedToolCatalog {
        let mut out = PresentedToolCatalog::default();
        for d in descriptors {
            let mut shown = d.clone();
            if let Some(override_) = self.overrides.get(&d.id) {
                if let Some(alias) = &override_.alias {
                    shown.id = alias.clone();
                }
                if let Some(desc) = &override_.description {
                    shown.description = desc.clone();
                }
            }
            if self.exposure(&d.id) == ToolExposure::OnDemand {
                out.discoverable.push(shown);
            } else {
                out.visible.push(shown);
            }
        }
        out
    }
}

/// Stable content hash over the model-visible descriptor surface. Uses a
/// canonical JSON encoding so equal schemas hash equally regardless of the
/// in-memory `Value` shape, and SHA-256 so the digest is portable across
/// processes and Rust versions. Only its source facts are persisted; the hash is
/// always derived, so stale or forged duplicate identity cannot be represented.
fn content_hash(
    prefix: &str,
    id: &str,
    description: &str,
    parameters: &serde_json::Value,
    kind: ToolKind,
    recovery: &crate::tool::ToolRecoveryPolicy,
    provider_server_tool: Option<&ProviderServerTool>,
) -> String {
    use sha2::{Digest, Sha256};
    let canonical = parameters.to_string();
    let mut hasher = Sha256::new();
    // Length-prefix each field so `(id, description)` and `(id+description, "")`
    // cannot collide by concatenation.
    let kind = match kind {
        ToolKind::Regular => "regular",
        ToolKind::ClientExecuted => "client_executed",
        ToolKind::AgentDelegation => "agent_delegation",
        ToolKind::Advisor => "advisor",
    };
    let recovery_mode = match recovery.mode() {
        crate::tool::ToolRecoveryMode::NeverReplay => "never_replay",
        crate::tool::ToolRecoveryMode::ReplaySafe => "replay_safe",
        crate::tool::ToolRecoveryMode::Idempotent => "idempotent",
        crate::tool::ToolRecoveryMode::DurableRequest => "durable_request",
    };
    let server_projection = provider_server_tool
        .map(|projection| serde_json::to_string(projection).expect("provider tool serializes"))
        .unwrap_or_default();
    for field in [
        id,
        description,
        canonical.as_str(),
        kind,
        recovery_mode,
        server_projection.as_str(),
    ] {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    hasher.update(recovery.max_attempts().get().to_le_bytes());
    let digest = hasher.finalize();
    let mut short_digest = [0_u8; 8];
    short_digest.copy_from_slice(&digest[..8]);
    // 16 hex chars (64 bits) keeps the id readable while a schema change still
    // moves the digest; the full prefix keeps owner namespacing.
    format!("{prefix}:{id}:{:016x}", u64::from_le_bytes(short_digest))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedRun {
    pub snapshot_id: crate::snapshot::ExecutableAgentSnapshotId,
    pub agent_id: crate::snapshot::AgentId,
    pub spec: ResolvedSpec,
}

#[cfg(test)]
#[path = "resolved/tests.rs"]
mod tests;
