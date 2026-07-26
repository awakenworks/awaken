//! Catalog-backed publication of complete model candidates.
//!
//! The adapter reads one catalog snapshot and, when required, one Workspace
//! credential inventory. It resolves both authored model selection and provider
//! provisioning in that consistency window. Runtime code never calls this
//! adapter; it receives only the resulting immutable candidates.

use std::sync::Arc;

use awaken_config_resolver::can_consume;
use awaken_config_store::ModelSelection;
use awaken_credential_vault::repo::CredentialRepo;
use awaken_credential_vault::{CredentialKind, CredentialSource, CredentialStatus};
use awaken_model_catalog::repo::CatalogRepo;
use awaken_model_catalog::{Offering, ProviderCatalog};
use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate};
use awaken_runtime_contract::{
    CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
    CredentialUsage, InferenceEndpoint,
};
use awaken_runtime_host::{
    ModelPublicationResolver, PublicationResolutionError, ResolvedPublicationModels,
};
use awaken_tenancy::ScopeId;

#[derive(Clone)]
enum CatalogSource {
    Static(ProviderCatalog),
    Live(Arc<dyn CatalogRepo>),
}

/// Configuration-plane adapter that freezes model, route and credential facts
/// into a publication. Every candidate must exist in the catalog; explicit
/// in-process scenario executors use their own composition resolver.
#[derive(Clone)]
pub struct CatalogModelPublicationResolver {
    source: CatalogSource,
    credentials: Arc<dyn CredentialRepo>,
}

impl CatalogModelPublicationResolver {
    /// Resolve against a frozen catalog snapshot. This is useful for deterministic
    /// tests; production composition should use [`Self::from_repo`].
    #[must_use]
    pub fn new(catalog: ProviderCatalog, credentials: Arc<dyn CredentialRepo>) -> Self {
        Self {
            source: CatalogSource::Static(catalog),
            credentials,
        }
    }

    /// Resolve against the live catalog repository at publication time.
    #[must_use]
    pub fn from_repo(repo: Arc<dyn CatalogRepo>, credentials: Arc<dyn CredentialRepo>) -> Self {
        Self {
            source: CatalogSource::Live(repo),
            credentials,
        }
    }

    async fn snapshot(&self) -> Result<ProviderCatalog, PublicationResolutionError> {
        match &self.source {
            CatalogSource::Static(catalog) => Ok(catalog.clone()),
            CatalogSource::Live(repo) => repo
                .snapshot()
                .await
                .map_err(|error| PublicationResolutionError::CatalogUnavailable(error.to_string())),
        }
    }

    fn binding_of(offering: &Offering) -> ModelBinding {
        ModelBinding::new(&offering.provider_id.0, &offering.model_id, "genai")
    }

    fn selected_bindings(
        catalog: &ProviderCatalog,
        selection: &ModelSelection,
        fallbacks: &[ModelBinding],
    ) -> Result<(ModelBinding, Vec<ModelBinding>), PublicationResolutionError> {
        if let Some(primary) = selection.resolved() {
            return Ok((
                Self::canonical_binding(catalog, primary)?,
                fallbacks
                    .iter()
                    .map(|binding| Self::canonical_binding(catalog, binding))
                    .collect::<Result<Vec<_>, _>>()?,
            ));
        }
        let mut offerings = catalog
            .offerings
            .iter()
            .filter(|offering| offering.status == awaken_model_catalog::OfferingStatus::Active);
        let primary = offerings
            .next()
            .ok_or(PublicationResolutionError::MissingPrimary)?;
        Ok((
            Self::binding_of(primary),
            offerings.map(Self::binding_of).collect(),
        ))
    }

    /// Normalize the public model-level selection into the one complete catalog
    /// identity frozen in the publication. Provider/backend-qualified bindings
    /// remain exact; an SDK/UI `{model}` selection is accepted only when the
    /// active catalog has one matching offering.
    fn canonical_binding(
        catalog: &ProviderCatalog,
        binding: &ModelBinding,
    ) -> Result<ModelBinding, PublicationResolutionError> {
        if Self::offering_for(catalog, binding).is_some() {
            return Ok(binding.clone());
        }
        let candidates = catalog
            .offerings
            .iter()
            .filter(|offering| {
                offering.status == awaken_model_catalog::OfferingStatus::Active
                    && offering.model_id == binding.model_ref
                    && (binding.provider_identity_ref.is_empty()
                        || offering.provider_id.as_str() == binding.provider_identity_ref)
                    && (binding.backend_ref.is_empty() || binding.backend_ref == "genai")
            })
            .map(Self::binding_of)
            .collect::<Vec<_>>();
        match candidates.as_slice() {
            [resolved] => Ok(resolved.clone()),
            [] => Err(PublicationResolutionError::CandidateUnavailable {
                binding: binding.clone(),
                reason: format!("model offering {} is not published", binding.model_ref),
            }),
            _ => Err(PublicationResolutionError::CandidateUnavailable {
                binding: binding.clone(),
                reason: format!(
                    "model {} is ambiguous; select a provider-qualified binding",
                    binding.model_ref
                ),
            }),
        }
    }

