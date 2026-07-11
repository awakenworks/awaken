//! Config resolver (ADR-0043) — the management plane's *read/resolution face*.
//! It owns **no aggregate**; it reads the config stores (`awaken-model-catalog`,
//! `awaken-credential-vault`, and — in the assembly — agent config) and produces the
//! already-resolved input the runtime executes: an [`InferenceTriple`] plus an
//! already-materialized secret ([`RedactedString`]).
//!
//! This is the crate formerly mislabeled "inference": it *resolves* config into
//! an executable binding; it does **not** run inference (that is
//! `awaken-provider-genai`). Execution never depends on this crate (D6/D9 / I4).
//!
//! P0: single offering / `Exact` credential / `Derive` endpoint. Pools, multi-tier
//! `InferenceProfile` failover, and `Pin` are P1.

#![forbid(unsafe_code)]

use awaken_agent_contract::RedactedString;
use awaken_credential_vault::{
    AvailabilityLedger, CredentialBinding, CredentialError, CredentialSource, SecretStore,
};
use awaken_model_catalog::{ModelApiCompat, ProviderCatalog};

/// Read ports for the authored aggregates (`McpStore`, `InferenceProfileStore`,
/// `ResourceStore`) + in-memory reference impls. They live on the read side so
/// the runtime host reads config without depending on the authoring HTTP crate
/// (which writes through the same ports).
pub mod stores;
/// Telemetry ceiling composition (ADR-0050 D3): Org baseline tightened by lower layers.
pub mod telemetry;
pub use stores::{
    InMemoryMcpStore, InMemoryProfileStore, InMemoryResourceStore, InferenceProfileStore, McpStore,
    ResourceStore,
};
pub use telemetry::{RedactionMode, TelemetryCeiling};

/// The resolved execution unit: *(model × credential-identity × provider ×
/// flavor)*. Mirrors awaken-management-contract's `InferenceTriple`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InferenceTriple {
    pub model_id: String,
    pub provider_id: String,
    pub protocol_endpoint_id: String,
    pub flavor: ModelApiCompat,
}

/// What the resolver hands the run loop: the concrete target + wire + an
/// already-resolved secret (or `None` for host-native). The runtime sees only
/// this — never a binding, a ref, or the stores (D6/D9).
#[derive(Debug)]
pub struct ResolvedInference {
    pub triple: InferenceTriple,
    /// The adapter kind that speaks this flavor (`anthropic`/`openai`/…).
    pub adapter_kind: &'static str,
    /// Endpoint base URL override, if any.
    pub base_url: Option<String>,
    /// The already-materialized provider credential; `None` when the binding is
    /// `None` or the source is host-native with no value.
    pub credential: Option<RedactedString>,
}

/// A resolution failure (fail-closed).
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no offering for model `{0}` (model reference did not resolve — fail closed)")]
    ModelUnresolved(String),
    #[error("endpoint `{0}` missing from catalog")]
    EndpointMissing(String),
    #[error("credential source `{0}` not provided")]
    SourceMissing(String),
    #[error("credential pool `{0}` not provided")]
    PoolMissing(String),
    #[error(
        "credential `{source_id}` cannot authenticate provider `{provider_id}` \
         (fail closed): a key scoped to one provider may not run another's model"
    )]
    IncompatibleCredential {
        source_id: String,
        provider_id: String,
    },
    #[error(
        "credential pool `{pool_id}` has no eligible member (fail closed): \
         {total} total, {cooled} cooled, {over_capacity} over capacity"
    )]
    NoEligibleCredential {
        pool_id: String,
        /// Members considered (the pool's eligible/enabled selection order).
        total: usize,
        /// Members excluded because their identity is in cooldown. Always 0 until
        /// availability-aware selection lands (E3-4); present so the diagnostic
        /// shape does not change when it does.
        cooled: usize,
        /// Members excluded because their account is over its capacity bucket.
        /// Always 0 until quota buckets land; present for the same reason.
        over_capacity: usize,
    },
    #[error(transparent)]
    Credential(#[from] CredentialError),
}

/// A credential lookup the assembly provides: individual sources by id, and pools
/// by id for the `OneOfCredentialPool` binding. `get_pool` defaults to `None`, so a
/// flat `HashMap<String, CredentialSource>` still satisfies the trait for the
/// `Exact`/`None` bindings without knowing about pools.
pub trait SourceLookup: Send + Sync {
    fn get(&self, id: &str) -> Option<&CredentialSource>;
    fn get_pool(&self, _id: &str) -> Option<&awaken_credential_vault::CredentialPool> {
        None
    }
}

impl SourceLookup for std::collections::HashMap<String, CredentialSource> {
    fn get(&self, id: &str) -> Option<&CredentialSource> {
        std::collections::HashMap::get(self, id)
    }
}

