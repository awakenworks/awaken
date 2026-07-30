//! Config resolver (ADR-0043) — the management plane's *read/resolution face*.
//! It owns **no aggregate**; it reads the config stores (`awaken-model-catalog`,
//! `awaken-credential-vault`, and — in the assembly — agent config). Agent
//! publication owns the separate secret-free model-candidate resolver; the
//! concrete [`ResolvedInference`] path remains for management preview/probe operations.
//!
//! This is the crate formerly mislabeled "inference": it *resolves* config into
//! an executable binding; it does **not** run inference (that is
//! `awaken-provider-genai`). Execution never depends on this crate (D6/D9 / I4).
//!
//! P0: single offering / `Exact` credential / `Derive` endpoint. Pools, multi-tier
//! `InferenceProfile` failover, and `Pin` are P1.

#![forbid(unsafe_code)]

pub use awaken_agent_contract::ModelTarget;
use awaken_agent_contract::RedactedString;
use awaken_credential_vault::{
    AvailabilityLedger, CredentialBinding, CredentialError, CredentialSource, SecretRef,
    SecretStore,
};
use awaken_model_catalog::{ApiDialect, ProviderCatalog};
pub use awaken_resource_contract::{
    BindingId, FileId, InputBinding, InputResourceId, MemoryStoreId, RepositoryId, ResourceAccess,
};

mod credential_selection;
mod reference_stores;
pub use credential_selection::{
    CredentialCandidateSet, can_consume, credential_can_supply, credential_candidates,
    derive_vendor_pool,
};
/// Read ports for authored aggregates. The runtime host reads through these
/// application contracts without depending on the authoring HTTP crate.
pub mod stores;
/// Telemetry ceiling composition (ADR-0050 D3): Org baseline tightened by lower layers.
pub mod telemetry;
pub use reference_stores::{
    InMemoryAgentInputBindingRepository, InMemoryProfileStore, InMemoryWebhookStore,
};
pub use stores::{
    AgentInputBindingRepository, AgentInputRepositoryError, ConfigRepositoryError,
    InferenceProfileStore, WebhookStore, get_workspace_profile, put_workspace_profile,
    validate_agent_input_revision, workspace_profile_key,
};
pub use telemetry::{RedactionMode, TelemetryCeiling};

/// The resolved execution unit: *(model × credential-identity × provider ×
/// dialect)*. Mirrors awaken-management-contract's `InferenceTriple`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InferenceTriple {
    pub model_id: String,
    pub provider_id: String,
    pub protocol_endpoint_id: String,
    pub dialect: ApiDialect,
}

/// What the resolver hands the run loop: the concrete target + wire + an
/// already-resolved secret for management preview/probe. Production publication
/// emits complete secret-free model candidates; runtime never consumes this preview type.
#[derive(Debug)]
pub struct ResolvedInference {
    pub triple: InferenceTriple,
    /// The adapter kind that speaks this dialect (`anthropic`/`openai`/…).
    pub adapter_kind: &'static str,
    /// Endpoint base URL override, if any.
    pub base_url: Option<String>,
    /// The already-materialized provider credential; `None` only when the authored
    /// binding explicitly requires no credential.
    pub credential: Option<RedactedString>,
}

/// A resolution failure (fail-closed).
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no offering for model `{0}` (model reference did not resolve — fail closed)")]
    ModelUnresolved(String),
    #[error(
        "model `{model_id}` matches multiple offerings ({candidates:?}); select a provider and endpoint"
    )]
    ModelAmbiguous {
        model_id: String,
        candidates: Vec<String>,
    },
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

/// Pure catalog-selection failures shared by preview, publication, and model
/// directory projections. No caller is allowed to implement its own `.find()`
/// precedence for Offerings.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OfferingSelectionError {
    #[error("model `{model_id}` has no active matching offering")]
    NotFound { model_id: String },
    #[error("model `{model_id}` matches multiple offerings ({candidates:?})")]
    Ambiguous {
        model_id: String,
        candidates: Vec<String>,
    },
    #[error("model target cannot combine endpoint_name and protocol_endpoint_id")]
    ConflictingEndpointQualifiers,
}