    fn offering_for<'a>(
        catalog: &'a ProviderCatalog,
        binding: &ModelBinding,
    ) -> Option<&'a Offering> {
        catalog.offerings.iter().find(|offering| {
            offering.status == awaken_model_catalog::OfferingStatus::Active
                && offering.model_id == binding.model_ref
                && offering.provider_id.as_str() == binding.provider_identity_ref
                && binding.backend_ref == "genai"
        })
    }

    fn credential_for<'a>(
        sources: &'a [CredentialSource],
        offering: &Offering,
    ) -> Option<&'a CredentialSource> {
        sources
            .iter()
            .filter(|source| {
                source.status == CredentialStatus::Active
                    && source.kind != CredentialKind::Env
                    && can_consume(offering.provider_id.as_str(), source)
            })
            .min_by(|left, right| left.id.0.cmp(&right.id.0))
    }

    fn provider_candidate(
        catalog: &ProviderCatalog,
        sources: &[CredentialSource],
        workspace: &ScopeId,
        binding: ModelBinding,
        offering: &Offering,
    ) -> Result<ResolvedModelCandidate, PublicationResolutionError> {
        let unavailable = |reason| PublicationResolutionError::CandidateUnavailable {
            binding: binding.clone(),
            reason,
        };
        let provider = catalog
            .providers
            .get(offering.provider_id.as_str())
            .ok_or_else(|| unavailable(format!("provider {} is missing", offering.provider_id)))?;
        let endpoint = catalog
            .endpoints
            .get(offering.protocol_endpoint_id.as_str())
            .ok_or_else(|| {
                unavailable(format!(
                    "endpoint {} is missing",
                    offering.protocol_endpoint_id
                ))
            })?;
        let credential = Self::credential_for(sources, offering).ok_or_else(|| {
            unavailable(format!(
                "no active persisted credential can consume model {} in Workspace {workspace}",
                binding.model_ref
            ))
        })?;
        let revision = u64::try_from(credential.version).map_err(|_| {
            unavailable(format!(
                "credential {} has a negative version",
                credential.id.0
            ))
        })?;
        let base_url = endpoint
            .base_url
            .clone()
            .ok_or_else(|| unavailable(format!("endpoint {} has no base URL", endpoint.id.0)))?;
        Ok(ResolvedModelCandidate::provider(
            binding,
            format!("{}@{}", offering.provider_id.0, provider.version),
            format!("{}@{}", offering.protocol_endpoint_id.0, endpoint.version),
            workspace.clone(),
            Some(CredentialAccess::new(
                CredentialRef {
                    id: credential.id.0.clone(),
                    revision,
                },
                match credential.kind {
                    CredentialKind::WorkerLocal => CredentialMaterialSource::WorkerReference,
                    CredentialKind::Vault | CredentialKind::Oauth => {
                        CredentialMaterialSource::ControlPlaneReference
                    }
                    CredentialKind::Env => unreachable!("environment sources are filtered out"),
                },
                CredentialUsage::ProviderAdapter,
                CredentialExecutionPolicy::self_hosted_provider(),
            )),
            InferenceEndpoint {
                adapter_kind: endpoint.dialect.adapter_kind().to_string(),
                api_dialect: endpoint.dialect.as_str().to_string(),
                base_url,
                upstream_model: offering
                    .upstream_model
                    .clone()
                    .unwrap_or_else(|| offering.model_id.clone()),
            },
        ))
    }

    fn candidate(
        &self,
        catalog: &ProviderCatalog,
        sources: &[CredentialSource],
        workspace: &ScopeId,
        binding: ModelBinding,
    ) -> Result<ResolvedModelCandidate, PublicationResolutionError> {
        if let Some(offering) = Self::offering_for(catalog, &binding) {
            return Self::provider_candidate(catalog, sources, workspace, binding, offering);
        }
        Err(PublicationResolutionError::CandidateUnavailable {
            reason: format!("model offering {} is not published", binding.model_ref),
            binding,
        })
    }
}

