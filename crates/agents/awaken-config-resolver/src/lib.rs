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
use awaken_credential_vault::{CredentialBinding, CredentialError, CredentialSource, SecretStore};
use awaken_model_catalog::{ModelApiCompat, ProviderCatalog};

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
    #[error("credential pool `{0}` has no member that could be materialized (fail closed)")]
    PoolExhausted(String),
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

    // Credential materialization (secret only exists from here to the seam).
    let credential = resolve_credential(binding, sources, secret_store).await?;

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
    pub model_id: String,
    pub credential_binding: CredentialBinding,
    #[serde(default)]
    pub disabled_endpoint_ids: Vec<String>,
}

/// Resolve an [`InferenceProfile`] into a [`ResolvedInference`]: the same core
/// resolution, but selecting only endpoints the profile has not disabled and using
/// the profile's credential binding (which may be a pool with failover).
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

/// Materialize the credential a binding selects. `None` yields no secret; `Exact`
/// materializes one named source; `OneOfCredentialPool` walks the pool's selection
/// order and returns the first member that materializes — a disabled or unusable
/// member fails over to the next. Fail-closed: an empty/all-bad pool is an error,
/// never a silent unauthenticated run.
async fn resolve_credential(
    binding: &CredentialBinding,
    sources: &dyn SourceLookup,
    secret_store: &dyn SecretStore,
) -> Result<Option<RedactedString>, ResolveError> {
    match binding {
        CredentialBinding::None => Ok(None),
        CredentialBinding::Exact {
            credential_source_id,
        } => {
            let source = sources
                .get(credential_source_id.0.as_str())
                .ok_or_else(|| ResolveError::SourceMissing(credential_source_id.0.clone()))?;
            Ok(Some(
                awaken_credential_vault::materialize(source, secret_store).await?,
            ))
        }
        CredentialBinding::OneOfCredentialPool { credential_pool_id } => {
            let pool = sources
                .get_pool(credential_pool_id.0.as_str())
                .ok_or_else(|| ResolveError::PoolMissing(credential_pool_id.0.clone()))?;
            // Try members in selection order; skip a member whose source is absent
            // or fails to materialize, so one bad key does not fail the run.
            for member in pool.selection_order() {
                let Some(source) = sources.get(member.credential_source_id.0.as_str()) else {
                    continue;
                };
                if let Ok(secret) = awaken_credential_vault::materialize(source, secret_store).await
                {
                    return Ok(Some(secret));
                }
            }
            Err(ResolveError::PoolExhausted(credential_pool_id.0.clone()))
        }
    }
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
                catalog_fingerprint: CatalogFingerprint("fp".into()),
                instructions: String::new(),
                max_steps: 8,
                model_binding: ModelBinding {
                    provider_instance_ref: "anthropic".into(),
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
}
