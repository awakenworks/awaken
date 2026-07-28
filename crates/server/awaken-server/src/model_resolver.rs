//! Catalog-backed publication of complete model candidates.
//!
//! The adapter reads one catalog snapshot and, when required, one Workspace
//! credential inventory. It resolves both authored model selection and provider
//! provisioning in that consistency window. Runtime code never calls this
//! adapter; it receives only the resulting immutable candidates.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use awaken_config_resolver::validate_acp_dialect;
use awaken_config_resolver::{
    InferenceProfile, InferenceProfileStore, ModelTarget, derive_vendor_pool, get_workspace_profile,
};
use awaken_config_store::ModelSelection;
use awaken_credential_vault::repo::CredentialRepo;
use awaken_credential_vault::{
    CredentialBinding, CredentialKind, CredentialSource, CredentialStatus,
};
use awaken_model_catalog::{Offering, ProviderCatalog};
use awaken_runtime_contract::CredentialRef;
use awaken_runtime_contract::resolved::{
    Backend, BackendModelSelection, ModelBinding, ResolvedModelCandidate,
};
use awaken_runtime_host::{
    ModelPublicationResolver, PublicationResolutionError, ResolvedPublicationModels,
};
use awaken_tenancy::ScopeId;

mod acp_configuration;
mod composition;
mod credential_publication;
use acp_configuration::validate_acp_session_configuration;
use composition::CatalogSource;
use credential_publication::PublicationCredentialLookup;

/// Configuration-plane adapter that freezes model, route and credential facts
/// into a publication. Every candidate must exist in the catalog; explicit
/// in-process scenario executors use their own composition resolver.
#[derive(Clone)]
pub struct CatalogModelPublicationResolver {
    source: CatalogSource,
    credentials: Arc<dyn CredentialRepo>,
    profiles: Option<Arc<dyn InferenceProfileStore>>,
    brokered_access_enabled: bool,
    workers: Option<Arc<dyn awaken_worker_registry::WorkerDirectory>>,
    credential_selection_sequences: Arc<Mutex<HashMap<String, u64>>>,
}

