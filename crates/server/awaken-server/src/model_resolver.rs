//! Catalog-backed publication of complete model candidates.
//!
//! The adapter reads one catalog snapshot and, when required, one Workspace
//! credential inventory. It resolves both authored model selection and provider
//! provisioning in that consistency window. Runtime code never calls this
//! adapter; it receives only the resulting immutable candidates.

use std::sync::Arc;

use awaken_config_resolver::can_consume;
use awaken_config_resolver::{
    InferenceProfile, InferenceProfileStore, ModelTarget, ProfileCandidate, get_workspace_profile,
};
use awaken_config_store::ModelSelection;
use awaken_credential_vault::repo::CredentialRepo;
use awaken_credential_vault::{
    CredentialBinding, CredentialKind, CredentialSource, CredentialStatus,
};
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

enum PublicationAccess<'a> {
    Direct(Option<&'a CredentialSource>),
    Brokered,
}

/// Configuration-plane adapter that freezes model, route and credential facts
/// into a publication. Every candidate must exist in the catalog; explicit
/// in-process scenario executors use their own composition resolver.
#[derive(Clone)]
pub struct CatalogModelPublicationResolver {
    source: CatalogSource,
    credentials: Arc<dyn CredentialRepo>,
    profiles: Option<Arc<dyn InferenceProfileStore>>,
}

/// The conventional Workspace-level profile consumed by `ModelSelection::Auto`.
pub const DEFAULT_INFERENCE_PROFILE_ID: &str = "workspace-default";

impl CatalogModelPublicationResolver {
    /// Resolve against a frozen catalog snapshot. This is useful for deterministic
    /// tests; production composition should use [`Self::from_repo`].
    #[must_use]
    pub fn new(catalog: ProviderCatalog, credentials: Arc<dyn CredentialRepo>) -> Self {
        Self {
            source: CatalogSource::Static(catalog),
            credentials,
            profiles: None,
        }
    }

    /// Resolve against the live catalog repository at publication time.
    #[must_use]
    pub fn from_repo(repo: Arc<dyn CatalogRepo>, credentials: Arc<dyn CredentialRepo>) -> Self {
        Self {
            source: CatalogSource::Live(repo),
            credentials,
            profiles: None,
        }
    }