#[async_trait::async_trait]
impl ModelPublicationResolver for CatalogModelPublicationResolver {
    async fn resolve_models(
        &self,
        workspace: &ScopeId,
        selection: &ModelSelection,
        fallbacks: &[ModelBinding],
    ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
        let catalog = self.snapshot().await?;
        let (primary_binding, fallback_bindings) =
            Self::selected_bindings(&catalog, selection, fallbacks)?;
        let all_bindings = std::iter::once(&primary_binding)
            .chain(fallback_bindings.iter())
            .collect::<Vec<_>>();
        let needs_credentials = all_bindings
            .iter()
            .any(|binding| Self::offering_for(&catalog, binding).is_some());
        let sources = if needs_credentials {
            self.credentials
                .list(workspace.as_str())
                .await
                .map_err(|error| {
                    PublicationResolutionError::CredentialInventoryUnavailable(error.to_string())
                })?
        } else {
            Vec::new()
        };
        let primary = self.candidate(&catalog, &sources, workspace, primary_binding.clone())?;
        let candidates = fallback_bindings
            .into_iter()
            .map(|binding| self.candidate(&catalog, &sources, workspace, binding))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ResolvedPublicationModels {
            primary,
            candidates,
            context_window: catalog.context_window(&primary_binding.model_ref),
            max_output_tokens: catalog.max_output_tokens(&primary_binding.model_ref),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_credential_vault::repo::{InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{CredentialCreateParams, InMemorySecretStore};
    use awaken_model_catalog::{
        ApiDialect, ModelAttributes, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
    };
    use awaken_runtime_contract::resolved::ModelProvisioning;

    fn offering(model: &str, provider: &str, endpoint: &str) -> Offering {
        Offering {
            model_id: model.to_string(),
            provider_id: ProviderId::new(provider),
            protocol_endpoint_id: ProtocolEndpointId::new(endpoint),
            dialect: ApiDialect::OpenAiChat,
            upstream_model: None,
            source: Default::default(),
            status: Default::default(),
            last_seen_at_unix_ms: None,
        }
    }

    fn catalog(models: &[&str]) -> ProviderCatalog {
        let mut catalog = ProviderCatalog::default();
        catalog.providers.insert(
            "openai".into(),
            Provider {
                id: ProviderId::new("openai"),
                slug: "openai".into(),
                display_name: "OpenAI".into(),
                version: 2,
            },
        );
        catalog.endpoints.insert(
            "ep1".into(),
            ProtocolEndpoint {
                id: ProtocolEndpointId::new("ep1"),
                provider_id: ProviderId::new("openai"),
                dialect: ApiDialect::OpenAiChat,
                base_url: Some("https://api.openai.invalid/v1".into()),
                timeout_secs: 30,
                display_name: "OpenAI".into(),
                version: 4,
            },
        );
        catalog.offerings = models
            .iter()
            .map(|model| offering(model, "openai", "ep1"))
            .collect();
        catalog
    }

    async fn resolver(models: &[&str]) -> CatalogModelPublicationResolver {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("openai".into()),
                env_key: Some("OPENAI_API_KEY".into()),
                secret: Some(RedactedString::new("test-secret")),
                oauth_command: None,
            },
            &InMemorySecretStore::new(),
            credentials.as_ref(),
        )
        .await
        .unwrap();
        CatalogModelPublicationResolver::new(catalog(models), credentials)
    }

    #[tokio::test]
    async fn auto_publication_returns_complete_ordered_candidates() {
        let resolver = resolver(&["m-first", "m-second", "m-third"]).await;
        let resolved = resolver
            .resolve_models(&ScopeId::from("workspace-a"), &ModelSelection::Auto, &[])
            .await
            .unwrap();
        assert_eq!(
            resolved.primary.binding,
            ModelBinding::new("openai", "m-first", "genai")
        );
        assert_eq!(
            resolved
                .candidates
                .iter()
                .map(|candidate| candidate.binding.model_ref.as_str())
                .collect::<Vec<_>>(),
            vec!["m-second", "m-third"]
        );
        assert!(matches!(
            resolved.primary.provisioning,
            ModelProvisioning::Provider { .. }
        ));
    }

    #[tokio::test]
    async fn model_level_selection_is_normalized_to_one_complete_catalog_binding() {
        let resolver = resolver(&["m-first"]).await;
        let resolved = resolver
            .resolve_models(
                &ScopeId::from("workspace-a"),
                &ModelSelection::Pinned(ModelBinding::new("", "m-first", "")),
                &[],
            )
            .await
            .unwrap();

        assert_eq!(
            resolved.primary.binding,
            ModelBinding::new("openai", "m-first", "genai")
        );
    }

    #[tokio::test]
    async fn pinned_publication_preserves_validated_authored_identity_and_order() {
        let resolver = resolver(&["primary", "fallback"]).await;
        let primary = ModelBinding::new("openai", "primary", "genai");
        let fallback = ModelBinding::new("openai", "fallback", "genai");
        let resolved = resolver
            .resolve_models(
                &ScopeId::from("workspace-a"),
                &ModelSelection::Pinned(primary.clone()),
                std::slice::from_ref(&fallback),
            )
            .await
            .unwrap();
        assert_eq!(resolved.primary.binding, primary);
        assert_eq!(resolved.candidates[0].binding, fallback);
    }

    #[tokio::test]
    async fn same_model_on_another_provider_is_not_treated_as_the_published_binding() {
        let resolver = resolver(&["primary"]).await;
        let binding = ModelBinding::new("other-provider", "primary", "genai");
        assert!(matches!(
            resolver
                .resolve_models(
                    &ScopeId::from("workspace-a"),
                    &ModelSelection::Pinned(binding.clone()),
                    &[],
                )
                .await,
            Err(PublicationResolutionError::CandidateUnavailable {
                binding: rejected,
                ..
            }) if rejected == binding
        ));
    }

    #[tokio::test]
    async fn one_unresolvable_fallback_rejects_the_entire_publication() {
        let resolver = resolver(&["primary"]).await;
        let error = resolver
            .resolve_models(
                &ScopeId::from("workspace-a"),
                &ModelSelection::Pinned(ModelBinding::new("openai", "primary", "genai")),
                &[ModelBinding::new("openai", "missing", "genai")],
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("missing"));
    }

    #[tokio::test]
    async fn primary_catalog_attributes_share_the_resolution_snapshot() {
        let mut catalog = catalog(&["primary"]);
        catalog.model_attributes.insert(
            "primary".into(),
            ModelAttributes {
                context_window: Some(200_000),
                max_output_tokens: Some(40_000),
                provenance: Default::default(),
            },
        );
        let credentials = resolver(&["unused"]).await.credentials;
        let resolver = CatalogModelPublicationResolver::new(catalog, credentials);
        let resolved = resolver
            .resolve_models(&ScopeId::from("workspace-a"), &ModelSelection::Auto, &[])
            .await
            .unwrap();
        assert_eq!(resolved.context_window, Some(200_000));
        assert_eq!(resolved.max_output_tokens, Some(40_000));
    }

    #[tokio::test]
    async fn empty_catalog_and_cross_workspace_credentials_fail_closed() {
        let empty_resolver = resolver(&[]).await;
        assert!(matches!(
            empty_resolver
                .resolve_models(&ScopeId::from("workspace-a"), &ModelSelection::Auto, &[],)
                .await,
            Err(PublicationResolutionError::MissingPrimary)
        ));

        let resolver = resolver(&["primary"]).await;
        assert!(
            resolver
                .resolve_models(&ScopeId::from("workspace-b"), &ModelSelection::Auto, &[],)
                .await
                .unwrap_err()
                .to_string()
                .contains("Workspace workspace-b")
        );
    }

    #[tokio::test]
    async fn worker_local_source_publishes_an_exact_worker_reference() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::WorkerLocal,
                provider_id: Some("openai".into()),
                env_key: None,
                secret: None,
                oauth_command: None,
            },
            &InMemorySecretStore::new(),
            credentials.as_ref(),
        )
        .await
        .unwrap();
        let resolver = CatalogModelPublicationResolver::new(catalog(&["primary"]), credentials);
        let resolved = resolver
            .resolve_models(&ScopeId::from("workspace-a"), &ModelSelection::Auto, &[])
            .await
            .unwrap();
        let ModelProvisioning::Provider {
            credential: Some(access),
            ..
        } = resolved.primary.provisioning
        else {
            panic!("provider publication carries its credential")
        };
        assert_eq!(access.credential.id, source.id.0);
        assert_eq!(access.credential.revision, 1);
        assert_eq!(
            access.material_source,
            CredentialMaterialSource::WorkerReference
        );
    }
}
