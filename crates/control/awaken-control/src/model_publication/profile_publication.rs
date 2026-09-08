//! Compile authored Workspace profiles through the canonical credential and
//! model-supply authorities, preserving authored order and Cloud account pools.
//! Component regression tests remain in the parent publication test suite.
use super::credential_publication::PublicationCredentialLookup;
use super::*;
use awaken_config_resolver::InferenceProfile;

impl CatalogModelPublicationResolver {
    pub(super) async fn resolve_brokered_offering(
        &self,
        resolver: &dyn ModelPublicationResolver,
        workspace: &ScopeId,
        offering: &Offering,
        fallbacks: &[ModelBinding],
    ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
        if !self.brokered_access_enabled {
            return Err(PublicationResolutionError::CandidateUnavailable {
                binding: Self::binding_of(offering),
                reason: "cloud_models_disabled: brokered model supply is disabled".into(),
            });
        }
        resolver
            .resolve_models(
                workspace,
                &ModelSelection::Target {
                    target: ModelTarget {
                        model_id: offering.model_id.clone(),
                        provider_id: Some(offering.provider_id.0.clone()),
                        api_dialect: Some(offering.dialect.as_str().into()),
                        protocol_endpoint_id: None,
                        endpoint_name: None,
                    },
                    backend_ref: "genai".into(),
                    configuration: Default::default(),
                },
                fallbacks,
            )
            .await
    }

    pub(super) async fn resolve_profile_models(
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
                offering_for_target(catalog, &candidate.target, &profile.disabled_endpoint_ids)
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
            if matches!(access, credential_publication::PublicationAccess::Brokered)
                && let Some(resolver) = &self.brokered_publication
            {
                let hosted = self
                    .resolve_brokered_offering(resolver.as_ref(), workspace, offering, &[])
                    .await?;
                resolved.push(hosted.primary);
                resolved.extend(hosted.candidates);
                continue;
            }
            resolved.push(
                self.provider_candidate(catalog, workspace, binding, offering, access, None)
                    .await?,
            );
        }
        let primary = resolved.remove(0);
        let primary_model = primary.binding().model_ref.clone();
        Ok(ResolvedPublicationModels {
            primary,
            candidates: resolved,
            context_window: catalog.context_window(&primary_model),
            max_output_tokens: catalog.max_output_tokens(&primary_model),
        })
    }
}