    /// Install the authored Profile read port. An `Auto` Agent then consumes the
    /// Workspace's `workspace-default` profile when present; pinned Agents remain
    /// exact overrides and never consult it.
    #[must_use]
    pub fn with_profiles(mut self, profiles: Arc<dyn InferenceProfileStore>) -> Self {
        self.profiles = Some(profiles);
        self
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

    fn offering_for_target<'a>(
        catalog: &'a ProviderCatalog,
        target: &ModelTarget,
        disabled_endpoints: &[String],
    ) -> Result<&'a Offering, PublicationResolutionError> {
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
                    && !disabled_endpoints.contains(&offering.protocol_endpoint_id.0)
            })
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [offering] => Ok(*offering),
            [] => Err(PublicationResolutionError::CandidateUnavailable {
                binding: ModelBinding::new(
                    target.provider_id.as_deref().unwrap_or_default(),
                    &target.model_id,
                    "genai",
                ),
                reason: "the exact profile offering is not active or published".into(),
            }),
            _ => Err(PublicationResolutionError::CandidateUnavailable {
                binding: ModelBinding::new(
                    target.provider_id.as_deref().unwrap_or_default(),
                    &target.model_id,
                    "genai",
                ),
                reason: "the profile target is ambiguous; select provider and endpoint".into(),
            }),
        }
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
        workspace: &ScopeId,
        binding: ModelBinding,
        offering: &Offering,
        access: PublicationAccess<'_>,
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
        let credential = match access {
            PublicationAccess::Direct(credential) => credential
                .map(|credential| {
                    let revision = u64::try_from(credential.version).map_err(|_| {
                        unavailable(format!(
                            "credential {} has a negative version",
                            credential.id.0
                        ))
                    })?;
                    Ok(CredentialAccess::new(
                        CredentialRef {
                            id: credential.id.0.clone(),
                            revision,
                        },
                        match credential.kind {
                            CredentialKind::WorkerLocal => {
                                CredentialMaterialSource::WorkerReference
                            }
                            CredentialKind::Vault | CredentialKind::Oauth => {
                                CredentialMaterialSource::ControlPlaneReference
                            }
                            CredentialKind::Env => {
                                return Err(unavailable(
                                    "environment credentials cannot be frozen into a publication"
                                        .into(),
                                ));
                            }
                        },
                        CredentialUsage::ProviderAdapter,
                        CredentialExecutionPolicy::self_hosted_provider(),
                    ))
                })
                .transpose()?,
            PublicationAccess::Brokered => None,
        };
        let base_url = endpoint
            .base_url
            .clone()
            .ok_or_else(|| unavailable(format!("endpoint {} has no base URL", endpoint.id.0)))?;
        let route_ref = if offering.source == awaken_model_catalog::OfferingSource::Brokered {
            format!(
                "brokered:{}@{}",
                offering.protocol_endpoint_id.0, endpoint.version
            )
        } else {
            format!("{}@{}", offering.protocol_endpoint_id.0, endpoint.version)
        };
        Ok(ResolvedModelCandidate::provider(
            binding,
            format!("{}@{}", offering.provider_id.0, provider.version),
            route_ref,
            workspace.clone(),
            credential,
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
            let credential = Self::credential_for(sources, offering).ok_or_else(|| {
                PublicationResolutionError::CandidateUnavailable {
                    binding: binding.clone(),
                    reason: format!(
                        "no active persisted credential can consume model {} in Workspace {workspace}",
                        binding.model_ref
                    ),
                }
            })?;
            return Self::provider_candidate(
                catalog,
                workspace,
                binding,
                offering,
                PublicationAccess::Direct(Some(credential)),
            );
        }
        Err(PublicationResolutionError::CandidateUnavailable {
            reason: format!("model offering {} is not published", binding.model_ref),
            binding,
        })
    }

    fn source_for_profile_candidate<'a>(
        sources: &'a [CredentialSource],
        offering: &Offering,
        binding: &CredentialBinding,
    ) -> Result<PublicationAccess<'a>, String> {
        let eligible = |source: &&CredentialSource| {
            source.status == CredentialStatus::Active
                && source.kind != CredentialKind::Env
                && can_consume(offering.provider_id.as_str(), source)
        };
        match binding {
            CredentialBinding::None => {
                if offering.source == awaken_model_catalog::OfferingSource::Brokered {
                    return Err(
                        "brokered offering requires an explicit brokered access binding".into(),
                    );
                }
                Ok(PublicationAccess::Direct(None))
            }
            CredentialBinding::Brokered => {
                if offering.source != awaken_model_catalog::OfferingSource::Brokered {
                    return Err(
                        "brokered access binding requires a brokered catalog offering".into(),
                    );
                }
                Ok(PublicationAccess::Brokered)
            }
            CredentialBinding::Exact {
                credential_source_id,
            } => {
                if offering.source == awaken_model_catalog::OfferingSource::Brokered {
                    return Err("brokered offering cannot consume a local credential".into());
                }
                sources
                    .iter()
                    .find(|source| source.id == *credential_source_id)
                    .filter(eligible)
                    .map(|source| PublicationAccess::Direct(Some(source)))
                    .ok_or_else(|| {
                    format!(
                        "exact credential {} is absent, inactive, or incompatible with provider {}",
                        credential_source_id.0, offering.provider_id
                    )
                    })
            }
            CredentialBinding::OneOfCredentialPool { .. } => {
                if offering.source == awaken_model_catalog::OfferingSource::Brokered {
                    return Err("brokered offering cannot consume a local credential pool".into());
                }
                Err("credential pool must be resolved through its authored membership".into())
            }
        }
    }

    async fn profile_source<'a>(
        &self,
        sources: &'a [CredentialSource],
        workspace: &ScopeId,
        offering: &Offering,
        candidate: &ProfileCandidate,
    ) -> Result<PublicationAccess<'a>, PublicationResolutionError> {
        if let CredentialBinding::OneOfCredentialPool { credential_pool_id } =
            &candidate.credential_binding
        {
            let pool = self
                .credentials
                .get_pool(credential_pool_id)
                .await
                .map_err(|error| {
                    PublicationResolutionError::CredentialInventoryUnavailable(error.to_string())
                })?;
            if pool.workspace_id != workspace.as_str() {
                return Err(PublicationResolutionError::CandidateUnavailable {
                    binding: Self::binding_of(offering),
                    reason: "credential pool belongs to another Workspace".into(),
                });
            }
            return pool
                .selection_order()
                .into_iter()
                .filter_map(|member| {
                    sources
                        .iter()
                        .find(|source| source.id == member.credential_source_id)
                })
                .find(|source| {
                    source.status == CredentialStatus::Active
                        && source.kind != CredentialKind::Env
                        && can_consume(offering.provider_id.as_str(), source)
                })
                .map(|source| PublicationAccess::Direct(Some(source)))
                .ok_or_else(|| PublicationResolutionError::CandidateUnavailable {
                    binding: Self::binding_of(offering),
                    reason: format!(
                        "credential pool {} has no active compatible member",
                        credential_pool_id.0
                    ),
                });
        }
        Self::source_for_profile_candidate(sources, offering, &candidate.credential_binding)
            .map_err(|reason| PublicationResolutionError::CandidateUnavailable {
                binding: Self::binding_of(offering),
                reason,
            })
    }

    async fn resolve_profile_models(
        &self,
        catalog: &ProviderCatalog,
        workspace: &ScopeId,
        profile: &InferenceProfile,
    ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
        if profile.workspace_id != workspace.as_str() {
            return Err(PublicationResolutionError::Invalid(
                "default inference profile belongs to another Workspace".into(),
            ));
        }
        let authored = std::iter::once(&profile.primary)
            .chain(profile.fallbacks.iter())
            .collect::<Vec<_>>();
        let offerings = authored
            .iter()
            .map(|candidate| {
                Self::offering_for_target(
                    catalog,
                    &candidate.target,
                    &profile.disabled_endpoint_ids,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let needs_inventory = authored.iter().any(|candidate| {
            !matches!(
                candidate.credential_binding,
                CredentialBinding::None | CredentialBinding::Brokered
            )
        });
        let sources = if needs_inventory {
            self.credentials
                .list(workspace.as_str())
                .await
                .map_err(|error| {
                    PublicationResolutionError::CredentialInventoryUnavailable(error.to_string())
                })?
        } else {
            Vec::new()
        };
        let mut resolved = Vec::with_capacity(authored.len());
        for (candidate, offering) in authored.into_iter().zip(offerings) {
            let access = self
                .profile_source(&sources, workspace, offering, candidate)
                .await?;
            resolved.push(Self::provider_candidate(
                catalog,
                workspace,
                Self::binding_of(offering),
                offering,
                access,
            )?);
        }
        let primary = resolved.remove(0);
        let primary_model = primary.binding.model_ref.clone();
        Ok(ResolvedPublicationModels {
            primary,
            candidates: resolved,
            context_window: catalog.context_window(&primary_model),
            max_output_tokens: catalog.max_output_tokens(&primary_model),
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
        if selection.is_auto()
            && let Some(profiles) = &self.profiles
        {
            let profile = get_workspace_profile(
                profiles.as_ref(),
                workspace.as_str(),
                DEFAULT_INFERENCE_PROFILE_ID,
            )
            .map_err(|error| PublicationResolutionError::Invalid(error.to_string()))?;
            if let Some(profile) = profile {
                if !fallbacks.is_empty() {
                    return Err(PublicationResolutionError::Invalid(
                        "Auto profile selection cannot be combined with authored fallbacks".into(),
                    ));
                }
                return self
                    .resolve_profile_models(&catalog, workspace, &profile)
                    .await;
            }
        }
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
    //! Cause graph for Workspace default-profile publication:
    //! C1 selection is Auto; C2 `workspace-default` exists; C3 profile belongs to
    //! the execution Workspace; C4 every target identifies one active Offering;
    //! C5 each local credential binding resolves to an active compatible source;
    //! C6 `brokered` binding and Offering source agree.
    //! E1 use the authored ordered chain; E2 retain legacy catalog Auto; E3 freeze
    //! the exact per-step credential/route; E4 reject the whole publication.
    //!
    //! Decision table:
    //! | Rule | C1 | C2 | C3 | C4 | C5 | C6 | Effect |
    //! | T1   | Y  | Y  | Y  | Y  | Y  | -  | E1+E3 |
    //! | T2   | Y  | N  | -  | -  | -  | -  | E2    |
    //! | T3   | Y  | Y  | N  | -  | -  | -  | E2 (foreign row is absent) |
    //! | T4   | Y  | Y  | Y  | N  | -  | -  | E4    |
    //! | T5   | Y  | Y  | Y  | Y  | N  | -  | E4    |
    //! | T6   | N  | -  | -  | Y  | Y  | -  | pinned override |
    //! | T7   | Y  | Y  | Y  | Y  | -  | Y  | brokered pin, no local secret |
    //! | T8   | Y  | Y  | Y  | Y  | -  | N  | E4 |

    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_config_resolver::InMemoryProfileStore;
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
    async fn t1_auto_uses_default_profile_with_exact_order_route_and_credential() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = InMemorySecretStore::new();
        let primary_credential = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("openai".into()),
                env_key: Some("OPENAI_PRIMARY_KEY".into()),
                secret: Some(RedactedString::new("primary-secret")),
                oauth_command: None,
            },
            &secrets,
            credentials.as_ref(),
        )
        .await
        .unwrap();
        let fallback_credential = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("openai".into()),
                env_key: Some("OPENAI_FALLBACK_KEY".into()),
                secret: Some(RedactedString::new("fallback-secret")),
                oauth_command: None,
            },
            &secrets,
            credentials.as_ref(),
        )
        .await
        .unwrap();
        let mut catalog = catalog(&["primary"]);
        catalog.endpoints.insert(
            "ep2".into(),
            ProtocolEndpoint {
                id: ProtocolEndpointId::new("ep2"),
                provider_id: ProviderId::new("openai"),
                dialect: ApiDialect::OpenAiChat,
                base_url: Some("https://fallback.openai.invalid/v1".into()),
                timeout_secs: 30,
                display_name: "OpenAI fallback".into(),
                version: 9,
            },
        );
        catalog
            .offerings
            .push(offering("fallback", "openai", "ep2"));
        let profiles = Arc::new(InMemoryProfileStore::new());
        profiles
            .put(
                DEFAULT_INFERENCE_PROFILE_ID.into(),
                InferenceProfile {
                    workspace_id: "workspace-a".into(),
                    primary: ProfileCandidate {
                        target: ModelTarget {
                            model_id: "primary".into(),
                            provider_id: Some("openai".into()),
                            protocol_endpoint_id: Some("ep1".into()),
                        },
                        credential_binding: CredentialBinding::Exact {
                            credential_source_id: primary_credential.id.clone(),
                        },
                    },
                    fallbacks: vec![ProfileCandidate {
                        target: ModelTarget {
                            model_id: "fallback".into(),
                            provider_id: Some("openai".into()),
                            protocol_endpoint_id: Some("ep2".into()),
                        },
                        credential_binding: CredentialBinding::Exact {
                            credential_source_id: fallback_credential.id.clone(),
                        },
                    }],
                    disabled_endpoint_ids: Vec::new(),
                },
            )
            .unwrap();
        let resolver =
            CatalogModelPublicationResolver::new(catalog, credentials).with_profiles(profiles);

        let resolved = resolver
            .resolve_models(&ScopeId::from("workspace-a"), &ModelSelection::Auto, &[])
            .await
            .unwrap();
        let pins = std::iter::once(&resolved.primary)
            .chain(resolved.candidates.iter())
            .map(|candidate| match &candidate.provisioning {
                ModelProvisioning::Provider {
                    route_ref,
                    credential: Some(credential),
                    ..
                } => (
                    candidate.binding.model_ref.as_str(),
                    route_ref.as_str(),
                    credential.credential.id.as_str(),
                ),
                _ => panic!("profile candidate must freeze route and credential"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            pins,
            vec![
                ("primary", "ep1@4", primary_credential.id.0.as_str()),
                ("fallback", "ep2@9", fallback_credential.id.0.as_str()),
            ]
        );
    }

    #[tokio::test]
    async fn t3_default_profile_from_another_workspace_is_not_observable() {
        let resolver = resolver(&["primary"]).await;
        let profiles = Arc::new(InMemoryProfileStore::new());
        profiles
            .put(
                DEFAULT_INFERENCE_PROFILE_ID.into(),
                InferenceProfile {
                    workspace_id: "workspace-b".into(),
                    primary: ProfileCandidate {
                        target: ModelTarget {
                            model_id: "primary".into(),
                            provider_id: Some("openai".into()),
                            protocol_endpoint_id: Some("ep1".into()),
                        },
                        credential_binding: CredentialBinding::None,
                    },
                    fallbacks: Vec::new(),
                    disabled_endpoint_ids: Vec::new(),
                },
            )
            .unwrap();
        let resolver = resolver.with_profiles(profiles);

        let resolved = resolver
            .resolve_models(&ScopeId::from("workspace-a"), &ModelSelection::Auto, &[])
            .await
            .unwrap();
        assert_eq!(resolved.primary.binding.model_ref, "primary");
        let ModelProvisioning::Provider {
            credential: Some(_),
            ..
        } = resolved.primary.provisioning
        else {
            panic!("foreign profile must be ignored in favor of local catalog Auto")
        };
    }

    #[tokio::test]
    async fn t4_disabled_profile_target_rejects_the_publication() {
        let resolver = resolver(&["primary"]).await;
        let profiles = Arc::new(InMemoryProfileStore::new());
        profiles
            .put(
                DEFAULT_INFERENCE_PROFILE_ID.into(),
                InferenceProfile {
                    workspace_id: "workspace-a".into(),
                    primary: ProfileCandidate {
                        target: ModelTarget {
                            model_id: "primary".into(),
                            provider_id: Some("openai".into()),
                            protocol_endpoint_id: Some("ep1".into()),
                        },
                        credential_binding: CredentialBinding::None,
                    },
                    fallbacks: Vec::new(),
                    disabled_endpoint_ids: vec!["ep1".into()],
                },
            )
            .unwrap();

        let error = resolver
            .with_profiles(profiles)
            .resolve_models(&ScopeId::from("workspace-a"), &ModelSelection::Auto, &[])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not active or published"));
    }

    #[tokio::test]
    async fn t5_incompatible_exact_profile_credential_rejects_the_publication() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let incompatible = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: Some("ANTHROPIC_API_KEY".into()),
                secret: Some(RedactedString::new("wrong-provider")),
                oauth_command: None,
            },
            &InMemorySecretStore::new(),
            credentials.as_ref(),
        )
        .await
        .unwrap();
        let profiles = Arc::new(InMemoryProfileStore::new());
        profiles
            .put(
                DEFAULT_INFERENCE_PROFILE_ID.into(),
                InferenceProfile {
                    workspace_id: "workspace-a".into(),
                    primary: ProfileCandidate {
                        target: ModelTarget {
                            model_id: "primary".into(),
                            provider_id: Some("openai".into()),
                            protocol_endpoint_id: Some("ep1".into()),
                        },
                        credential_binding: CredentialBinding::Exact {
                            credential_source_id: incompatible.id,
                        },
                    },
                    fallbacks: Vec::new(),
                    disabled_endpoint_ids: Vec::new(),
                },
            )
            .unwrap();

        let error = CatalogModelPublicationResolver::new(catalog(&["primary"]), credentials)
            .with_profiles(profiles)
            .resolve_models(&ScopeId::from("workspace-a"), &ModelSelection::Auto, &[])
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("incompatible with provider openai")
        );
    }

    #[tokio::test]
    async fn t6_pinned_selection_ignores_the_workspace_default_profile() {
        let resolver = resolver(&["primary"]).await;
        let profiles = Arc::new(InMemoryProfileStore::new());
        profiles
            .put(
                DEFAULT_INFERENCE_PROFILE_ID.into(),
                InferenceProfile {
                    workspace_id: "workspace-a".into(),
                    primary: ProfileCandidate {
                        target: ModelTarget::unqualified("not-published"),
                        credential_binding: CredentialBinding::None,
                    },
                    fallbacks: Vec::new(),
                    disabled_endpoint_ids: Vec::new(),
                },
            )
            .unwrap();

        let resolved = resolver
            .with_profiles(profiles)
            .resolve_models(
                &ScopeId::from("workspace-a"),
                &ModelSelection::Pinned(ModelBinding::new("openai", "primary", "genai")),
                &[],
            )
            .await
            .unwrap();
        assert_eq!(resolved.primary.binding.model_ref, "primary");
    }

    #[tokio::test]
    async fn t7_brokered_profile_publishes_a_marker_without_local_credential() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let mut catalog = catalog(&["managed-model"]);
        catalog.offerings[0].source = awaken_model_catalog::OfferingSource::Brokered;
        catalog.endpoints.get_mut("ep1").unwrap().base_url =
            Some("https://cloud-control.invalid".into());
        let profiles = Arc::new(InMemoryProfileStore::new());
        profiles
            .put(
                DEFAULT_INFERENCE_PROFILE_ID.into(),
                InferenceProfile {
                    workspace_id: "workspace-a".into(),
                    primary: ProfileCandidate {
                        target: ModelTarget {
                            model_id: "managed-model".into(),
                            provider_id: Some("openai".into()),
                            protocol_endpoint_id: Some("ep1".into()),
                        },
                        credential_binding: CredentialBinding::Brokered,
                    },
                    fallbacks: Vec::new(),
                    disabled_endpoint_ids: Vec::new(),
                },
            )
            .unwrap();

        let resolved = CatalogModelPublicationResolver::new(catalog, credentials)
            .with_profiles(profiles)
            .resolve_models(&ScopeId::from("workspace-a"), &ModelSelection::Auto, &[])
            .await
            .unwrap();
        let ModelProvisioning::Provider {
            route_ref,
            credential,
            ..
        } = resolved.primary.provisioning
        else {
            panic!("brokered model remains a native Provider-protocol candidate")
        };
        assert_eq!(route_ref, "brokered:ep1@4");
        assert!(credential.is_none());
    }

    #[tokio::test]
    async fn t8_brokered_binding_cannot_relabel_a_direct_offering() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let profiles = Arc::new(InMemoryProfileStore::new());
        profiles
            .put(
                DEFAULT_INFERENCE_PROFILE_ID.into(),
                InferenceProfile {
                    workspace_id: "workspace-a".into(),
                    primary: ProfileCandidate {
                        target: ModelTarget {
                            model_id: "primary".into(),
                            provider_id: Some("openai".into()),
                            protocol_endpoint_id: Some("ep1".into()),
                        },
                        credential_binding: CredentialBinding::Brokered,
                    },
                    fallbacks: Vec::new(),
                    disabled_endpoint_ids: Vec::new(),
                },
            )
            .unwrap();

        let error = CatalogModelPublicationResolver::new(catalog(&["primary"]), credentials)
            .with_profiles(profiles)
            .resolve_models(&ScopeId::from("workspace-a"), &ModelSelection::Auto, &[])
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("requires a brokered catalog offering")
        );
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