impl CatalogModelPublicationResolver {
    async fn snapshot(&self) -> Result<ProviderCatalog, PublicationResolutionError> {
        self.source
            .snapshot()
            .await
            .map_err(PublicationResolutionError::CatalogUnavailable)
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
        if let Some((backend_ref, model_ref)) = selection.backend_exact() {
            let primary = ModelBinding::new("", model_ref, backend_ref);
            Self::validate_acp_binding(&primary, BackendModelSelection::Exact)?;
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

    async fn candidate(
        &self,
        catalog: &ProviderCatalog,
        sources: &[CredentialSource],
        workspace: &ScopeId,
        binding: ModelBinding,
    ) -> Result<ResolvedModelCandidate, PublicationResolutionError> {
        if let Some(offering) = Self::offering_for(catalog, &binding) {
            validate_acp_dialect(&binding.backend_ref, offering.dialect).map_err(|error| {
                PublicationResolutionError::CandidateUnavailable {
                    binding: binding.clone(),
                    reason: error.to_string(),
                }
            })?;
            let pool = derive_vendor_pool(
                workspace.as_str(),
                offering.provider_id.as_str(),
                &binding.backend_ref,
                sources,
            );
            let derived_binding = CredentialBinding::OneOfCredentialPool {
                credential_pool_id: pool.id.clone(),
            };
            let lookup = PublicationCredentialLookup {
                sources,
                pool: Some(&pool),
            };
            let access = self
                .publication_access(&lookup, workspace, offering, &derived_binding, &binding)
                .map_err(|reason| PublicationResolutionError::CandidateUnavailable {
                    binding: binding.clone(),
                    reason: format!(
                        "no active persisted credential can consume model {} in Workspace {workspace}: {reason}",
                        binding.model_ref
                    ),
                })?;
            return Self::provider_candidate(catalog, workspace, binding, offering, access);
        }
        if matches!(Backend::from_ref(&binding.backend_ref), Backend::Acp { .. }) {
            let configuration = Default::default();
            return self
                .backend_candidate(
                    binding,
                    sources,
                    BackendModelSelection::Exact,
                    &configuration,
                )
                .await;
        }
        Err(PublicationResolutionError::CandidateUnavailable {
            reason: format!("model offering {} is not published", binding.model_ref),
            binding,
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
            if offering.source == awaken_model_catalog::OfferingSource::Brokered
                && !self.brokered_access_enabled
            {
                return Err(PublicationResolutionError::CandidateUnavailable {
                    binding: Self::binding_of(offering),
                    reason: "cloud_models_disabled: brokered model supply is disabled".into(),
                });
            }
            let pool = if let CredentialBinding::OneOfCredentialPool { credential_pool_id } =
                &candidate.credential_binding
            {
                Some(
                    self.credentials
                        .get_pool(credential_pool_id)
                        .await
                        .map_err(|error| {
                            PublicationResolutionError::CredentialInventoryUnavailable(
                                error.to_string(),
                            )
                        })?,
                )
            } else {
                None
            };
            let lookup = PublicationCredentialLookup {
                sources: &sources,
                pool: pool.as_ref(),
            };
            let binding = Self::binding_of(offering);
            let access = self
                .publication_access(
                    &lookup,
                    workspace,
                    offering,
                    &candidate.credential_binding,
                    &binding,
                )
                .map_err(|reason| PublicationResolutionError::CandidateUnavailable {
                    binding: binding.clone(),
                    reason,
                })?;
            resolved.push(Self::provider_candidate(
                catalog, workspace, binding, offering, access,
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

    async fn backend_candidate(
        &self,
        binding: ModelBinding,
        sources: &[CredentialSource],
        model_selection: BackendModelSelection,
        session_configuration: &awaken_runtime_contract::resolved::AcpSessionConfiguration,
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
        let required_credential = awaken_worker_registry::WorkerCredentialRevision {
            id: source.id.0.clone(),
            revision,
        };
        let (capability_adapter_version, capability_fingerprint, negotiated) = self
            .verified_backend_capability(
                &resolved_binding.backend_ref,
                &required_credential,
                wall_clock_ms(),
            )
            .await?;
        validate_acp_session_configuration(&resolved_binding, session_configuration, &negotiated)?;
        Ok(ResolvedModelCandidate::backend_owned(
            resolved_binding,
            CredentialRef {
                id: source.id.0.clone(),
                revision,
            },
            model_selection,
            capability_adapter_version,
            capability_fingerprint,
            session_configuration.clone(),
        ))
    }

    async fn verified_backend_capability(
        &self,
        backend_ref: &str,
        credential: &awaken_worker_registry::WorkerCredentialRevision,
        now_ms: u64,
    ) -> Result<
        (
            String,
            String,
            awaken_acp_contract::NegotiatedAcpCapabilities,
        ),
        PublicationResolutionError,
    > {
        let workers = self.workers.as_ref().ok_or_else(|| {
            PublicationResolutionError::CandidateUnavailable {
                binding: ModelBinding::new(&credential.id, "", backend_ref),
                reason: "live Worker capability observations are unavailable".into(),
            }
        })?;
        let snapshots = workers.list().await.map_err(|error| {
            PublicationResolutionError::CandidateUnavailable {
                binding: ModelBinding::new(&credential.id, "", backend_ref),
                reason: format!("Worker capability observations are unavailable: {error}"),
            }
        })?;
        let mut fingerprints = snapshots
            .into_iter()
            .map(|registered| registered.snapshot)
            .filter(|worker| {
                worker.state.accepts_work()
                    && worker.expires_at_ms > now_ms
                    && worker
                        .credential_observations
                        .iter()
                        .any(|observation| observation.is_selectable_at(credential, now_ms))
            })
            .flat_map(|worker| worker.acp_capability_observations)
            .filter(|capability| {
                capability.valid_until_ms > now_ms
                    && capability.observation.observed_at_ms <= now_ms
                    && capability.observation.backend_ref == backend_ref
                    && capability.observation.state
                        == awaken_acp_contract::AcpCapabilityObservationState::Verified
            })
            .filter_map(|capability| {
                capability.observation.fingerprint.and_then(|fingerprint| {
                    capability.observation.negotiated.map(|negotiated| {
                        (
                            capability.observation.adapter_version,
                            fingerprint,
                            negotiated,
                        )
                    })
                })
            })
            .collect::<Vec<_>>();
        fingerprints.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
        fingerprints.dedup_by(|left, right| left.0 == right.0 && left.1 == right.1);
        match (fingerprints.pop(), fingerprints.is_empty()) {
            (Some((version, fingerprint, negotiated)), true)
                if !version.trim().is_empty() && !fingerprint.trim().is_empty() =>
            {
                Ok((version, fingerprint, negotiated))
            }
            (None, _) => Err(PublicationResolutionError::CandidateUnavailable {
                binding: ModelBinding::new(&credential.id, "", backend_ref),
                reason: format!("no fresh verified ACP capability is available for {backend_ref}"),
            }),
            _ => Err(PublicationResolutionError::CandidateUnavailable {
                binding: ModelBinding::new(&credential.id, "", backend_ref),
                reason: format!(
                    "multiple incompatible ACP capability fingerprints are live for {backend_ref}"
                ),
            }),
        }
    }
}

fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
            self.backend_candidate(
                primary_binding.clone(),
                &sources,
                BackendModelSelection::Default,
                selection
                    .acp_configuration()
                    .expect("backend default has ACP configuration"),
            )
            .await?
        } else if selection.backend_exact().is_some() {
            self.backend_candidate(
                primary_binding.clone(),
                &sources,
                BackendModelSelection::Exact,
                selection
                    .acp_configuration()
                    .expect("backend exact has ACP configuration"),
            )
            .await?
        } else {
            self.candidate(&catalog, &sources, workspace, primary_binding.clone())
                .await?
        };
        let mut candidates = Vec::with_capacity(fallback_bindings.len());
        for binding in fallback_bindings {
            candidates.push(
                self.candidate(&catalog, &sources, workspace, binding)
                    .await?,
            );
        }
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
    use awaken_config_resolver::{InMemoryProfileStore, ProfileCandidate};
    use awaken_credential_vault::repo::{
        InMemoryCredentialRepo, ensure_worker_local, enter_credential,
    };
    use awaken_credential_vault::{
        CredentialCreateParams, CredentialPool, CredentialPoolId, CredentialPoolMember,
        InMemorySecretStore, SelectionPolicy, WorkerLocalBinding,
    };
    use awaken_model_catalog::{
        ApiDialect, ModelAttributes, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
    };
    use awaken_runtime_contract::resolved::ModelProvisioning;
    use awaken_runtime_contract::{CredentialMaterialSource, CredentialUsage};
    use awaken_worker_registry::{
        MemoryWorkerDirectory, WorkerAcpCapabilityObservation, WorkerCredentialObservation,
        WorkerCredentialRevision, WorkerDirectory, WorkerHeartbeat, WorkerManifest,
        WorkerRegistration,
    };

    async fn verified_acp_worker(
        credential_id: &str,
        backend_ref: &str,
        fingerprint: &str,
    ) -> Arc<dyn WorkerDirectory> {
        let directory = Arc::new(MemoryWorkerDirectory::new());
        let now = wall_clock_ms();
        let registered = directory
            .register(
                WorkerRegistration {
                    worker_id: format!("worker-{backend_ref}"),
                    incarnation_id: "boot-1".into(),
                    manifest: WorkerManifest::default(),
                },
                now,
                60_000,
            )
            .await
            .expect("register capability Worker");
        directory
            .heartbeat(
                &registered.snapshot.identity,
                WorkerHeartbeat {
                    sequence: 1,
                    ready: true,
                    in_flight: 0,
                    credential_observations: [WorkerCredentialObservation::available(
                        WorkerCredentialRevision {
                            id: credential_id.into(),
                            revision: 1,
                        },
                        now,
                        now + 30_000,
                    )]
                    .into_iter()
                    .collect(),
                    acp_capability_observations: vec![WorkerAcpCapabilityObservation {
                        observation: awaken_acp_contract::AcpCapabilityObservation {
                            backend_ref: backend_ref.into(),
                            adapter_version: "test".into(),
                            state: awaken_acp_contract::AcpCapabilityObservationState::Verified,
                            observed_at_ms: now,
                            fingerprint: Some(fingerprint.into()),
                            negotiated: Some(awaken_acp_contract::NegotiatedAcpCapabilities {
                                protocol_version: "1".into(),
                                load_session: false,
                                prompt_image: false,
                                prompt_audio: false,
                                prompt_embedded_context: false,
                                mcp_http: false,
                                mcp_sse: false,
                                session_list: false,
                                modes: Vec::new(),
                                config_options: Vec::new(),
                            }),
                            reason_code: None,
                        },
                        valid_until_ms: now + 30_000,
                    }],
                },
                now,
                60_000,
            )
            .await
            .expect("publish capability Worker");
        directory
    }

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

    // Cause/effect decision table for D5 publication-time pool selection:
    // R1 one RotateSpread pool + sequence 0 -> first authored healthy member;
    // R2 same resolver/pool + next publication -> next healthy member;
    // E1 each immutable publication freezes exactly the selected revision;
    // E2 Runtime receives no pool or reselection authority.
    #[tokio::test]
    async fn rotate_spread_pool_rotates_across_publications_before_freezing_exact_access() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = InMemorySecretStore::new();
        let first = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("openai".into()),
                env_key: Some("OPENAI_API_KEY".into()),
                secret: Some(RedactedString::new("first")),
                oauth_command: None,
            },
            &secrets,
            credentials.as_ref(),
        )
        .await
        .unwrap();
        let second = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("openai".into()),
                env_key: Some("OPENAI_API_KEY".into()),
                secret: Some(RedactedString::new("second")),
                oauth_command: None,
            },
            &secrets,
            credentials.as_ref(),
        )
        .await
        .unwrap();
        let pool_id = CredentialPoolId("pool:rotate".into());
        credentials
            .put_pool(CredentialPool {
                id: pool_id.clone(),
                workspace_id: "workspace-a".into(),
                members: vec![
                    CredentialPoolMember {
                        credential_source_id: first.id.clone(),
                        ordinal: 0,
                        enabled: true,
                        selection_weight: 0,
                    },
                    CredentialPoolMember {
                        credential_source_id: second.id.clone(),
                        ordinal: 1,
                        enabled: true,
                        selection_weight: 0,
                    },
                ],
                policy: SelectionPolicy::RotateSpread,
            })
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
                        credential_binding: CredentialBinding::OneOfCredentialPool {
                            credential_pool_id: pool_id,
                        },
                    },
                    fallbacks: Vec::new(),
                    disabled_endpoint_ids: Vec::new(),
                },
            )
            .unwrap();
        let resolver = CatalogModelPublicationResolver::new(catalog(&["primary"]), credentials)
            .with_profiles(profiles);
        let selected_id = |models: &ResolvedPublicationModels| match &models.primary.provisioning {
            ModelProvisioning::Provider {
                credential: Some(access),
                ..
            } => access.credential.id.clone(),
            _ => panic!("pool publication must freeze one exact credential"),
        };

        let publication_0 = resolver
            .resolve_models(&ScopeId::from("workspace-a"), &explicit_profile(), &[])
            .await
            .unwrap();
        let publication_1 = resolver
            .resolve_models(&ScopeId::from("workspace-a"), &explicit_profile(), &[])
            .await
            .unwrap();
        assert_eq!(selected_id(&publication_0), first.id.0, "R1/E1");
        assert_eq!(selected_id(&publication_1), second.id.0, "R2/E1");
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
        let workers = verified_acp_worker(&local.id.0, "acp:codex", "sha256:codex-test").await;
        let backend_owned =
            CatalogModelPublicationResolver::new(catalog(&["primary"]), credentials)
                .with_worker_directory(workers)
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
        let workers = verified_acp_worker(&codex.id.0, "acp:codex", "sha256:codex-test").await;
        let resolver =
            CatalogModelPublicationResolver::new(ProviderCatalog::default(), credentials.clone())
                .with_worker_directory(workers);

        let default = resolver
            .resolve_models(
                &ScopeId::from("workspace-a"),
                &ModelSelection::BackendDefault {
                    backend_ref: "acp:codex".into(),
                    configuration: Default::default(),
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
                &default.primary.provisioning,
                ModelProvisioning::BackendOwned {
                    credential,
                    model_selection: BackendModelSelection::Default,
                    ..
                } if credential.id == codex.id.0 && credential.revision == 1
            ),
            "B1"
        );
        assert!(matches!(
            &default.primary.provisioning,
            ModelProvisioning::BackendOwned {
                capability_fingerprint,
                ..
            } if capability_fingerprint == "sha256:codex-test"
        ));

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
                            configuration: Default::default(),
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
                            configuration: Default::default(),
                        },
                        &[],
                    )
                    .await,
                Err(PublicationResolutionError::CandidateUnavailable { .. })
            ),
            "B4"
        );

        let secondary_workers =
            verified_acp_worker(&secondary.id.0, "acp:codex", "sha256:codex-test").await;
        let exact_resolver =
            CatalogModelPublicationResolver::new(ProviderCatalog::default(), credentials.clone())
                .with_worker_directory(secondary_workers);
        let selected = exact_resolver
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
                            configuration: Default::default(),
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
