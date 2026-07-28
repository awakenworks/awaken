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
use awaken_runtime_contract::resolved::{
    Backend, BackendModelSelection, ModelBinding, ResolvedModelCandidate,
};
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
    brokered_access_enabled: bool,
}

impl CatalogModelPublicationResolver {
    /// Resolve against a frozen catalog snapshot. This is useful for deterministic
    /// tests; production composition should use [`Self::from_repo`].
    #[must_use]
    pub fn new(catalog: ProviderCatalog, credentials: Arc<dyn CredentialRepo>) -> Self {
        Self {
            source: CatalogSource::Static(catalog),
            credentials,
            profiles: None,
            brokered_access_enabled: true,
        }
    }

    /// Resolve against the live catalog repository at publication time.
    #[must_use]
    pub fn from_repo(repo: Arc<dyn CatalogRepo>, credentials: Arc<dyn CredentialRepo>) -> Self {
        Self {
            source: CatalogSource::Live(repo),
            credentials,
            profiles: None,
            brokered_access_enabled: true,
        }
    }

    /// Install the authored Profile read port for explicit
    /// [`ModelSelection::Profile`] choices.
    #[must_use]
    pub fn with_profiles(mut self, profiles: Arc<dyn InferenceProfileStore>) -> Self {
        self.profiles = Some(profiles);
        self
    }

