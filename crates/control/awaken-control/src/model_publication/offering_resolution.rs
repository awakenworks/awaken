//! Exact catalog-offering lookup for publication model selections.

use awaken_config_resolver::{ModelTarget, select_offering};
use awaken_config_service::PublicationResolutionError;
use awaken_model_catalog::{Offering, ProviderCatalog};
use awaken_runtime_contract::resolved::ModelBinding;

pub(super) fn offering_for<'a>(
    catalog: &'a ProviderCatalog,
    binding: &ModelBinding,
) -> Result<&'a Offering, PublicationResolutionError> {
    let target = ModelTarget {
        model_id: binding.model_ref.clone(),
        provider_id: (!binding.provider_identity_ref.is_empty())
            .then(|| binding.provider_identity_ref.clone()),
        api_dialect: None,
        protocol_endpoint_id: None,
        endpoint_name: None,
    };
    select_offering(catalog, &target, &[]).map_err(|error| {
        PublicationResolutionError::CandidateUnavailable {
            binding: binding.clone(),
            reason: error.to_string(),
        }
    })
}

pub(super) fn offering_for_target<'a>(
    catalog: &'a ProviderCatalog,
    target: &ModelTarget,
    disabled_endpoints: &[String],
) -> Result<&'a Offering, PublicationResolutionError> {
    select_offering(catalog, target, disabled_endpoints).map_err(|error| {
        PublicationResolutionError::CandidateUnavailable {
            binding: ModelBinding::new(
                target.provider_id.as_deref().unwrap_or_default(),
                &target.model_id,
                "genai",
            ),
            reason: error.to_string(),
        }
    })
}