/// Resolve a model reference + credential binding against the catalog into a
/// [`ResolvedInference`]. This is `reconcile_model_ref` + `resolve_inference` +
/// credential `materialize`, composed (ADR-0043).
///
/// Picks the first offering for `model_id` (`Offering(model) ∩ flavor`), the given
/// binding, and materializes its credential. Endpoint selection honors no toggles;
/// use [`resolve_profile`] to skip endpoints an operator disabled.
pub async fn resolve_inference(
    catalog: &ProviderCatalog,
    model_id: &str,
    binding: &CredentialBinding,
    sources: &dyn SourceLookup,
    secret_store: &dyn SecretStore,
) -> Result<ResolvedInference, ResolveError> {
    resolve_inference_toggled(catalog, model_id, &[], binding, sources, secret_store).await
}

/// The core resolution, with an operator's disabled-endpoint toggle applied: an
/// offering whose endpoint id is in `disabled_endpoints` is skipped, so a
/// `(credential × interface)` an operator turned off is never selected.
async fn resolve_inference_toggled(
    catalog: &ProviderCatalog,
    model_id: &str,
    disabled_endpoints: &[String],
    binding: &CredentialBinding,
    sources: &dyn SourceLookup,
    secret_store: &dyn SecretStore,
) -> Result<ResolvedInference, ResolveError> {
    // reconcile_model_ref + resolve_inference (Derive: first enabled offering).
    let offering = catalog
        .offerings
        .iter()
        .find(|o| o.model_id == model_id && !disabled_endpoints.contains(&o.protocol_endpoint_id.0))
        .ok_or_else(|| ResolveError::ModelUnresolved(model_id.to_string()))?;

    let endpoint = catalog
        .endpoints
        .get(offering.protocol_endpoint_id.as_str())
        .ok_or_else(|| ResolveError::EndpointMissing(offering.protocol_endpoint_id.0.clone()))?;

    let triple = InferenceTriple {
        model_id: offering
            .upstream_model
            .clone()
            .unwrap_or_else(|| offering.model_id.clone()),
        provider_id: offering.provider_id.0.clone(),
        protocol_endpoint_id: offering.protocol_endpoint_id.0.clone(),
        flavor: offering.flavor,
    };

    // Credential materialization (secret only exists from here to the seam). The
    // offering's provider gates the credential via can_consume — an incompatible
    // key never authenticates a model it cannot serve.
    // Availability-aware pool selection is wired by the host when it tracks a live
    // ledger; the base resolution path does not cool credentials itself.
    let credential = resolve_credential(
        binding,
        sources,
        secret_store,
        Some(offering.provider_id.0.as_str()),
        None,
    )
    .await?;

    Ok(ResolvedInference {
        triple,
        adapter_kind: endpoint.flavor.adapter_kind(),
        base_url: endpoint.base_url.clone(),
        credential,
    })
}

/// An authored "how to run this model" unit (ADR-0043 `InferenceProfile` /
/// oversight-next `ProviderIdentity`): it names the model, the credential binding
/// (vault-backed, never inline), and any endpoints the operator has toggled off.
/// The resolver reads it — it is never flowed into the runtime.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct InferenceProfile {
    /// The pinned / primary model — tried first. Kept as a bare field for wire and
    /// storage compatibility; the ordered model axis is [`model_axis`] (this plus
    /// [`model_fallbacks`]).
    ///
    /// [`model_axis`]: InferenceProfile::model_axis
    /// [`model_fallbacks`]: InferenceProfile::model_fallbacks
    pub model_id: String,
    /// Additional models the resolver falls over to, in order, after `model_id`.
    /// Empty (the default) means a single-model profile — unchanged behavior, and
    /// older stored rows load without the field. Together with `model_id` these
    /// form the [`AxisBinding`] the profile exposes as [`model_axis`].
    ///
    /// [`model_axis`]: InferenceProfile::model_axis
    #[serde(default)]
    pub model_fallbacks: Vec<String>,
    /// The credential-identity axis. `CredentialBinding` is *already* an
    /// [`AxisBinding`] over provider identities — `Exact` is a pin, and
    /// `OneOfCredentialPool` is a pool with failover — so the identity axis needs
    /// no new type here; each resolved model reuses this binding.
    pub credential_binding: CredentialBinding,
    #[serde(default)]
    pub disabled_endpoint_ids: Vec<String>,
}

impl InferenceProfile {
    /// The model axis as an ordered [`AxisBinding`]: a lone `model_id` is a
    /// [`Pin`](AxisBinding::Pin); `model_id` plus fallbacks is a
    /// [`Pool`](AxisBinding::Pool) in try-order.
    #[must_use]
    pub fn model_axis(&self) -> AxisBinding<String> {
        if self.model_fallbacks.is_empty() {
            AxisBinding::Pin(self.model_id.clone())
        } else {
            let mut models = Vec::with_capacity(self.model_fallbacks.len() + 1);
            models.push(self.model_id.clone());
            models.extend(self.model_fallbacks.iter().cloned());
            AxisBinding::Pool(models)
        }
    }
}

