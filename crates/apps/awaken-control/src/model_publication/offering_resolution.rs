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
        model_id: binding.identity().1.to_owned(),
        provider_id: (!binding.identity().0.is_empty()).then(|| binding.identity().0.to_owned()),
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
    select_offering(catalog, target, disabled_endpoints)
        .map_err(|error| target_unavailable(target, "genai", error.to_string()))
}

/// `CandidateUnavailable` for an authored target that the catalog could not
/// serve. An authored partial target is valid selection intent, so a missing
/// provider must not replace the catalog's ambiguity/not-found cause with an
/// unrelated executable-binding validation error.
pub(super) fn target_unavailable(
    target: &ModelTarget,
    backend_ref: &str,
    reason: String,
) -> PublicationResolutionError {
    if target.provider_id.is_none() {
        return PublicationResolutionError::Invalid(format!(
            "authored model target `{}` on backend `{backend_ref}` cannot be published: {reason}",
            target.model_id
        ));
    }
    match ModelBinding::parse(
        target.provider_id.as_deref().unwrap_or_default(),
        &target.model_id,
        backend_ref,
    ) {
        Ok(binding) => PublicationResolutionError::CandidateUnavailable { binding, reason },
        Err(error) => PublicationResolutionError::invalid_binding(
            format!("authored model target `{}`", target.model_id),
            error,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_target_keeps_catalog_refusal_instead_of_inventing_a_binding() {
        // Causes: T1 omitted provider is legal selection intent; T2 a complete
        // target names a binding; T3 an explicitly empty provider is malformed.
        // Effects: E1 preserve catalog ambiguity/not-found, E2 retain the
        // existing CandidateUnavailable, E3 reject malformed coordinates.
        // Rules: T1 -> E1, T2 -> E2, T3 -> E3. No credential/runtime effects.
        for reason in ["ambiguous model: choose an endpoint", "model not found"] {
            let mut target = ModelTarget {
                model_id: "shared-model".into(),
                provider_id: None,
                api_dialect: None,
                protocol_endpoint_id: None,
                endpoint_name: None,
            };
            let partial = target_unavailable(&target, "genai", reason.into());
            assert!(
                matches!(&partial, PublicationResolutionError::Invalid(message) if message.contains(reason)),
                "T1/E1: {partial}"
            );
            target.provider_id = Some("vendor".into());
            assert!(
                matches!(target_unavailable(&target, "genai", reason.into()),
                PublicationResolutionError::CandidateUnavailable { reason: retained, .. } if retained == reason),
                "T2/E2"
            );
            target.provider_id = Some(String::new());
            assert!(
                matches!(
                    target_unavailable(&target, "genai", reason.into()),
                    PublicationResolutionError::InvalidBinding { .. }
                ),
                "T3/E3"
            );
        }
    }
}