    /// Select whether brokered Offering/Profile pairs may enter a new immutable
    /// publication. Disabling this never relabels them as direct/BYOK candidates.
    #[must_use]
    pub fn with_brokered_access(mut self, enabled: bool) -> Self {
        self.brokered_access_enabled = enabled;
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
        if let Some(backend_ref) = selection.backend_default_ref() {
            let primary = ModelBinding::new("", "", backend_ref);
            Self::validate_acp_binding(&primary, BackendModelSelection::Default)?;
            return Ok((
                primary,
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
        if matches!(Backend::from_ref(&binding.backend_ref), Backend::Acp { .. }) {
            Self::validate_acp_binding(binding, BackendModelSelection::Exact)?;
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

    fn validate_acp_binding(
        binding: &ModelBinding,
        selection: BackendModelSelection,
    ) -> Result<&'static awaken_run_executor_acp::AcpCli, PublicationResolutionError> {
        let Backend::Acp { cli } = Backend::from_ref(&binding.backend_ref) else {
            return Err(PublicationResolutionError::CandidateUnavailable {
                binding: binding.clone(),
                reason: "backend-owned model selection requires an ACP backend".into(),
            });
        };
        if cli.trim().is_empty() || binding.backend_ref != format!("acp:{cli}") {
            return Err(PublicationResolutionError::CandidateUnavailable {
                binding: binding.clone(),
                reason: "backend-owned model selection requires an exact acp:<cli> backend".into(),
            });
        }
        let profile = awaken_run_executor_acp::acp_cli(&cli).ok_or_else(|| {
            PublicationResolutionError::CandidateUnavailable {
                binding: binding.clone(),
                reason: format!("ACP backend {cli} is not in the executable catalog"),
            }
        })?;
        let coherent = match selection {
            BackendModelSelection::Default => binding.model_ref.is_empty(),
            BackendModelSelection::Exact => !binding.model_ref.trim().is_empty(),
        };
        if !coherent {
            return Err(PublicationResolutionError::CandidateUnavailable {
                binding: binding.clone(),
                reason: format!("incoherent {selection:?} backend model selection"),
            });
        }
        if selection == BackendModelSelection::Exact
            && profile.backend_model_interface
                == awaken_run_executor_acp::BackendModelInterface::Unsupported
        {
            return Err(PublicationResolutionError::CandidateUnavailable {
                binding: binding.clone(),
                reason: format!("ACP backend {cli} cannot guarantee an exact model selection"),
            });
        }
        Ok(profile)
    }

    fn offering_for<'a>(
        catalog: &'a ProviderCatalog,
        binding: &ModelBinding,
    ) -> Option<&'a Offering> {
        catalog.offerings.iter().find(|offering| {
            offering.status == awaken_model_catalog::OfferingStatus::Active
                && offering.model_id == binding.model_ref
                && offering.provider_id.as_str() == binding.provider_identity_ref
                && (binding.backend_ref == "genai"
                    || matches!(Backend::from_ref(&binding.backend_ref), Backend::Acp { .. }))
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
        binding: &ModelBinding,
    ) -> Option<&'a CredentialSource> {
        sources
            .iter()
            .filter(|source| {
                source.status == CredentialStatus::Active
                    && source.kind != CredentialKind::Env
                    && Self::credential_can_supply(offering, binding, source)
            })
            .min_by_key(|source| {
                (
                    source.env_key.as_deref()
                        != Some(awaken_credential_vault::CLAUDE_CODE_SETUP_TOKEN_ENV),
                    source.id.0.as_str(),
                )
            })
    }

    fn credential_can_supply(
        offering: &Offering,
        binding: &ModelBinding,
        source: &CredentialSource,
    ) -> bool {
        if source.env_key.as_deref() == Some(awaken_credential_vault::CLAUDE_CODE_SETUP_TOKEN_ENV) {
            return offering.provider_id.as_str() == "anthropic"
                && binding.backend_ref == "acp:claude"
                && source.provider_id.as_deref() == Some("anthropic");
        }
        can_consume(offering.provider_id.as_str(), source)
            && Self::credential_usage(binding, source).is_ok()
    }

    fn credential_usage(
        binding: &ModelBinding,
        source: &CredentialSource,
    ) -> Result<CredentialUsage, String> {
        let Backend::Acp { cli } = Backend::from_ref(&binding.backend_ref) else {
            if source.env_key.as_deref()
                == Some(awaken_credential_vault::CLAUDE_CODE_SETUP_TOKEN_ENV)
            {
                return Err("Claude Code setup tokens require backend acp:claude".into());
            }
            return Ok(CredentialUsage::ProviderAdapter);
        };
        let profile = awaken_run_executor_acp::acp_cli(&cli)
            .ok_or_else(|| format!("ACP backend {cli} is not in the executable catalog"))?;
        let Some(delivery) = profile.model_delivery else {
            return Ok(CredentialUsage::ProviderAdapter);
        };
        let Some(name) = source.env_key.as_deref() else {
            return Ok(CredentialUsage::ProviderAdapter);
        };
        if !delivery.supports_credential_env(name) {
            return Err(format!(
                "ACP backend {cli} does not accept credential environment {name}"
            ));
        }
        Ok(CredentialUsage::EnvironmentVariable {
            name: name.to_string(),
        })
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
                    let usage =
                        Self::credential_usage(&binding, credential).map_err(unavailable)?;
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
                        usage,
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
            let credential = Self::credential_for(sources, offering, &binding).ok_or_else(|| {
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
        if matches!(Backend::from_ref(&binding.backend_ref), Backend::Acp { .. }) {
            return Self::backend_candidate(binding, sources, BackendModelSelection::Exact);
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
        if offering.source == awaken_model_catalog::OfferingSource::Brokered
            && !self.brokered_access_enabled
        {
            return Err(PublicationResolutionError::CandidateUnavailable {
                binding: Self::binding_of(offering),
                reason: "cloud_models_disabled: brokered model supply is disabled".into(),
            });
        }
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

    fn backend_candidate(
        binding: ModelBinding,
        sources: &[CredentialSource],
        model_selection: BackendModelSelection,
    ) -> Result<ResolvedModelCandidate, PublicationResolutionError> {
        Self::validate_acp_binding(&binding, model_selection)?;
        let mut matches = sources.iter().filter(|source| {
            source.status == CredentialStatus::Active
                && source.kind == CredentialKind::WorkerLocal
                && source.worker_local_binding.as_ref().is_some_and(|local| {
                    local.driver_id == binding.backend_ref
                        && (binding.provider_identity_ref.is_empty()
                            || binding.provider_identity_ref == source.id.0)
                })
        });
        let source =
            matches
                .next()
                .ok_or_else(|| PublicationResolutionError::CandidateUnavailable {
                    binding: binding.clone(),
                    reason: format!(
                        "no active Worker-local binding is registered for {}",
                        binding.backend_ref
                    ),
                })?;
        if matches.next().is_some() {
            return Err(PublicationResolutionError::CandidateUnavailable {
                binding: binding.clone(),
                reason: format!(
                    "multiple Worker-local bindings are registered for {}; select an exact identity",
                    binding.backend_ref
                ),
            });
        }
        let revision = u64::try_from(source.version)
            .ok()
            .filter(|revision| *revision > 0)
            .ok_or_else(|| PublicationResolutionError::CandidateUnavailable {
                binding: binding.clone(),
                reason: format!(
                    "Worker-local source {} has an invalid revision",
                    source.id.0
                ),
            })?;
        let mut resolved_binding = binding;
        resolved_binding
            .provider_identity_ref
            .clone_from(&source.id.0);
        Ok(ResolvedModelCandidate::backend_owned(
            resolved_binding,
            CredentialRef {
                id: source.id.0.clone(),
                revision,
            },
            model_selection,
        ))
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
        if let Some(profile_id) = selection.profile_ref() {
            let profiles = self.profiles.as_ref().ok_or_else(|| {
                PublicationResolutionError::Invalid(
                    "profile model selection is unavailable in this composition".into(),
                )
            })?;
            let profile = get_workspace_profile(profiles.as_ref(), workspace.as_str(), profile_id)
                .map_err(|error| PublicationResolutionError::Invalid(error.to_string()))?;
            let profile = profile.ok_or_else(|| {
                PublicationResolutionError::Invalid(format!(
                    "inference profile `{profile_id}` does not exist in Workspace `{workspace}`"
                ))
            })?;
            if !fallbacks.is_empty() {
                return Err(PublicationResolutionError::Invalid(
                    "profile selection cannot be combined with authored fallbacks".into(),
                ));
            }
            return self
                .resolve_profile_models(&catalog, workspace, &profile)
                .await;
        }
        let (primary_binding, fallback_bindings) =
            Self::selected_bindings(&catalog, selection, fallbacks)?;
        let all_bindings = std::iter::once(&primary_binding)
            .chain(fallback_bindings.iter())
            .collect::<Vec<_>>();
        let needs_credentials = all_bindings.iter().any(|binding| {
            Self::offering_for(&catalog, binding).is_some()
                || matches!(Backend::from_ref(&binding.backend_ref), Backend::Acp { .. })
        });
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
        let primary = if selection.backend_default_ref().is_some() {
            Self::backend_candidate(
                primary_binding.clone(),
                &sources,
                BackendModelSelection::Default,
            )?
        } else {
            self.candidate(&catalog, &sources, workspace, primary_binding.clone())?
        };
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
    //! Cause graph for explicit Workspace-profile publication:
    //! C1 selection names a profile; C2 that profile exists; C3 profile belongs to
    //! the execution Workspace; C4 every target identifies one active Offering;
    //! C5 each local credential binding resolves to an active compatible source;
    //! C6 `brokered` binding and Offering source agree.
    //! E1 use the authored ordered chain; E2 fail closed; E3 freeze
    //! the exact per-step credential/route; E4 reject the whole publication.
    //!
    //! Decision table:
    //! | Rule | C1 | C2 | C3 | C4 | C5 | C6 | Effect |
    //! | T1   | Y  | Y  | Y  | Y  | Y  | -  | E1+E3 |
    //! | T2   | Y  | N  | -  | -  | -  | -  | E2    |
    //! | T3   | Y  | Y  | N  | -  | -  | -  | E2    |
    //! | T4   | Y  | Y  | Y  | N  | -  | -  | E4    |
    //! | T5   | Y  | Y  | Y  | Y  | N  | -  | E4    |
    //! | T6   | N  | -  | -  | Y  | Y  | -  | pinned override |
    //! | T7   | Y  | Y  | Y  | Y  | -  | Y  | brokered pin, no local secret |
    //! | T8   | Y  | Y  | Y  | Y  | -  | N  | E4 |

    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_config_resolver::InMemoryProfileStore;
    use awaken_credential_vault::repo::{
        InMemoryCredentialRepo, ensure_worker_local, enter_credential,
    };
    use awaken_credential_vault::{
        CredentialCreateParams, InMemorySecretStore, WorkerLocalBinding,
    };
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

    fn explicit_profile() -> ModelSelection {
        ModelSelection::Profile {
            profile_id: "profile-a".into(),
        }
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
                "profile-a".into(),
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
            .resolve_models(&ScopeId::from("workspace-a"), &explicit_profile(), &[])
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
    async fn t3_explicit_profile_from_another_workspace_fails_closed() {
        let resolver = resolver(&["primary"]).await;
        let profiles = Arc::new(InMemoryProfileStore::new());
        profiles
            .put(
                "profile-a".into(),
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

        let error = resolver
            .resolve_models(&ScopeId::from("workspace-a"), &explicit_profile(), &[])
            .await
            .expect_err("foreign profile is absent in the execution Workspace");
        assert!(error.to_string().contains("does not exist"));
    }

    #[tokio::test]
    async fn t4_disabled_profile_target_rejects_the_publication() {
        let resolver = resolver(&["primary"]).await;
        let profiles = Arc::new(InMemoryProfileStore::new());
        profiles
            .put(
                "profile-a".into(),
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
            .resolve_models(&ScopeId::from("workspace-a"), &explicit_profile(), &[])
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
                "profile-a".into(),
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
            .resolve_models(&ScopeId::from("workspace-a"), &explicit_profile(), &[])
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
                "profile-a".into(),
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
                "profile-a".into(),
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
            .resolve_models(&ScopeId::from("workspace-a"), &explicit_profile(), &[])
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
    async fn t8_disabled_cloud_supply_rejects_brokered_without_byok_fallback() {
        // Cause graph: C1 a brokered Offering/Profile is cached; C2 Cloud model
        // supply is disabled; C3 a local inventory might exist. C1+C2 always
        // yields E1 explicit rejection; C3 cannot relabel or rescue the target.
        //
        // | Rule | brokered target | Cloud enabled | local fallback | Result |
        // |---|---:|---:|---:|---|
        // | B1 | 1 | 1 | - | publish brokered marker |
        // | B2 | 1 | 0 | 0 | cloud_models_disabled |
        // | B3 | 1 | 0 | 1 | cloud_models_disabled; no fallback |
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let mut catalog = catalog(&["managed-model"]);
        catalog.offerings[0].source = awaken_model_catalog::OfferingSource::Brokered;
        let profiles = Arc::new(InMemoryProfileStore::new());
        profiles
            .put(
                "profile-a".into(),
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

        let error = CatalogModelPublicationResolver::new(catalog, credentials)
            .with_profiles(profiles)
            .with_brokered_access(false)
            .resolve_models(&ScopeId::from("workspace-a"), &explicit_profile(), &[])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cloud_models_disabled"));
    }

    #[tokio::test]
    async fn t9_brokered_binding_cannot_relabel_a_direct_offering() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let profiles = Arc::new(InMemoryProfileStore::new());
        profiles
            .put(
                "profile-a".into(),
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
            .resolve_models(&ScopeId::from("workspace-a"), &explicit_profile(), &[])
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
    async fn provider_and_backend_ownership_remain_mutually_exclusive_on_acp() {
        // Cause graph: an exact provider/model match selects Provider provisioning
        // and preserves the authored ACP executor; without that provider match the
        // same ACP backend resolves only through its WorkerLocal identity.
        //
        // Decision table:
        // P1 provider + catalog model + ACP -> Provider/Vault on exact ACP backend
        // P2 WorkerLocal id + exact model  -> BackendOwned/CLI login
        // P3 bare model                    -> canonical native provider binding
        let resolver = resolver(&["primary"]).await;
        let managed = resolver
            .resolve_models(
                &ScopeId::from("workspace-a"),
                &ModelSelection::Pinned(ModelBinding::new("openai", "primary", "acp:codex")),
                &[],
            )
            .await
            .expect("P1");
        assert_eq!(managed.primary.binding.backend_ref, "acp:codex", "P1");
        assert!(
            matches!(
                managed.primary.provisioning,
                ModelProvisioning::Provider { .. }
            ),
            "P1"
        );

        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let local = ensure_worker_local(
            credentials.as_ref(),
            "workspace-a",
            WorkerLocalBinding::new("acp:codex", "default"),
            None,
        )
        .await
        .unwrap();
        let backend_owned =
            CatalogModelPublicationResolver::new(catalog(&["primary"]), credentials)
                .resolve_models(
                    &ScopeId::from("workspace-a"),
                    &ModelSelection::Pinned(ModelBinding::new(local.id.0, "primary", "acp:codex")),
                    &[],
                )
                .await
                .expect("P2");
        assert!(
            matches!(
                backend_owned.primary.provisioning,
                ModelProvisioning::BackendOwned { .. }
            ),
            "P2"
        );

        let native = resolver
            .resolve_models(
                &ScopeId::from("workspace-a"),
                &ModelSelection::Pinned(ModelBinding::new("", "primary", "")),
                &[],
            )
            .await
            .expect("P3");
        assert_eq!(native.primary.binding.backend_ref, "genai", "P3");
    }

    #[tokio::test]
    async fn claude_setup_token_is_selected_only_for_the_managed_claude_backend() {
        // Cause graph: exact provider offering + selected backend + credential
        // env semantics -> one published CredentialAccess usage.
        //
        // | Rule | Backend | API key | setup token | Selected usage |
        // | S1 | genai | yes | yes | API key / ProviderAdapter |
        // | S2 | acp:claude | yes | yes | setup token / exact env |
        // | S3 | acp:gemini | yes | yes | no compatible credential |
        let mut catalog = ProviderCatalog::default();
        catalog.providers.insert(
            "anthropic".into(),
            Provider {
                id: ProviderId::new("anthropic"),
                slug: "anthropic".into(),
                display_name: "Anthropic".into(),
                version: 1,
            },
        );
        catalog.endpoints.insert(
            "anthropic-messages".into(),
            ProtocolEndpoint {
                id: ProtocolEndpointId::new("anthropic-messages"),
                provider_id: ProviderId::new("anthropic"),
                dialect: ApiDialect::AnthropicMessages,
                base_url: Some("https://api.anthropic.com/v1".into()),
                timeout_secs: 30,
                display_name: "Anthropic".into(),
                version: 1,
            },
        );
        catalog.offerings = vec![Offering {
            model_id: "claude-test".into(),
            provider_id: ProviderId::new("anthropic"),
            protocol_endpoint_id: ProtocolEndpointId::new("anthropic-messages"),
            dialect: ApiDialect::AnthropicMessages,
            upstream_model: None,
            source: Default::default(),
            status: Default::default(),
            last_seen_at_unix_ms: None,
        }];
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = InMemorySecretStore::new();
        let api_key = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: Some("ANTHROPIC_API_KEY".into()),
                secret: Some(RedactedString::new("api-key")),
                oauth_command: None,
            },
            &secrets,
            credentials.as_ref(),
        )
        .await
        .unwrap();
        let setup_token = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: Some(awaken_credential_vault::CLAUDE_CODE_SETUP_TOKEN_ENV.into()),
                secret: Some(RedactedString::new("setup-token")),
                oauth_command: None,
            },
            &secrets,
            credentials.as_ref(),
        )
        .await
        .unwrap();
        let resolver = CatalogModelPublicationResolver::new(catalog, credentials);

        let native = resolver
            .resolve_models(
                &ScopeId::from("workspace-a"),
                &ModelSelection::Pinned(ModelBinding::new("anthropic", "claude-test", "genai")),
                &[],
            )
            .await
            .expect("S1");
        let ModelProvisioning::Provider {
            credential: Some(native_access),
            ..
        } = native.primary.provisioning
        else {
            panic!("S1 provider credential")
        };
        assert_eq!(native_access.credential.id, api_key.id.0, "S1");
        assert_eq!(native_access.usage, CredentialUsage::ProviderAdapter, "S1");

        let claude = resolver
            .resolve_models(
                &ScopeId::from("workspace-a"),
                &ModelSelection::Pinned(ModelBinding::new(
                    "anthropic",
                    "claude-test",
                    "acp:claude",
                )),
                &[],
            )
            .await
            .expect("S2");
        let ModelProvisioning::Provider {
            credential: Some(claude_access),
            ..
        } = claude.primary.provisioning
        else {
            panic!("S2 provider credential")
        };
        assert_eq!(claude_access.credential.id, setup_token.id.0, "S2");
        assert_eq!(
            claude_access.usage,
            CredentialUsage::EnvironmentVariable {
                name: awaken_credential_vault::CLAUDE_CODE_SETUP_TOKEN_ENV.into(),
            },
            "S2"
        );

        assert!(
            resolver
                .resolve_models(
                    &ScopeId::from("workspace-a"),
                    &ModelSelection::Pinned(ModelBinding::new(
                        "anthropic",
                        "claude-test",
                        "acp:gemini",
                    )),
                    &[],
                )
                .await
                .is_err(),
            "S3"
        );
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
        let source = ensure_worker_local(
            credentials.as_ref(),
            "workspace-a",
            WorkerLocalBinding::new("provider:openai", "default"),
            Some("openai".into()),
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

    #[tokio::test]
    async fn backend_owned_publication_pins_one_local_binding_without_material() {
        // Cause graph:
        // authored backend policy -> executable ACP catalog row -> active exact
        // WorkerLocal locator -> immutable BackendOwned candidate. No endpoint,
        // API key, or materialization edge exists.
        //
        // Decision table:
        // B1 default + one binding -> Default, empty model, exact credential
        // B2 exact + one binding   -> Exact, authored model, exact credential
        // B3 no binding            -> CandidateUnavailable
        // B4 default + many        -> CandidateUnavailable (never random)
        // B5 exact source id + many-> selected exact source
        // B6 unknown ACP backend   -> CandidateUnavailable
        // B7 unsupported exact     -> CandidateUnavailable at publication
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let codex = ensure_worker_local(
            credentials.as_ref(),
            "workspace-a",
            WorkerLocalBinding::new("acp:codex", "default"),
            None,
        )
        .await
        .unwrap();
        let resolver =
            CatalogModelPublicationResolver::new(ProviderCatalog::default(), credentials.clone());

        let default = resolver
            .resolve_models(
                &ScopeId::from("workspace-a"),
                &ModelSelection::BackendDefault {
                    backend_ref: "acp:codex".into(),
                },
                &[],
            )
            .await
            .expect("B1");
        assert_eq!(default.primary.binding.model_ref, "", "B1");
        assert_eq!(
            default.primary.binding.provider_identity_ref, codex.id.0,
            "B1"
        );
        assert!(
            matches!(
                default.primary.provisioning,
                ModelProvisioning::BackendOwned {
                    ref credential,
                    model_selection: BackendModelSelection::Default,
                } if credential.id == codex.id.0 && credential.revision == 1
            ),
            "B1"
        );

        let exact = resolver
            .resolve_models(
                &ScopeId::from("workspace-a"),
                &ModelSelection::Pinned(ModelBinding::new("", "gpt-exact", "acp:codex")),
                &[],
            )
            .await
            .expect("B2");
        assert_eq!(exact.primary.binding.model_ref, "gpt-exact", "B2");
        assert!(
            matches!(
                exact.primary.provisioning,
                ModelProvisioning::BackendOwned {
                    model_selection: BackendModelSelection::Exact,
                    ..
                }
            ),
            "B2"
        );

        assert!(
            matches!(
                resolver
                    .resolve_models(
                        &ScopeId::from("workspace-a"),
                        &ModelSelection::BackendDefault {
                            backend_ref: "acp:claude".into(),
                        },
                        &[],
                    )
                    .await,
                Err(PublicationResolutionError::CandidateUnavailable { .. })
            ),
            "B3"
        );

        let secondary = ensure_worker_local(
            credentials.as_ref(),
            "workspace-a",
            WorkerLocalBinding::new("acp:codex", "secondary"),
            None,
        )
        .await
        .unwrap();
        assert!(
            matches!(
                resolver
                    .resolve_models(
                        &ScopeId::from("workspace-a"),
                        &ModelSelection::BackendDefault {
                            backend_ref: "acp:codex".into(),
                        },
                        &[],
                    )
                    .await,
                Err(PublicationResolutionError::CandidateUnavailable { .. })
            ),
            "B4"
        );

        let selected = resolver
            .resolve_models(
                &ScopeId::from("workspace-a"),
                &ModelSelection::Pinned(ModelBinding::new(
                    secondary.id.0.clone(),
                    "gpt-exact",
                    "acp:codex",
                )),
                &[],
            )
            .await
            .expect("B5");
        assert_eq!(
            selected.primary.binding.provider_identity_ref, secondary.id.0,
            "B5"
        );

        assert!(
            matches!(
                resolver
                    .resolve_models(
                        &ScopeId::from("workspace-a"),
                        &ModelSelection::BackendDefault {
                            backend_ref: "acp:not-installed".into(),
                        },
                        &[],
                    )
                    .await,
                Err(PublicationResolutionError::CandidateUnavailable { .. })
            ),
            "B6"
        );
        let error = resolver
            .resolve_models(
                &ScopeId::from("workspace-a"),
                &ModelSelection::Pinned(ModelBinding::new("", "model-x", "acp:opencode")),
                &[],
            )
            .await
            .expect_err("B7");
        assert!(error.to_string().contains("cannot guarantee"), "B7");
    }
}