/// A per-axis binding: the agent either pins one value or pools an ordered set the
/// resolver fails over across. The unifying shape behind "select a model" (`Pin`)
/// and "spread across a model pool" (`Pool`) — and, via
/// [`CredentialBinding`](awaken_credential_vault::CredentialBinding), behind the
/// credential-identity axis too. Names align with awaken-next's `AxisBinding`
/// (`Pin | Pool`); the `Any` variant is deferred until a slice needs it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum AxisBinding<T> {
    /// Exactly this value.
    Pin(T),
    /// This ordered set, tried in order (the first is the preference).
    Pool(Vec<T>),
}

impl<T: Clone> AxisBinding<T> {
    /// The candidates in try-order: one for a [`Pin`](Self::Pin), the whole set for
    /// a [`Pool`](Self::Pool).
    #[must_use]
    pub fn candidates(&self) -> Vec<T> {
        match self {
            AxisBinding::Pin(v) => vec![v.clone()],
            AxisBinding::Pool(vs) => vs.clone(),
        }
    }

    /// The preferred candidate: the pinned value, or the first pool member.
    /// `None` only for an empty pool.
    #[must_use]
    pub fn primary(&self) -> Option<&T> {
        match self {
            AxisBinding::Pin(v) => Some(v),
            AxisBinding::Pool(vs) => vs.first(),
        }
    }
}

/// Resolve an [`InferenceProfile`] into a [`ResolvedInference`]: the same core
/// resolution, but selecting only endpoints the profile has not disabled and using
/// the profile's credential binding (which may be a pool with failover). Resolves
/// the *primary* model only; use [`resolve_profile_candidates`] for the whole axis.
pub async fn resolve_profile(
    catalog: &ProviderCatalog,
    profile: &InferenceProfile,
    sources: &dyn SourceLookup,
    secret_store: &dyn SecretStore,
) -> Result<ResolvedInference, ResolveError> {
    resolve_inference_toggled(
        catalog,
        &profile.model_id,
        &profile.disabled_endpoint_ids,
        &profile.credential_binding,
        sources,
        secret_store,
    )
    .await
}

/// Resolve an [`InferenceProfile`] into the **ordered candidate list** the engine
/// fails over across: one [`ResolvedInference`] per model in the profile's
/// [`model_axis`](InferenceProfile::model_axis), each carrying its own materialized
/// credential (the credential axis fails over *within* each resolution). This is
/// the unification of the model axis and the credential axis into one ordered set
/// of `(model × identity)` candidates.
///
/// Fail-closed per candidate is *not* terminal: a model that does not resolve (no
/// offering, or its whole credential pool is exhausted) is skipped, so one bad
/// model does not sink the profile. The result preserves axis order; it is empty
/// only when *no* candidate resolved, which the caller treats as fail-closed.
pub async fn resolve_profile_candidates(
    catalog: &ProviderCatalog,
    profile: &InferenceProfile,
    sources: &dyn SourceLookup,
    secret_store: &dyn SecretStore,
) -> Result<Vec<ResolvedInference>, ResolveError> {
    let mut resolved = Vec::new();
    let mut last_err = None;
    for model_id in profile.model_axis().candidates() {
        match resolve_inference_toggled(
            catalog,
            &model_id,
            &profile.disabled_endpoint_ids,
            &profile.credential_binding,
            sources,
            secret_store,
        )
        .await
        {
            Ok(r) => resolved.push(r),
            Err(e) => last_err = Some(e),
        }
    }
    if resolved.is_empty() {
        // Every candidate failed: surface the last reason (fail-closed) rather than
        // an empty success.
        return Err(
            last_err.unwrap_or_else(|| ResolveError::ModelUnresolved(profile.model_id.clone()))
        );
    }
    Ok(resolved)
}

/// Materialize the credential a binding selects. `None` yields no secret; `Exact`
/// materializes one named source; `OneOfCredentialPool` walks the pool's selection
/// order and returns the first member that materializes — a disabled or unusable
/// member fails over to the next. Fail-closed: an empty/all-bad pool is an error,
/// never a silent unauthenticated run.
/// The validity join (ADR-0118 `can_consume`): may this credential authenticate
/// this provider? A source scoped to a provider (`provider_id = Some("anthropic")`)
/// may only consume that provider's offerings; an unscoped source
/// (`provider_id = None`, host-native / env) may consume any. This is what stops an
/// otherwise-materializable key being paired with a model it cannot authenticate —
/// the invalid `(model × credential)` combination the ADR calls out.
#[must_use]
pub fn can_consume(offering_provider_id: &str, source: &CredentialSource) -> bool {
    source
        .provider_id
        .as_deref()
        .is_none_or(|scoped| scoped == offering_provider_id)
}

