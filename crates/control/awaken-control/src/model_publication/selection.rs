//! Pure authored-model selection and canonical binding validation.

use awaken_acp_contract::AcpPublicationCapability;
use awaken_agent_config::ModelSelection;
use awaken_config_resolver::{ModelTarget, select_offering};
use awaken_config_service::PublicationResolutionError;
use awaken_model_catalog::ProviderCatalog;
use awaken_runtime_contract::resolved::{Backend, BackendModelSelection, ModelBinding};

use super::CatalogModelPublicationResolver;

impl CatalogModelPublicationResolver {
    pub(super) fn selected_bindings(
        &self,
        catalog: &ProviderCatalog,
        selection: &ModelSelection,
        fallbacks: &[ModelBinding],
    ) -> Result<(ModelBinding, Vec<ModelBinding>), PublicationResolutionError> {
        if let Some(primary) = selection.resolved() {
            return Ok((
                self.canonical_binding(catalog, primary)?,
                fallbacks
                    .iter()
                    .map(|binding| self.canonical_binding(catalog, binding))
                    .collect::<Result<Vec<_>, _>>()?,
            ));
        }
        if let Some((target, backend_ref)) = selection.target() {
            let offering = select_offering(catalog, target, &[]).map_err(|error| {
                super::offering_resolution::target_unavailable(
                    target,
                    backend_ref,
                    error.to_string(),
                )
            })?;
            let primary = ModelBinding::parse(
                offering.provider_id.as_str(),
                &offering.model_id,
                backend_ref,
            )
            .map_err(|error| {
                PublicationResolutionError::invalid_binding(
                    format!(
                        "catalog offering `{}` for authored target on backend `{backend_ref}`",
                        offering.model_id
                    ),
                    error,
                )
            })?;
            return Ok((
                primary,
                fallbacks
                    .iter()
                    .map(|binding| self.canonical_binding(catalog, binding))
                    .collect::<Result<Vec<_>, _>>()?,
            ));
        }
        if let Some(backend_ref) = selection.backend_default_ref() {
            let primary = ModelBinding::parse("", "", backend_ref).map_err(|error| {
                PublicationResolutionError::invalid_binding(
                    format!("authored backend-default selection on `{backend_ref}`"),
                    error,
                )
            })?;
            self.validate_acp_binding(&primary, BackendModelSelection::Default)?;
            return Ok((
                primary,
                fallbacks
                    .iter()
                    .map(|binding| self.canonical_binding(catalog, binding))
                    .collect::<Result<Vec<_>, _>>()?,
            ));
        }
        if let Some((backend_ref, model_ref)) = selection.backend_exact() {
            let primary = ModelBinding::parse("", model_ref, backend_ref).map_err(|error| {
                PublicationResolutionError::invalid_binding(
                    format!("authored backend-exact selection `{model_ref}` on `{backend_ref}`"),
                    error,
                )
            })?;
            self.validate_acp_binding(&primary, BackendModelSelection::Exact)?;
            return Ok((
                primary,
                fallbacks
                    .iter()
                    .map(|binding| self.canonical_binding(catalog, binding))
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
            Self::binding_of(primary)?,
            offerings
                .map(Self::binding_of)
                .collect::<Result<Vec<_>, _>>()?,
        ))
    }

    /// Validate a complete internal binding against the current executable
    /// catalog. Public partial/qualified model syntax is represented only by
    /// `ModelSelection::Target`; Pinned never fills an omitted axis.
    pub(super) fn canonical_binding(
        &self,
        catalog: &ProviderCatalog,
        binding: &ModelBinding,
    ) -> Result<ModelBinding, PublicationResolutionError> {
        let backend = Backend::from_ref(&binding.backend_ref);
        if let Backend::Remote(endpoint) = backend {
            Self::remote_origin(endpoint.endpoint()).map_err(|reason| {
                PublicationResolutionError::CandidateUnavailable {
                    binding: binding.clone(),
                    reason,
                }
            })?;
            return Ok(binding.clone());
        }
        if matches!(backend, Backend::Acp(_)) {
            if binding.provider_identity_ref.trim().is_empty() {
                return Err(PublicationResolutionError::CandidateUnavailable {
                    binding: binding.clone(),
                    reason: "Pinned ACP binding requires its exact Worker-local identity; use Target for a Provider route or BackendExact for selection intent".into(),
                });
            }
            self.validate_acp_binding(binding, BackendModelSelection::Exact)?;
            return Ok(binding.clone());
        }
        if let Backend::Invalid(invalid) = backend {
            return Err(PublicationResolutionError::CandidateUnavailable {
                binding: binding.clone(),
                reason: format!("invalid backend_ref {}", invalid.as_str()),
            });
        }
        if binding.provider_identity_ref.trim().is_empty()
            || binding.model_ref.trim().is_empty()
            || binding.backend_ref.trim().is_empty()
        {
            return Err(PublicationResolutionError::CandidateUnavailable {
                binding: binding.clone(),
                reason: "Pinned model binding must contain provider, model, and backend; use Target or BackendExact for public selection intent".into(),
            });
        }
        let target = ModelTarget {
            model_id: binding.model_ref.clone(),
            provider_id: (!binding.provider_identity_ref.is_empty())
                .then(|| binding.provider_identity_ref.clone()),
            api_dialect: None,
            protocol_endpoint_id: None,
            endpoint_name: None,
        };
        let offering = select_offering(catalog, &target, &[]).map_err(|error| {
            PublicationResolutionError::CandidateUnavailable {
                binding: binding.clone(),
                reason: error.to_string(),
            }
        })?;
        ModelBinding::parse(
            offering.provider_id.as_str(),
            &offering.model_id,
            &binding.backend_ref,
        )
        .map_err(|error| {
            PublicationResolutionError::invalid_binding(
                format!(
                    "catalog offering `{}` canonicalizing pinned binding `{}`",
                    offering.model_id, binding.model_ref
                ),
                error,
            )
        })
    }

    pub(super) fn validate_acp_binding(
        &self,
        binding: &ModelBinding,
        selection: BackendModelSelection,
    ) -> Result<&AcpPublicationCapability, PublicationResolutionError> {
        let Backend::Acp(backend) = Backend::from_ref(&binding.backend_ref) else {
            return Err(PublicationResolutionError::CandidateUnavailable {
                binding: binding.clone(),
                reason: "backend-owned model selection requires an ACP backend".into(),
            });
        };
        let cli = backend.cli();
        if binding.backend_ref != format!("acp:{cli}") {
            return Err(PublicationResolutionError::CandidateUnavailable {
                binding: binding.clone(),
                reason: "backend-owned model selection requires an exact acp:<cli> backend".into(),
            });
        }
        let profile = self
            .acp_capabilities
            .iter()
            .find(|capability| capability.backend_ref == binding.backend_ref)
            .ok_or_else(|| PublicationResolutionError::CandidateUnavailable {
                binding: binding.clone(),
                reason: format!("ACP backend {cli} is not in the executable catalog"),
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
        if selection == BackendModelSelection::Exact && !profile.model_selection.admits_exact() {
            return Err(PublicationResolutionError::CandidateUnavailable {
                binding: binding.clone(),
                reason: format!("ACP backend {cli} cannot guarantee an exact model selection"),
            });
        }
        Ok(profile)
    }
}