/// Select exactly one active Offering for a target.
///
/// Endpoint names match the suffix of the canonical
/// `<provider>.<dialect>.<name>` endpoint id. An exact endpoint id always stays
/// exact. A zero or multi-match result fails closed.
pub fn select_offering<'a>(
    catalog: &'a ProviderCatalog,
    target: &ModelTarget,
    disabled_endpoints: &[String],
) -> Result<&'a awaken_model_catalog::Offering, OfferingSelectionError> {
    if target.protocol_endpoint_id.is_some() && target.endpoint_name.is_some() {
        return Err(OfferingSelectionError::ConflictingEndpointQualifiers);
    }
    let matches = catalog
        .offerings
        .iter()
        .filter(|offering| {
            offering.status == awaken_model_catalog::OfferingStatus::Active
                && offering.model_id == target.model_id
                && target
                    .provider_id
                    .as_deref()
                    .is_none_or(|provider| offering.provider_id.as_str() == provider)
                && target
                    .protocol_endpoint_id
                    .as_deref()
                    .is_none_or(|endpoint| offering.protocol_endpoint_id.as_str() == endpoint)
                && target.endpoint_name.as_deref().is_none_or(|name| {
                    offering.protocol_endpoint_id.as_str() == name
                        || offering
                            .protocol_endpoint_id
                            .as_str()
                            .strip_suffix(name)
                            .is_some_and(|prefix| prefix.ends_with('.'))
                })
                && !disabled_endpoints.contains(&offering.protocol_endpoint_id.0)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [offering] => Ok(*offering),
        [] => Err(OfferingSelectionError::NotFound {
            model_id: target.model_id.clone(),
        }),
        offerings => Err(OfferingSelectionError::Ambiguous {
            model_id: target.model_id.clone(),
            candidates: offerings
                .iter()
                .map(|offering| {
                    format!("{}/{}", offering.provider_id, offering.protocol_endpoint_id)
                })
                .collect(),
        }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ExecutableModelReadiness {
    Ready,
    OfferingUnavailable,
    CredentialUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ExecutableModelOption {
    pub provider_id: String,
    pub model_id: String,
    pub endpoint_id: String,
    pub readiness: ExecutableModelReadiness,
}

/// Pure Catalog × Credential evaluator shared by config reads and Managed
/// model-directory projection. Publication performs the stateful credential
/// choice and revision fence after this side-effect-free readiness check.
#[must_use]
pub fn project_executable_models(
    catalog: &ProviderCatalog,
    credentials: &[CredentialSource],
    backend_ref: &str,
) -> Vec<ExecutableModelOption> {
    let mut options = catalog
        .offerings
        .iter()
        .map(|offering| {
            let readiness = if offering.status != awaken_model_catalog::OfferingStatus::Active {
                ExecutableModelReadiness::OfferingUnavailable
            } else if credentials.iter().any(|credential| {
                credential.status == awaken_credential_vault::CredentialStatus::Active
                    && credential.is_executable_origin()
                    && credential_can_supply(offering.provider_id.as_str(), backend_ref, credential)
            }) {
                ExecutableModelReadiness::Ready
            } else {
                ExecutableModelReadiness::CredentialUnavailable
            };
            ExecutableModelOption {
                provider_id: offering.provider_id.0.clone(),
                model_id: offering.model_id.clone(),
                endpoint_id: offering.protocol_endpoint_id.0.clone(),
                readiness,
            }
        })
        .collect::<Vec<_>>();
    options.sort_by(|left, right| {
        (&left.model_id, &left.provider_id, &left.endpoint_id).cmp(&(
            &right.model_id,
            &right.provider_id,
            &right.endpoint_id,
        ))
    });
    options
}

#[cfg(test)]
mod offering_selection_tests {
    use super::*;
    use awaken_model_catalog::{
        ApiDialect, Offering, OfferingSource, OfferingStatus, ProtocolEndpointId, ProviderId,
    };

    fn offering(provider: &str, endpoint: &str) -> Offering {
        Offering {
            model_id: "shared/model".into(),
            provider_id: ProviderId::new(provider),
            protocol_endpoint_id: ProtocolEndpointId::new(endpoint),
            dialect: ApiDialect::OpenAiChat,
            upstream_model: None,
            source: OfferingSource::Manual,
            status: OfferingStatus::Active,
            last_seen_at_unix_ms: None,
        }
    }

    #[test]
    fn catalog_selector_fails_closed_and_honors_each_qualifier() {
        // Causes: C1 model exists; C2 provider qualifier; C3 endpoint-name
        // qualifier; C4 exact endpoint qualifier; C5 multiple active matches.
        // Effects: E1 one Offering; E2 not-found; E3 ambiguous; E4 invalid.
        // Rules: no qualifier+C5 -> E3; provider narrows to one -> E1;
        // provider+name -> E1; exact endpoint -> E1; both endpoint forms -> E4.
        let catalog = ProviderCatalog {
            offerings: vec![
                offering("anyrouter", "anyrouter.open_ai_chat.primary"),
                offering("qwen", "qwen.open_ai_chat"),
            ],
            ..ProviderCatalog::default()
        };
        assert!(matches!(
            select_offering(&catalog, &ModelTarget::unqualified("shared/model"), &[]),
            Err(OfferingSelectionError::Ambiguous { .. })
        ));
        let provider = ModelTarget {
            model_id: "shared/model".into(),
            provider_id: Some("qwen".into()),
            protocol_endpoint_id: None,
            endpoint_name: None,
        };
        assert_eq!(
            select_offering(&catalog, &provider, &[])
                .unwrap()
                .provider_id
                .as_str(),
            "qwen"
        );
        let named = ModelTarget {
            provider_id: Some("anyrouter".into()),
            endpoint_name: Some("primary".into()),
            ..ModelTarget::unqualified("shared/model")
        };
        assert_eq!(
            select_offering(&catalog, &named, &[])
                .unwrap()
                .protocol_endpoint_id
                .as_str(),
            "anyrouter.open_ai_chat.primary"
        );
        let conflicting = ModelTarget {
            protocol_endpoint_id: Some("anyrouter.open_ai_chat.primary".into()),
            ..named
        };
        assert_eq!(
            select_offering(&catalog, &conflicting, &[]),
            Err(OfferingSelectionError::ConflictingEndpointQualifiers)
        );
    }
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
/// Picks the first offering for `model_id` (`Offering(model) ∩ dialect`), the given
/// binding, and materializes its credential. Endpoint selection honors no toggles;
/// use [`resolve_profile`] to skip endpoints an operator disabled.
pub async fn resolve_inference(
    catalog: &ProviderCatalog,
    model_id: &str,
    binding: &CredentialBinding,
    sources: &dyn SourceLookup,
    secret_store: &dyn SecretStore,
) -> Result<ResolvedInference, ResolveError> {
    resolve_inference_target(
        catalog,
        &ModelTarget::unqualified(model_id),
        &[],
        binding,
        sources,
        secret_store,
    )
    .await
}

/// The core resolution, with an operator's disabled-endpoint toggle applied: an
/// offering whose endpoint id is in `disabled_endpoints` is skipped, so a
/// `(credential × interface)` an operator turned off is never selected.
pub async fn resolve_inference_target(
    catalog: &ProviderCatalog,
    target: &ModelTarget,
    disabled_endpoints: &[String],
    binding: &CredentialBinding,
    sources: &dyn SourceLookup,
    secret_store: &dyn SecretStore,
) -> Result<ResolvedInference, ResolveError> {
    let offering =
        select_offering(catalog, target, disabled_endpoints).map_err(|error| match error {
            OfferingSelectionError::NotFound { model_id } => {
                ResolveError::ModelUnresolved(model_id)
            }
            OfferingSelectionError::ConflictingEndpointQualifiers => {
                ResolveError::ModelUnresolved(target.model_id.clone())
            }
            OfferingSelectionError::Ambiguous {
                model_id,
                candidates,
            } => ResolveError::ModelAmbiguous {
                model_id,
                candidates,
            },
        })?;

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
        dialect: offering.dialect,
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
        // The inference path carries no workspace parameter; the pool fence
        // (member vs pool workspace) is intrinsic and always applied.
        None,
    )
    .await?;

    Ok(ResolvedInference {
        triple,
        adapter_kind: endpoint.dialect.adapter_kind(),
        base_url: endpoint.base_url.clone(),
        credential,
    })
}

/// An authored "how to run this model" unit (ADR-0043 `InferenceProfile` /
/// oversight-next `ProviderIdentity`): it names an exact primary and ordered
/// fallback candidates, each with a vault-backed (never inline) credential
/// binding, plus endpoints the operator has toggled off. The resolver reads it —
/// it is never flowed into the runtime.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct InferenceProfile {
    /// Owning workspace, stamped by the trusted configuration edge. An empty
    /// value represents an unstamped domain value and is never treated as owned.
    #[serde(default)]
    pub workspace_id: String,
    /// Exact preferred offering. Provider and endpoint qualifiers prevent a model
    /// id shared by BYOK and Cloud sources from becoming ambiguous at runtime.
    pub primary: ProfileCandidate,
    /// Explicit alternates, tried in authored order. The resolver never appends an
    /// implicit Cloud, BYOK, or local fallback.
    #[serde(default)]
    pub fallbacks: Vec<ProfileCandidate>,
    #[serde(default)]
    pub disabled_endpoint_ids: Vec<String>,
}

/// One explicit step in a profile's failover chain. Binding credentials per
/// target allows a BYOK primary and Cloud fallback (or the reverse) without ever
/// guessing which identity may authenticate which provider.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProfileCandidate {
    pub target: ModelTarget,
    pub credential_binding: CredentialBinding,
}

impl InferenceProfile {
    /// The candidate axis as an ordered [`AxisBinding`]: a lone primary is a
    /// [`Pin`](AxisBinding::Pin); primary plus fallbacks is a
    /// [`Pool`](AxisBinding::Pool) in explicit try-order.
    #[must_use]
    pub fn model_axis(&self) -> AxisBinding<ProfileCandidate> {
        if self.fallbacks.is_empty() {
            AxisBinding::Pin(self.primary.clone())
        } else {
            let mut models = Vec::with_capacity(self.fallbacks.len() + 1);
            models.push(self.primary.clone());
            models.extend(self.fallbacks.iter().cloned());
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
    resolve_inference_target(
        catalog,
        &profile.primary.target,
        &profile.disabled_endpoint_ids,
        &profile.primary.credential_binding,
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
    for candidate in profile.model_axis().candidates() {
        match resolve_inference_target(
            catalog,
            &candidate.target,
            &profile.disabled_endpoint_ids,
            &candidate.credential_binding,
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
        return Err(last_err.unwrap_or_else(|| {
            ResolveError::ModelUnresolved(profile.primary.target.model_id.clone())
        }));
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
/// may only consume that provider's offerings; an unscoped material source
/// (`provider_id = None`, explicitly unscoped persisted Vault/OAuth source) may
/// consume any. A backend-owned WorkerLocal identity without a provider scope is
/// never provider material: it authenticates its own driver instead. This is what
/// stops an otherwise-materializable key or local CLI login being paired with a
/// model it cannot authenticate — the invalid `(model × credential)` combination
/// the ADR calls out.
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
    expected_workspace: Option<&str>,
) -> Result<Option<RedactedString>, ResolveError> {
    match credential_candidates(
        binding,
        sources,
        offering_provider,
        None,
        availability,
        expected_workspace,
        0,
    )? {
        // Brokered access has no locally materializable Provider secret. The
        // management preview resolves the public model/protocol shape only; the
        // runtime broker materializer performs live entitlement admission.
        CredentialCandidateSet::None | CredentialCandidateSet::Brokered => Ok(None),
        CredentialCandidateSet::Direct {
            sources,
            pool_id: None,
            ..
        } => {
            let source = sources
                .into_iter()
                .next()
                .expect("an Exact binding always produces one candidate");
            Ok(Some(
                awaken_credential_vault::materialize(source, secret_store).await?,
            ))
        }
        CredentialCandidateSet::Direct {
            sources,
            pool_id: Some(pool_id),
            total,
            cooled,
        } => {
            // Try members in eligible order; skip a member whose source is absent,
            // incompatible with the provider, or fails to materialize, so one bad key
            // does not fail the run.
            for source in sources {
                if let Ok(secret) = awaken_credential_vault::materialize(source, secret_store).await
                {
                    return Ok(Some(secret));
                }
            }
            Err(ResolveError::NoEligibleCredential {
                pool_id,
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

/// An authored webhook endpoint (ADR-0048): a workspace-scoped subscription that
/// receives signed lifecycle events. A management config resource alongside
/// [`InferenceProfile`], so it shares the admin store, the tenant
/// fence, and the secret-free invariant — the `whsec_` signing key is NOT on the
/// row; it is sealed in the [`SecretStore`] and reached by [`secret_ref`], resolved
/// only at dispatch. Unlike a provider credential it is a bare sealed secret (a
/// symmetric signing key), not a [`CredentialBinding`]/[`CredentialSource`].
///
/// [`secret_ref`]: WebhookEndpointDef::secret_ref
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WebhookEndpointDef {
    pub id: String,
    /// The owning workspace — events for this workspace only reach this endpoint.
    /// Carried on the row because dispatch enumerates by workspace, not by id.
    pub workspace_id: String,
    /// The HTTPS endpoint the signed payload is POSTed to.
    pub url: String,
    /// The event types this endpoint receives; empty = all types.
    pub event_types: Vec<String>,
    /// Delivery suspended (manual, or auto after repeated failures).
    pub disabled: bool,
    /// Handle to the sealed `whsec_` signing secret in the [`SecretStore`] — never
    /// the secret itself, never echoed after create.
    pub secret_ref: SecretRef,
}

impl WebhookEndpointDef {
    /// Whether this endpoint wants `event_type` (empty `event_types` = all).
    #[must_use]
    pub fn wants(&self, event_type: &str) -> bool {
        self.event_types.is_empty() || self.event_types.iter().any(|t| t == event_type)
    }
}

/// An Agent's authored default inputs. The repository supplies Workspace as the
/// aggregate key, so this value contains only Agent-local configuration. The same
/// typed [`InputBinding`] language is used by Session attachments; no authorization
/// subject, policy, API key, secret, content pin, Project, or WorkUnit enters it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentInputConfig {
    pub agent_id: String,
    /// Exact Environment revision selected as this Agent's Session default.
    /// It is secret-free and shares the same CAS revision as Resource bindings,
    /// so callers cannot observe a mixed default bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<AgentEnvironmentBinding>,
    #[cfg_attr(feature = "schema", schemars(with = "Vec<InputBindingSchema>"))]
    pub inputs: Vec<InputBinding>,
    pub revision: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentEnvironmentBinding {
    pub environment_id: String,
    pub revision: u64,
}

// JSON Schema is an API representation concern, not part of the resource-domain
// contract. Keep these private shadows at the configuration boundary so the
// resource contract does not depend on schemars (or any HTTP/OpenAPI tooling).
#[cfg(feature = "schema")]
#[allow(dead_code, reason = "type-only JSON Schema projection")]
#[derive(schemars::JsonSchema)]
#[schemars(rename = "InputBinding")]
struct InputBindingSchema {
    binding_id: String,
    target: InputResourceIdSchema,
    mount_path: String,
    access: ResourceAccessSchema,
    instructions: Option<String>,
}

#[cfg(feature = "schema")]
#[allow(dead_code, reason = "type-only JSON Schema projection")]
#[derive(schemars::JsonSchema)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
#[schemars(rename = "InputResourceId")]
enum InputResourceIdSchema {
    File(String),
    MemoryStore(String),
    Repository(String),
}

#[cfg(feature = "schema")]
#[allow(dead_code, reason = "type-only JSON Schema projection")]
#[derive(schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "ResourceAccess")]
enum ResourceAccessSchema {
    ReadOnly,
    ReadWrite,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum AgentInputConfigWire {
    Canonical {
        agent_id: String,
        #[serde(default)]
        environment: Option<AgentEnvironmentBinding>,
        inputs: Vec<InputBinding>,
        revision: i64,
    },
    Legacy {
        agent_id: String,
        resources: Vec<LegacyResourceBinding>,
        version: i64,
    },
}

#[derive(serde::Deserialize)]
struct LegacyResourceBinding {
    kind: LegacyResourceKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    resource_id: String,
    mount_path: String,
    access: ResourceAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyResourceKind {
    Outputs,
    File,
    MemoryStore,
    GithubRepository,
    Skill,
}

impl<'de> serde::Deserialize<'de> for AgentInputConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        match AgentInputConfigWire::deserialize(deserializer)? {
            AgentInputConfigWire::Canonical {
                agent_id,
                environment,
                inputs,
                revision,
            } => Ok(Self {
                agent_id,
                environment,
                inputs,
                revision,
            }),
            AgentInputConfigWire::Legacy {
                agent_id,
                resources,
                version,
            } => {
                let mut inputs = Vec::with_capacity(resources.len());
                for (index, resource) in resources.into_iter().enumerate() {
                    let target = match resource.kind {
                        LegacyResourceKind::File => {
                            InputResourceId::File(FileId::from(resource.resource_id))
                        }
                        LegacyResourceKind::MemoryStore => {
                            InputResourceId::MemoryStore(MemoryStoreId::from(resource.resource_id))
                        }
                        LegacyResourceKind::GithubRepository => {
                            InputResourceId::Repository(RepositoryId::from(resource.resource_id))
                        }
                        LegacyResourceKind::Outputs | LegacyResourceKind::Skill => {
                            return Err(D::Error::custom(
                                "legacy outputs/skill resource bindings are not Agent inputs; migrate outputs to the Environment and skills to Agent skills",
                            ));
                        }
                    };
                    let access = if matches!(target, InputResourceId::File(_)) {
                        ResourceAccess::ReadOnly
                    } else {
                        resource.access
                    };
                    inputs.push(InputBinding {
                        binding_id: BindingId::new(format!("agent:{agent_id}:input:{index}")),
                        target,
                        mount_path: resource.mount_path,
                        access,
                        instructions: resource.instructions,
                    });
                }
                Ok(Self {
                    agent_id,
                    environment: None,
                    inputs,
                    revision: version,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_vault::repo::{InMemoryCredentialRepo, ensure_worker_local};
    use awaken_credential_vault::{
        CredentialCreateParams, CredentialKind, CredentialSourceId, InMemorySecretStore,
        WorkerLocalBinding, create_source,
    };

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
                dialect: ApiDialect::AnthropicMessages,
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
            dialect: ApiDialect::AnthropicMessages,
            upstream_model: None,
            source: Default::default(),
            status: Default::default(),
            last_seen_at_unix_ms: None,
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
        assert_eq!(resolved.triple.dialect, ApiDialect::AnthropicMessages);
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
    async fn unqualified_duplicate_model_is_ambiguous_and_endpoint_target_is_exact() {
        let mut cat = catalog();
        let mut endpoint = cat.endpoints["ep1"].clone();
        endpoint.id = awaken_model_catalog::ProtocolEndpointId::new("ep2");
        endpoint.display_name = "backup".into();
        cat.endpoints.insert("ep2".into(), endpoint);
        let mut offering = cat.offerings[0].clone();
        offering.protocol_endpoint_id = awaken_model_catalog::ProtocolEndpointId::new("ep2");
        cat.offerings.push(offering);
        let store = InMemorySecretStore::new();
        let sources: HashMap<String, CredentialSource> = HashMap::new();

        let error = resolve_inference(
            &cat,
            "claude-opus-4-8",
            &CredentialBinding::None,
            &sources,
            &store,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ResolveError::ModelAmbiguous { .. }));

        let resolved = resolve_inference_target(
            &cat,
            &ModelTarget {
                model_id: "claude-opus-4-8".into(),
                provider_id: Some("anthropic".into()),
                protocol_endpoint_id: Some("ep2".into()),
                endpoint_name: None,
            },
            &[],
            &CredentialBinding::None,
            &sources,
            &store,
        )
        .await
        .unwrap();
        assert_eq!(resolved.triple.protocol_endpoint_id, "ep2");
    }

    #[tokio::test]
    async fn preview_resolution_does_not_mutate_secret_free_snapshot() {
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
            metadata: Default::default(),
            root_agent_id: AgentId("agent1".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("fp".into()),
                instructions: String::new(),
                max_steps: 8,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding {
                        provider_identity_ref: "anthropic".into(),
                        model_ref: "claude-opus-4-8".into(),
                        backend_ref: "genai".into(),
                    },
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("fp".into()),
        };

        let inference = resolve_inference(
            &catalog(),
            &snapshot.resolved_spec.model_binding.model_ref,
            &CredentialBinding::Exact {
                credential_source_id: CredentialSourceId(source.id.0.clone()),
            },
            &sources,
            &store,
        )
        .await
        .unwrap();

        // The persisted snapshot serializes with NO plaintext secret (D6/D9).
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("sk-topsecret"));
        assert_eq!(
            inference.credential.unwrap().expose_secret(),
            "sk-topsecret"
        );
        assert_eq!(inference.triple.model_id, "claude-opus-4-8");
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
            workspace_id: "ws".into(),
            primary: ProfileCandidate {
                target: ModelTarget::unqualified("primary"),
                credential_binding: CredentialBinding::None,
            },
            fallbacks: Vec::new(),
            disabled_endpoint_ids: Vec::new(),
        };
        assert_eq!(
            single.model_axis(),
            AxisBinding::Pin(ProfileCandidate {
                target: ModelTarget::unqualified("primary"),
                credential_binding: CredentialBinding::None,
            })
        );

        let pooled = InferenceProfile {
            workspace_id: "ws".into(),
            primary: ProfileCandidate {
                target: ModelTarget::unqualified("primary"),
                credential_binding: CredentialBinding::None,
            },
            fallbacks: vec![
                ProfileCandidate {
                    target: ModelTarget::unqualified("backup1"),
                    credential_binding: CredentialBinding::None,
                },
                ProfileCandidate {
                    target: ModelTarget::unqualified("backup2"),
                    credential_binding: CredentialBinding::None,
                },
            ],
            disabled_endpoint_ids: Vec::new(),
        };
        // The pinned model leads the pool, then fallbacks in order.
        assert_eq!(
            pooled.model_axis().candidates(),
            vec![
                ProfileCandidate {
                    target: ModelTarget::unqualified("primary"),
                    credential_binding: CredentialBinding::None,
                },
                ProfileCandidate {
                    target: ModelTarget::unqualified("backup1"),
                    credential_binding: CredentialBinding::None,
                },
                ProfileCandidate {
                    target: ModelTarget::unqualified("backup2"),
                    credential_binding: CredentialBinding::None,
                },
            ]
        );
    }

    #[test]
    fn axis_binding_serde_is_tagged_snake_case() {
        let pin: AxisBinding<String> = AxisBinding::Pin("m".into());
        assert_eq!(
            serde_json::to_string(&pin).unwrap(),
            r#"{"kind":"pin","value":"m"}"#
        );
    }

    #[test]
    fn inference_profile_rejects_the_retired_flat_contract() {
        let legacy = r#"{"model_id":"m","credential_binding":{"type":"none"}}"#;
        let error = serde_json::from_str::<InferenceProfile>(legacy).unwrap_err();
        assert!(error.to_string().contains("unknown field `model_id`"));
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
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: Some("KEY".into()),
                secret: Some(RedactedString::new("sk-any")),
                oauth_command: None,
            },
            &store,
        )
        .await
        .unwrap();
        // An explicitly unscoped persisted source consumes any provider.
        assert!(can_consume("anthropic", &unscoped));
        assert!(can_consume("openai", &unscoped));

        let backend_login = ensure_worker_local(
            &InMemoryCredentialRepo::new(),
            "ws",
            WorkerLocalBinding::new("acp:claude", "default"),
            None,
        )
        .await
        .unwrap();
        assert!(
            !can_consume("anthropic", &backend_login),
            "a CLI-owned login is not an unscoped provider secret"
        );
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

    // ---- CEG 02: resolve_credential unit cases (R1–R10) ----
    use awaken_credential_vault::{
        CredentialPool, CredentialPoolId, CredentialPoolMember, CredentialStatus, SelectionPolicy,
    };

    /// A lookup exposing individual sources plus one pool (mirrors the tests/ ctx).
    struct PoolCtx {
        sources: HashMap<String, CredentialSource>,
        pool: CredentialPool,
    }
    impl SourceLookup for PoolCtx {
        fn get(&self, id: &str) -> Option<&CredentialSource> {
            self.sources.get(id)
        }
        fn get_pool(&self, id: &str) -> Option<&CredentialPool> {
            (self.pool.id.0 == id).then_some(&self.pool)
        }
    }

    /// A source that exists but is `Disabled` — present for lookup, fails to materialize.
    fn disabled_source(id: &str) -> CredentialSource {
        CredentialSource {
            id: CredentialSourceId(id.into()),
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: None,
            env_key: None,
            material_ref: None,
            auxiliary_material_refs: Default::default(),
            oauth_command: None,
            worker_local_binding: None,
            status: CredentialStatus::Disabled,
            version: 1,
        }
    }

    async fn vault_source(
        store: &InMemorySecretStore,
        provider: Option<&str>,
        secret: &str,
    ) -> CredentialSource {
        create_source(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: provider.map(str::to_string),
                env_key: None,
                secret: Some(RedactedString::new(secret)),
                oauth_command: None,
            },
            store,
        )
        .await
        .unwrap()
    }

    fn member(id: &str, ordinal: u32) -> CredentialPoolMember {
        CredentialPoolMember {
            credential_source_id: CredentialSourceId(id.into()),
            ordinal,
            enabled: true,
            selection_weight: 0,
        }
    }

    #[tokio::test]
    async fn resolve_credential_none_binding_yields_no_secret() {
        // R1: a None binding materializes nothing (even on the provider-gated path).
        let store = InMemorySecretStore::new();
        let sources: HashMap<String, CredentialSource> = HashMap::new();
        let got = resolve_credential(
            &CredentialBinding::None,
            &sources,
            &store,
            Some("anthropic"),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn resolve_credential_exact_missing_source_is_source_missing() {
        // R2: Exact naming an absent source fails closed with SourceMissing.
        let store = InMemorySecretStore::new();
        let sources: HashMap<String, CredentialSource> = HashMap::new();
        let err = resolve_credential(
            &CredentialBinding::Exact {
                credential_source_id: CredentialSourceId("ghost".into()),
            },
            &sources,
            &store,
            Some("anthropic"),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ResolveError::SourceMissing(id) if id == "ghost"));
    }

    #[tokio::test]
    async fn resolve_credential_exact_materialize_failure_propagates_credential_error() {
        // R4: the source is present and compatible (unscoped) so the gate passes, but
        // materialize fails — the vault CredentialError is propagated transparently,
        // never remapped to SourceMissing / IncompatibleCredential.
        let store = InMemorySecretStore::new();
        let mut sources = HashMap::new();
        sources.insert("s".to_string(), disabled_source("s"));
        let err = resolve_credential(
            &CredentialBinding::Exact {
                credential_source_id: CredentialSourceId("s".into()),
            },
            &sources,
            &store,
            Some("anthropic"),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            ResolveError::Credential(CredentialError::NotActive(_))
        ));
    }

    #[tokio::test]
    async fn resolve_credential_exhausted_pool_reports_total_and_zero_cooled() {
        // R8: every member unusable → NoEligibleCredential; with no ledger, cooled = 0
        // and total counts the enabled selection order.
        let store = InMemorySecretStore::new();
        let mut sources = HashMap::new();
        sources.insert("a".to_string(), disabled_source("a"));
        sources.insert("b".to_string(), disabled_source("b"));
        let ctx = PoolCtx {
            sources,
            pool: CredentialPool {
                id: CredentialPoolId("p".into()),
                workspace_id: "ws".into(),
                members: vec![member("a", 0), member("b", 1)],
                policy: SelectionPolicy::FirstHealthy,
            },
        };
        let err = resolve_credential(
            &CredentialBinding::OneOfCredentialPool {
                credential_pool_id: CredentialPoolId("p".into()),
            },
            &ctx,
            &store,
            Some("anthropic"),
            None,
            None,
        )
        .await
        .unwrap_err();
        match err {
            ResolveError::NoEligibleCredential {
                pool_id,
                total,
                cooled,
                over_capacity,
            } => {
                assert_eq!(pool_id, "p");
                assert_eq!(total, 2);
                assert_eq!(cooled, 0);
                assert_eq!(over_capacity, 0);
            }
            other => panic!("expected NoEligibleCredential, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_credential_pool_ledger_rotates_past_cooled_and_counts_them() {
        // R9: with a ledger, a cooled first member is dropped from eligible_order and
        // the next healthy member is selected; when all are cooled the count surfaces
        // as `cooled`; past the deadline the first member auto-resumes and wins again.
        let store = InMemorySecretStore::new();
        let cold = vault_source(&store, Some("anthropic"), "sk-first").await;
        let good = vault_source(&store, Some("anthropic"), "sk-second").await;
        let mut sources = HashMap::new();
        sources.insert(cold.id.0.clone(), cold.clone());
        sources.insert(good.id.0.clone(), good.clone());
        let ctx = PoolCtx {
            sources,
            pool: CredentialPool {
                id: CredentialPoolId("p".into()),
                workspace_id: "ws".into(),
                members: vec![member(&cold.id.0, 0), member(&good.id.0, 1)],
                policy: SelectionPolicy::FirstHealthy,
            },
        };
        let ledger = AvailabilityLedger::new();
        ledger.cool_down(&CredentialSourceId(cold.id.0.clone()), 10_000);

        let binding = CredentialBinding::OneOfCredentialPool {
            credential_pool_id: CredentialPoolId("p".into()),
        };

        // now < deadline: the cooled first member rotates out; the second is chosen.
        let got = resolve_credential(
            &binding,
            &ctx,
            &store,
            Some("anthropic"),
            Some((&ledger, 5_000)),
            None,
        )
        .await
        .unwrap();
        assert_eq!(got.unwrap().expose_secret(), "sk-second");

        // Cool the good one too: nothing eligible, cooled = 2 of total 2.
        ledger.cool_down(&CredentialSourceId(good.id.0.clone()), 10_000);
        let err = resolve_credential(
            &binding,
            &ctx,
            &store,
            Some("anthropic"),
            Some((&ledger, 5_000)),
            None,
        )
        .await
        .unwrap_err();
        match err {
            ResolveError::NoEligibleCredential { total, cooled, .. } => {
                assert_eq!(total, 2);
                assert_eq!(cooled, 2);
            }
            other => panic!("expected NoEligibleCredential, got {other:?}"),
        }

        // Past the deadline the first member auto-resumes and is preferred again.
        let resumed = resolve_credential(
            &binding,
            &ctx,
            &store,
            Some("anthropic"),
            Some((&ledger, 20_000)),
            None,
        )
        .await
        .unwrap();
        assert_eq!(resumed.unwrap().expose_secret(), "sk-first");
    }

    #[tokio::test]
    async fn resolve_credential_mcp_path_skips_compat_gate_for_scoped_key() {
        // R10: offering_provider = None (the MCP-server path) skips can_consume, so a
        // provider-scoped key materializes regardless of scope.
        let store = InMemorySecretStore::new();
        let scoped = vault_source(&store, Some("openai"), "sk-scoped").await;
        let mut sources = HashMap::new();
        sources.insert(scoped.id.0.clone(), scoped.clone());
        let got = resolve_credential(
            &CredentialBinding::Exact {
                credential_source_id: CredentialSourceId(scoped.id.0.clone()),
            },
            &sources,
            &store,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(got.unwrap().expose_secret(), "sk-scoped");
    }

    #[tokio::test]
    async fn resolve_credential_pool_skips_incompatible_member_and_selects_compatible() {
        // R (pool compat gate): a pool member scoped to a DIFFERENT provider than the
        // offering is skipped by can_consume, and the next provider-compatible member
        // is selected — a wrong-provider key never authenticates the run even from a
        // pool (the in-loop can_consume `continue`, distinct from disabled/absent).
        let store = InMemorySecretStore::new();
        let wrong = vault_source(&store, Some("openai"), "sk-openai").await;
        let right = vault_source(&store, Some("anthropic"), "sk-anthropic").await;
        let mut sources = HashMap::new();
        sources.insert(wrong.id.0.clone(), wrong.clone());
        sources.insert(right.id.0.clone(), right.clone());
        let ctx = PoolCtx {
            sources,
            pool: CredentialPool {
                id: CredentialPoolId("p".into()),
                workspace_id: "ws".into(),
                members: vec![member(&wrong.id.0, 0), member(&right.id.0, 1)],
                policy: SelectionPolicy::FirstHealthy,
            },
        };
        let got = resolve_credential(
            &CredentialBinding::OneOfCredentialPool {
                credential_pool_id: CredentialPoolId("p".into()),
            },
            &ctx,
            &store,
            Some("anthropic"),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(got.unwrap().expose_secret(), "sk-anthropic");
    }

    #[tokio::test]
    async fn resolve_credential_pool_all_incompatible_fails_closed() {
        // R (pool compat gate, exhausted): every member is scoped to a different
        // provider than the offering, so none can authenticate it → NoEligibleCredential
        // (fail closed), never a silent unauthenticated run and never a wrong-provider
        // key smuggled through the pool. cooled = 0 (no ledger; the exclusion is compat).
        let store = InMemorySecretStore::new();
        let a = vault_source(&store, Some("openai"), "sk-1").await;
        let b = vault_source(&store, Some("cohere"), "sk-2").await;
        let mut sources = HashMap::new();
        sources.insert(a.id.0.clone(), a.clone());
        sources.insert(b.id.0.clone(), b.clone());
        let ctx = PoolCtx {
            sources,
            pool: CredentialPool {
                id: CredentialPoolId("p".into()),
                workspace_id: "ws".into(),
                members: vec![member(&a.id.0, 0), member(&b.id.0, 1)],
                policy: SelectionPolicy::FirstHealthy,
            },
        };
        let err = resolve_credential(
            &CredentialBinding::OneOfCredentialPool {
                credential_pool_id: CredentialPoolId("p".into()),
            },
            &ctx,
            &store,
            Some("anthropic"),
            None,
            None,
        )
        .await
        .unwrap_err();
        match err {
            ResolveError::NoEligibleCredential { total, cooled, .. } => {
                assert_eq!(total, 2);
                assert_eq!(cooled, 0);
            }
            other => panic!("expected NoEligibleCredential, got {other:?}"),
        }
    }

    // ---- SEC: read-side workspace fence on credential materialization ----

    #[tokio::test]
    async fn resolve_credential_pool_skips_a_cross_workspace_member_and_fails_closed() {
        // FAIL-CLOSED: a CredentialPool owned by workspace A whose member references
        // a CredentialSource owned by workspace B must NOT materialize B's secret.
        // The read-side tenant fence skips the cross-workspace member like an absent
        // one; with nothing eligible left the pool errors NoEligibleCredential — the
        // pool's `workspace_id` is intrinsic, so this fence needs no extra parameter.
        let store = InMemorySecretStore::new();
        // The victim secret is owned by workspace B.
        let foreign = create_source(
            CredentialCreateParams {
                workspace_id: "wrkspc_b".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: None,
                secret: Some(RedactedString::new("sk-tenant-b-secret")),
                oauth_command: None,
            },
            &store,
        )
        .await
        .unwrap();
        let mut sources = HashMap::new();
        sources.insert(foreign.id.0.clone(), foreign.clone());
        // The pool is owned by workspace A, yet lists workspace B's source.
        let ctx = PoolCtx {
            sources,
            pool: CredentialPool {
                id: CredentialPoolId("p".into()),
                workspace_id: "wrkspc_a".into(),
                members: vec![member(&foreign.id.0, 0)],
                policy: SelectionPolicy::FirstHealthy,
            },
        };
        let err = resolve_credential(
            &CredentialBinding::OneOfCredentialPool {
                credential_pool_id: CredentialPoolId("p".into()),
            },
            &ctx,
            &store,
            Some("anthropic"),
            None,
            None,
        )
        .await
        .unwrap_err();
        // The cross-tenant member is excluded; the pool is exhausted, fail-closed.
        match err {
            ResolveError::NoEligibleCredential { pool_id, total, .. } => {
                assert_eq!(pool_id, "p");
                assert_eq!(total, 1, "the sole member was excluded by the tenant fence");
            }
            other => panic!("expected NoEligibleCredential, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_credential_exact_fences_a_cross_workspace_source() {
        // FAIL-CLOSED: an `Exact` binding naming a source owned by another workspace
        // is rejected as `SourceMissing` (no existence leak) when the caller supplies
        // the owning workspace — the foreign secret never materializes.
        let store = InMemorySecretStore::new();
        let foreign = create_source(
            CredentialCreateParams {
                workspace_id: "wrkspc_b".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: None,
                secret: Some(RedactedString::new("sk-exact-b")),
                oauth_command: None,
            },
            &store,
        )
        .await
        .unwrap();
        let mut sources = HashMap::new();
        sources.insert(foreign.id.0.clone(), foreign.clone());
        let err = resolve_credential(
            &CredentialBinding::Exact {
                credential_source_id: CredentialSourceId(foreign.id.0.clone()),
            },
            &sources,
            &store,
            Some("anthropic"),
            None,
            // The binding's owning workspace is A; the source belongs to B.
            Some("wrkspc_a"),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&err, ResolveError::SourceMissing(id) if *id == foreign.id.0),
            "a cross-workspace Exact source is fenced as SourceMissing (no leak), got {err:?}"
        );

        // Same-workspace Exact still materializes (the fence does not over-reach).
        let ok = resolve_credential(
            &CredentialBinding::Exact {
                credential_source_id: CredentialSourceId(foreign.id.0.clone()),
            },
            &sources,
            &store,
            Some("anthropic"),
            None,
            Some("wrkspc_b"),
        )
        .await
        .unwrap();
        assert_eq!(ok.unwrap().expose_secret(), "sk-exact-b");
    }

    // ---- CEG 02: resolve_inference_target core (A3) ----

    #[tokio::test]
    async fn resolve_inference_missing_endpoint_is_endpoint_missing() {
        // A3(b): the offering resolves but its endpoint id is absent from the catalog.
        let mut cat = catalog();
        cat.endpoints.clear();
        let store = InMemorySecretStore::new();
        let sources: HashMap<String, CredentialSource> = HashMap::new();
        let err = resolve_inference_target(
            &cat,
            &ModelTarget::unqualified("claude-opus-4-8"),
            &[],
            &CredentialBinding::None,
            &sources,
            &store,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ResolveError::EndpointMissing(id) if id == "ep1"));
    }

    #[tokio::test]
    async fn resolve_inference_uses_upstream_model_id_when_set() {
        // A3(d): upstream_model overrides the triple's model_id (the wire name sent
        // upstream may differ from the catalog model id).
        let mut cat = catalog();
        cat.offerings[0].upstream_model = Some("claude-opus-4-8-20990101".into());
        let store = InMemorySecretStore::new();
        let sources: HashMap<String, CredentialSource> = HashMap::new();
        let resolved = resolve_inference_target(
            &cat,
            &ModelTarget::unqualified("claude-opus-4-8"),
            &[],
            &CredentialBinding::None,
            &sources,
            &store,
        )
        .await
        .unwrap();
        assert_eq!(resolved.triple.model_id, "claude-opus-4-8-20990101");
        // The catalog-facing endpoint/provider are unchanged by the alias.
        assert_eq!(resolved.triple.provider_id, "anthropic");
    }

    #[tokio::test]
    async fn disabled_sole_endpoint_is_model_unresolved_not_endpoint_missing() {
        // A3(e) asymmetry: disabling the model's only offering endpoint filters the
        // offering out *before* endpoint lookup, so the model reads as unresolvable —
        // ModelUnresolved, NOT EndpointMissing.
        let cat = catalog();
        let store = InMemorySecretStore::new();
        let sources: HashMap<String, CredentialSource> = HashMap::new();
        let err = resolve_inference_target(
            &cat,
            &ModelTarget::unqualified("claude-opus-4-8"),
            &["ep1".to_string()],
            &CredentialBinding::None,
            &sources,
            &store,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ResolveError::ModelUnresolved(_)));
    }

    // ---- CEG 02: AxisBinding empty pool (A5d) ----

    #[test]
    fn axis_binding_empty_pool_has_no_primary_and_no_candidates() {
        let empty: AxisBinding<String> = AxisBinding::Pool(Vec::new());
        assert!(empty.primary().is_none());
        assert!(empty.candidates().is_empty());
    }

    // ---- CEG 02: WebhookEndpointDef::wants (A7) ----

    #[test]
    fn webhook_wants_matches_listed_types_and_empty_means_all() {
        fn ep(types: &[&str]) -> WebhookEndpointDef {
            WebhookEndpointDef {
                id: "e".into(),
                workspace_id: "ws".into(),
                url: "https://x/hook".into(),
                event_types: types.iter().map(|s| (*s).to_string()).collect(),
                disabled: false,
                secret_ref: SecretRef("whsec".into()),
            }
        }
        assert!(ep(&[]).wants("run.completed")); // empty = every type
        assert!(ep(&["run.completed", "run.failed"]).wants("run.failed"));
        assert!(!ep(&["run.completed"]).wants("run.failed"));
    }
}

/// The canonical Agent input configuration is the same typed language Sessions
/// consume. The legacy flat resource wire remains a read-only migration boundary.
#[cfg(test)]
mod resource_binding_serde_contract {
    use super::{
        AgentEnvironmentBinding, AgentInputConfig, BindingId, InputBinding, InputResourceId,
        MemoryStoreId, ResourceAccess,
    };

    fn cfg() -> AgentInputConfig {
        AgentInputConfig {
            agent_id: "a".into(),
            environment: Some(AgentEnvironmentBinding {
                environment_id: "env-production".into(),
                revision: 7,
            }),
            inputs: vec![InputBinding {
                binding_id: BindingId::from("memory"),
                target: InputResourceId::MemoryStore(MemoryStoreId::from("store-1")),
                mount_path: "/mnt/memory".into(),
                access: ResourceAccess::ReadWrite,
                instructions: Some("read it first".into()),
            }],
            revision: 1,
        }
    }

    #[test]
    fn round_trips_lossless() {
        let c = cfg();
        let json = serde_json::to_string(&c).expect("serializes");
        let back: AgentInputConfig = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, c, "round-trip is lossless");
        assert_eq!(back.environment.unwrap().revision, 7);
    }

    #[test]
    fn a_supported_legacy_binding_migrates_to_the_typed_language() {
        const LEGACY: &str = r#"{
          "agent_id": "a",
          "resources": [
            { "kind": "memory_store", "resource_id": "store-1",
              "mount_path": "/mnt/memory", "access": "read_write" }
          ],
          "version": 1
        }"#;
        let back: AgentInputConfig =
            serde_json::from_str(LEGACY).expect("a persisted legacy binding must still load");
        assert_eq!(back.revision, 1);
        assert_eq!(back.inputs[0].binding_id.as_str(), "agent:a:input:0");
        assert_eq!(back.inputs[0].target.id(), "store-1");
        assert_eq!(back.inputs[0].access, ResourceAccess::ReadWrite);
    }

    #[test]
    fn legacy_outputs_and_skills_fail_closed_instead_of_entering_the_input_union() {
        for kind in ["outputs", "skill"] {
            let json = format!(
                r#"{{"agent_id":"a","resources":[{{"kind":"{kind}","resource_id":"x","mount_path":"/x","access":"read_only"}}],"version":1}}"#
            );
            assert!(serde_json::from_str::<AgentInputConfig>(&json).is_err());
        }
    }

    #[test]
    fn an_unknown_future_field_is_ignored_not_rejected() {
        let json = r#"{"agent_id":"a","inputs":[],"revision":1,"a_future_field":42}"#;
        let back: AgentInputConfig =
            serde_json::from_str(json).expect("an unknown field is ignored");
        assert_eq!(back.agent_id, "a");
    }
}