/// Materialize the credential a binding selects, gated by [`can_consume`] when an
/// `offering_provider` is given (the inference path); `None` skips the join (e.g.
/// an MCP-server credential, which is not a model provider). `None` binding yields
/// no secret; `Exact` materializes one named source; `OneOfCredentialPool` walks
/// the pool and returns the first member that is *both* compatible and
/// materializable — an incompatible, disabled, or unusable member fails over to the
/// next. Fail-closed: an empty/all-bad pool is an error, never a silent run.
async fn resolve_credential(
    binding: &CredentialBinding,
    sources: &dyn SourceLookup,
    secret_store: &dyn SecretStore,
    offering_provider: Option<&str>,
    availability: Option<(&AvailabilityLedger, u64)>,
) -> Result<Option<RedactedString>, ResolveError> {
    match binding {
        CredentialBinding::None => Ok(None),
        CredentialBinding::Exact {
            credential_source_id,
        } => {
            let source = sources
                .get(credential_source_id.0.as_str())
                .ok_or_else(|| ResolveError::SourceMissing(credential_source_id.0.clone()))?;
            if let Some(provider) = offering_provider {
                if !can_consume(provider, source) {
                    return Err(ResolveError::IncompatibleCredential {
                        source_id: credential_source_id.0.clone(),
                        provider_id: provider.to_string(),
                    });
                }
            }
            Ok(Some(
                awaken_credential_vault::materialize(source, secret_store).await?,
            ))
        }
        CredentialBinding::OneOfCredentialPool { credential_pool_id } => {
            let pool = sources
                .get_pool(credential_pool_id.0.as_str())
                .ok_or_else(|| ResolveError::PoolMissing(credential_pool_id.0.clone()))?;
            // The eligible order drops cooled members (mid-run rotation) when a ledger
            // is supplied; `cooled` is how many the cooldown excluded, for the
            // fail-closed diagnostic.
            let full = pool.selection_order();
            let total = full.len();
            let order = match availability {
                Some((ledger, now_ms)) => pool.eligible_order(ledger, now_ms),
                None => full,
            };
            let cooled = total - order.len();
            // Try members in eligible order; skip a member whose source is absent,
            // incompatible with the provider, or fails to materialize, so one bad key
            // does not fail the run.
            for member in order {
                let Some(source) = sources.get(member.credential_source_id.0.as_str()) else {
                    continue;
                };
                if offering_provider.is_some_and(|provider| !can_consume(provider, source)) {
                    continue;
                }
                if let Ok(secret) = awaken_credential_vault::materialize(source, secret_store).await
                {
                    return Ok(Some(secret));
                }
            }
            Err(ResolveError::NoEligibleCredential {
                pool_id: credential_pool_id.0.clone(),
                total,
                cooled,
                // Quota buckets are not modeled yet; capacity exclusion stays 0.
                over_capacity: 0,
            })
        }
    }
}

/// The cooldown deadline a failure disposition implies, in wall-clock ms, or `None`
/// if the failure is not a quota/rate signal. The bridge from
/// [`Disposition`](awaken_runtime_contract::resilience::Disposition) to the vault's
/// [`AvailabilityLedger`](awaken_credential_vault::AvailabilityLedger): a
/// `Quota{retry_after}` cools the identity that hit it until `now + retry_after`
/// (or a default window when the provider sent no hint), which the caller records
/// so the next selection rotates past it.
#[must_use]
pub fn cooldown_deadline(
    disposition: awaken_runtime_contract::resilience::Disposition,
    now_ms: u64,
) -> Option<u64> {
    use awaken_runtime_contract::resilience::Disposition;
    /// Fallback cooldown when a 429/quota carries no `Retry-After` (60s).
    const DEFAULT_COOLDOWN_MS: u64 = 60_000;
    match disposition {
        Disposition::Quota { retry_after } => {
            let window = retry_after
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or(DEFAULT_COOLDOWN_MS);
            Some(now_ms.saturating_add(window))
        }
        Disposition::Transient | Disposition::Permanent => None,
    }
}

/// A management-plane MCP server identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpServerId(pub String);

/// An authored MCP server definition (ADR-0043 Phase 3): where the server lives
/// and which credential authenticates to it. The binding is the same vault-backed
/// [`CredentialBinding`] inference uses (never an inline secret), so
/// `OneOfCredentialPool` failover applies to MCP credentials for free. The
/// resolver reads it — it is never flowed into the runtime.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpServerDef {
    pub id: McpServerId,
    pub display_name: String,
    pub url: String,
    pub credential_binding: CredentialBinding,
    pub version: i64,
}

/// Which MCP servers an agent uses — the management-plane agent↔MCP binding
/// (ADR-0043 Phase 3). References [`McpServerDef`]s by id; the resolver
/// materializes the referenced defs into [`ResolvedMcpServer`]s at run bind time.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentMcpConfig {
    pub agent_id: String,
    pub mcp_server_ids: Vec<McpServerId>,
    pub version: i64,
}

/// Which resources an agent is bound to — the management-plane agent↔resource
/// binding (ADR-0038). At run bind time each [`ResourceBinding`] is materialized
/// two ways: into a `MountRequirement` the sandbox realizes, and into a prompt
/// fragment appended to the agent's effective system prompt (ADR-0038 A3a). Rows
/// are secret-free — a private resource's credential is a binding by reference,
/// resolved through the vault like MCP auth, never material here.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentResourceConfig {
    pub agent_id: String,
    pub resources: Vec<ResourceBinding>,
    pub version: i64,
}

/// One resource bound to an agent. `resource_id` addresses the backing resource
/// (a file/skill id, a memory store id, a repo URL; empty for the outputs mount);
/// `mount_path` is where it appears in the sandbox; `instructions` is optional
/// per-binding guidance rendered into the agent's system prompt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResourceBinding {
    pub kind: ResourceKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resource_id: String,
    pub mount_path: String,
    pub access: ResourceAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// The resource kind a binding realizes — one variant per ADR-0038 resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ResourceKind {
    /// The outputs mount the host collects as artifacts.
    Outputs,
    /// An immutable file blob.
    File,
    /// A persistent, keyed memory store.
    MemoryStore,
    /// A git working tree cloned from a remote.
    GithubRepository,
    /// A versioned skill bundle.
    Skill,
}

/// Whether a bound resource is read-only or writable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ResourceAccess {
    ReadOnly,
    ReadWrite,
}

/// Render a bound resource into the prompt fragment appended to the agent's system
/// prompt at compile time (ADR-0038 A3a): one blurb per kind naming its mount path
/// and access, plus any per-binding `instructions`. This is the resolve-side
/// template layer; the config-store's `compose_instructions` does the join and the
/// runtime never re-renders it per turn.
#[must_use]
pub fn resource_binding_prompt(binding: &ResourceBinding) -> String {
    let path = &binding.mount_path;
    let access = match binding.access {
        ResourceAccess::ReadOnly => "read-only",
        ResourceAccess::ReadWrite => "read/write",
    };
    let base = match binding.kind {
        ResourceKind::Outputs => format!(
            "Write any output files you want the caller to keep under `{path}`; \
             files there are collected as run artifacts."
        ),
        ResourceKind::File => format!("A file is mounted {access} at `{path}`."),
        ResourceKind::MemoryStore => format!(
            "A persistent memory store is mounted {access} as the single file `{path}`. \
             Read that exact file for prior context. To remember something, write it \
             back to that same file at `{path}` (overwrite it) — do not create any other \
             file. Its contents persist across sessions."
        ),
        ResourceKind::GithubRepository => format!(
            "A git repository is checked out at `{path}` ({access}); use git there to \
             read, edit, commit, and push."
        ),
        ResourceKind::Skill => format!("A skill bundle is mounted read-only at `{path}`."),
    };
    match &binding.instructions {
        Some(extra) if !extra.is_empty() => format!("{base}\n{extra}"),
        _ => base,
    }
}

/// The ordered prompt fragments for an agent's bound resources — the bridge from the
/// [`AgentResourceConfig`] aggregate to the config-store's `compile_with_resource_prompts`
/// (which appends them to the agent's effective system prompt, ADR-0038 A3a). One
/// fragment per binding, in binding order; empty when the agent binds no resources
/// (so compilation stays byte-identical to an unbound agent).
#[must_use]
pub fn resource_prompts_for(config: &AgentResourceConfig) -> Vec<String> {
    config
        .resources
        .iter()
        .map(resource_binding_prompt)
        .collect()
}

/// The injection-ready MCP server the resolver hands the runtime: the display
/// name, the URL, and an already-materialized credential (or `None` for an
/// unauthenticated server). The runtime sees only this — never a binding, a ref,
/// or the stores (D6/D9); secret-free rows never cross after this point.
#[derive(Debug)]
pub struct ResolvedMcpServer {
    pub name: String,
    pub url: String,
    pub credential: Option<RedactedString>,
}

/// Resolve authored [`McpServerDef`]s into injection-ready [`ResolvedMcpServer`]s
/// by materializing each def's credential binding — the same
/// [`resolve_credential`] path inference uses, so a pool binding fails over
/// member-by-member. Fail-closed: a def whose `Exact` source is missing or
/// cannot be materialized (or whose pool is missing/exhausted) is an error, never
/// a silently unauthenticated server; a `None` binding yields `credential: None`.
pub async fn resolve_mcp_servers(
    defs: &[McpServerDef],
    sources: &dyn SourceLookup,
    secret_store: &dyn SecretStore,
) -> Result<Vec<ResolvedMcpServer>, ResolveError> {
    let mut resolved = Vec::with_capacity(defs.len());
    for def in defs {
        // MCP-server credential: not a model provider, so no can_consume join and no
        // availability ledger.
        let credential =
            resolve_credential(&def.credential_binding, sources, secret_store, None, None).await?;
        resolved.push(ResolvedMcpServer {
            name: def.display_name.clone(),
            url: def.url.clone(),
            credential,
        });
    }
    Ok(resolved)
}

/// The complete run input the resolver hands the run loop. Two parts travel
/// together but are **not** merged: the [`ExecutableAgentSnapshot`] is
/// serializable and secret-free (it can be persisted/replayed), while
/// [`ResolvedInference`] carries the non-serializable `RedactedString` (it exists
/// only in memory, for this run). This is the D6/D9-correct reading of "the
/// snapshot carries the resolved credential" — the secret never enters the
/// persisted snapshot.
pub struct RunInput {
    pub snapshot: awaken_runtime_contract::snapshot::ExecutableAgentSnapshot,
    pub inference: ResolvedInference,
}

/// Orchestrate a run: take a compiled (agent-config) snapshot, read its selected
/// model from `resolved_spec.model_binding`, resolve the inference triple +
/// materialize the credential against the catalog/credential stores, and bundle
/// both into a [`RunInput`]. The model is **never re-picked** here (G22): the
/// snapshot's `model_ref` is authoritative.
pub async fn resolve_run(
    snapshot: awaken_runtime_contract::snapshot::ExecutableAgentSnapshot,
    catalog: &ProviderCatalog,
    binding: &CredentialBinding,
    sources: &dyn SourceLookup,
    secret_store: &dyn SecretStore,
) -> Result<RunInput, ResolveError> {
    let model_id = snapshot.resolved_spec.model_binding.model_ref.clone();
    let inference = resolve_inference(catalog, &model_id, binding, sources, secret_store).await?;
    Ok(RunInput {
        snapshot,
        inference,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_vault::{
        CredentialCreateParams, CredentialKind, CredentialSourceId, InMemorySecretStore,
        create_source,
    };

    fn binding(kind: ResourceKind, path: &str, access: ResourceAccess) -> ResourceBinding {
        ResourceBinding {
            kind,
            resource_id: String::new(),
            mount_path: path.to_string(),
            access,
            instructions: None,
        }
    }

    #[test]
    fn resource_binding_prompt_names_path_access_and_appends_instructions() {
        // outputs: artifact guidance
        let out = resource_binding_prompt(&binding(
            ResourceKind::Outputs,
            "/mnt/session/outputs",
            ResourceAccess::ReadWrite,
        ));
        assert!(out.contains("/mnt/session/outputs") && out.contains("artifacts"));

        // memory: read/write access is named, and per-binding instructions append.
        let mut mem = binding(
            ResourceKind::MemoryStore,
            "/mnt/memory/prefs",
            ResourceAccess::ReadWrite,
        );
        assert!(resource_binding_prompt(&mem).contains("read/write"));
        mem.instructions = Some("user preferences".to_string());
        let rendered = resource_binding_prompt(&mem);
        assert!(rendered.contains("/mnt/memory/prefs"));
        assert!(rendered.ends_with("user preferences"));

        // repo + file + skill each name their path.
        assert!(
            resource_binding_prompt(&binding(
                ResourceKind::GithubRepository,
                "/workspace/repo",
                ResourceAccess::ReadWrite,
            ))
            .contains("/workspace/repo")
        );
        assert!(
            resource_binding_prompt(&binding(
                ResourceKind::File,
                "/workspace/data.csv",
                ResourceAccess::ReadOnly,
            ))
            .contains("read-only")
        );
        assert!(
            resource_binding_prompt(&binding(
                ResourceKind::Skill,
                "/mnt/skills/xlsx",
                ResourceAccess::ReadOnly,
            ))
            .contains("/mnt/skills/xlsx")
        );
    }

    #[test]
    fn resource_prompts_for_maps_each_binding_in_order() {
        // Empty bindings → empty fragments (unbound agent compiles byte-identically).
        let empty = AgentResourceConfig {
            agent_id: "a".into(),
            resources: vec![],
            version: 1,
        };
        assert!(resource_prompts_for(&empty).is_empty());

        let cfg = AgentResourceConfig {
            agent_id: "a".into(),
            resources: vec![
                binding(ResourceKind::File, "/w/a.csv", ResourceAccess::ReadOnly),
                binding(
                    ResourceKind::Outputs,
                    "/mnt/session/outputs",
                    ResourceAccess::ReadWrite,
                ),
            ],
            version: 1,
        };
        let prompts = resource_prompts_for(&cfg);
        assert_eq!(prompts.len(), 2);
        assert!(prompts[0].contains("/w/a.csv"));
        assert!(prompts[1].contains("/mnt/session/outputs"));
    }
    use awaken_model_catalog::{
        Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
    };
    use std::collections::HashMap;

    fn catalog() -> ProviderCatalog {
        let mut c = ProviderCatalog::default();
        c.providers.insert(
            "anthropic".into(),
            Provider {
                id: ProviderId::new("anthropic"),
                slug: "anthropic".into(),
                display_name: "Anthropic".into(),
                version: 1,
            },
        );
        c.endpoints.insert(
            "ep1".into(),
            ProtocolEndpoint {
                id: ProtocolEndpointId::new("ep1"),
                provider_id: ProviderId::new("anthropic"),
                flavor: ModelApiCompat::AnthropicMessages,
                base_url: Some("https://api.anthropic.com".into()),
                timeout_secs: 300,
                display_name: "prod".into(),
                version: 1,
            },
        );
        c.offerings.push(Offering {
            model_id: "claude-opus-4-8".into(),
            provider_id: ProviderId::new("anthropic"),
            protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
            flavor: ModelApiCompat::AnthropicMessages,
            upstream_model: None,
        });
        c
    }

    #[tokio::test]
    async fn resolves_triple_and_materializes_credential() {
        let catalog = catalog();
        let store = InMemorySecretStore::new();
        let source = create_source(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: Some("ANTHROPIC_API_KEY".into()),
                secret: Some(RedactedString::new("sk-abc123")),
                oauth_command: None,
            },
            &store,
        )
        .await
        .unwrap();
        let mut sources = HashMap::new();
        sources.insert(source.id.0.clone(), source.clone());

        let resolved = resolve_inference(
            &catalog,
            "claude-opus-4-8",
            &CredentialBinding::Exact {
                credential_source_id: CredentialSourceId(source.id.0.clone()),
            },
            &sources,
            &store,
        )
        .await
        .unwrap();

        assert_eq!(resolved.triple.provider_id, "anthropic");
        assert_eq!(resolved.triple.flavor, ModelApiCompat::AnthropicMessages);
        assert_eq!(resolved.adapter_kind, "anthropic");
        assert_eq!(
            resolved.base_url.as_deref(),
            Some("https://api.anthropic.com")
        );
        assert_eq!(
            resolved.credential.as_ref().unwrap().expose_secret(),
            "sk-abc123"
        );
    }

    #[tokio::test]
    async fn unknown_model_fails_closed() {
        let store = InMemorySecretStore::new();
        let sources: HashMap<String, CredentialSource> = HashMap::new();
        let err = resolve_inference(
            &catalog(),
            "ghost-model",
            &CredentialBinding::None,
            &sources,
            &store,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ResolveError::ModelUnresolved(_)));
    }

    #[tokio::test]
    async fn run_input_snapshot_is_secret_free_while_credential_rides_alongside() {
        use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
        use awaken_runtime_contract::snapshot::{
            AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
        };

        let store = InMemorySecretStore::new();
        let source = create_source(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: None,
                secret: Some(RedactedString::new("sk-topsecret")),
                oauth_command: None,
            },
            &store,
        )
        .await
        .unwrap();
        let mut sources = HashMap::new();
        sources.insert(source.id.0.clone(), source.clone());

        let snapshot = ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snap1".into()),
            root_agent_id: AgentId("agent1".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("fp".into()),
                instructions: String::new(),
                max_steps: 8,
                model_binding: ModelBinding {
                    provider_identity_ref: "anthropic".into(),
                    model_ref: "claude-opus-4-8".into(),
                    backend_ref: "genai".into(),
                },
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
            },
            fingerprint: CatalogFingerprint("fp".into()),
        };

        let run = resolve_run(
            snapshot,
            &catalog(),
            &CredentialBinding::Exact {
                credential_source_id: CredentialSourceId(source.id.0.clone()),
            },
            &sources,
            &store,
        )
        .await
        .unwrap();

        // The persisted snapshot serializes with NO plaintext secret (D6/D9).
        let json = serde_json::to_string(&run.snapshot).unwrap();
        assert!(!json.contains("sk-topsecret"));
        // The credential rides in the non-serialized inference half, at the seam.
        assert_eq!(
            run.inference.credential.unwrap().expose_secret(),
            "sk-topsecret"
        );
        assert_eq!(run.inference.triple.model_id, "claude-opus-4-8");
    }

    #[test]
    fn axis_binding_pin_yields_one_candidate() {
        let axis = AxisBinding::Pin("m1".to_string());
        assert_eq!(axis.candidates(), vec!["m1".to_string()]);
        assert_eq!(axis.primary(), Some(&"m1".to_string()));
    }

    #[test]
    fn axis_binding_pool_preserves_order_and_primary_is_first() {
        let axis = AxisBinding::Pool(vec!["a".to_string(), "b".to_string(), "c".to_string()]);
        assert_eq!(axis.candidates(), vec!["a", "b", "c"]);
        assert_eq!(axis.primary(), Some(&"a".to_string()));
    }

    #[test]
    fn model_axis_is_a_pin_without_fallbacks_and_a_pool_with_them() {
        let single = InferenceProfile {
            model_id: "primary".into(),
            model_fallbacks: Vec::new(),
            credential_binding: CredentialBinding::None,
            disabled_endpoint_ids: Vec::new(),
        };
        assert_eq!(single.model_axis(), AxisBinding::Pin("primary".into()));

        let pooled = InferenceProfile {
            model_id: "primary".into(),
            model_fallbacks: vec!["backup1".into(), "backup2".into()],
            credential_binding: CredentialBinding::None,
            disabled_endpoint_ids: Vec::new(),
        };
        // The pinned model leads the pool, then fallbacks in order.
        assert_eq!(
            pooled.model_axis().candidates(),
            vec!["primary", "backup1", "backup2"]
        );
    }

    #[test]
    fn axis_binding_serde_is_tagged_snake_case() {
        let pin: AxisBinding<String> = AxisBinding::Pin("m".into());
        assert_eq!(
            serde_json::to_string(&pin).unwrap(),
            r#"{"kind":"pin","value":"m"}"#
        );
        // A profile row written before `model_fallbacks` existed still loads.
        let legacy = r#"{"model_id":"m","credential_binding":{"type":"none"}}"#;
        let profile: InferenceProfile = serde_json::from_str(legacy).unwrap();
        assert!(profile.model_fallbacks.is_empty());
        assert_eq!(profile.model_axis(), AxisBinding::Pin("m".into()));
    }

    #[tokio::test]
    async fn can_consume_gates_a_scoped_key_to_its_provider() {
        let store = InMemorySecretStore::new();
        let scoped = create_source(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("openai".into()),
                env_key: None,
                secret: Some(RedactedString::new("sk-openai")),
                oauth_command: None,
            },
            &store,
        )
        .await
        .unwrap();
        assert!(can_consume("openai", &scoped));
        assert!(!can_consume("anthropic", &scoped));

        let unscoped = create_source(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Env,
                provider_id: None,
                env_key: Some("KEY".into()),
                secret: Some(RedactedString::new("sk-any")),
                oauth_command: None,
            },
            &store,
        )
        .await
        .unwrap();
        // Host-native / unscoped consumes any provider.
        assert!(can_consume("anthropic", &unscoped));
        assert!(can_consume("openai", &unscoped));
    }

    #[tokio::test]
    async fn exact_binding_with_an_incompatible_key_fails_closed() {
        let store = InMemorySecretStore::new();
        let openai = create_source(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("openai".into()),
                env_key: None,
                secret: Some(RedactedString::new("sk-openai")),
                oauth_command: None,
            },
            &store,
        )
        .await
        .unwrap();
        let mut sources = HashMap::new();
        sources.insert(openai.id.0.clone(), openai.clone());

        // catalog()'s offering is provider `anthropic`; an openai-scoped key must
        // not authenticate it.
        let err = resolve_inference(
            &catalog(),
            "claude-opus-4-8",
            &CredentialBinding::Exact {
                credential_source_id: CredentialSourceId(openai.id.0.clone()),
            },
            &sources,
            &store,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ResolveError::IncompatibleCredential { .. }));
    }

    #[test]
    fn cooldown_deadline_only_fires_for_quota_and_honors_the_retry_hint() {
        use awaken_runtime_contract::resilience::Disposition;
        use std::time::Duration;

        // A 429 with a Retry-After cools until now + that hint.
        assert_eq!(
            cooldown_deadline(
                Disposition::Quota {
                    retry_after: Some(Duration::from_secs(30))
                },
                1_000
            ),
            Some(31_000)
        );
        // A quota signal without a hint uses the default 60s window.
        assert_eq!(
            cooldown_deadline(Disposition::Quota { retry_after: None }, 1_000),
            Some(61_000)
        );
        // Transient / permanent failures never cool the identity.
        assert_eq!(cooldown_deadline(Disposition::Transient, 1_000), None);
        assert_eq!(cooldown_deadline(Disposition::Permanent, 1_000), None);
    }
}
