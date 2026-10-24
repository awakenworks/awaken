// @formal surface obligation=config.inference_access_resolved_once_at_publication
// @formal reason: Auto selects only offerings whose credential access resolves through the existing candidate path at publication; it adds no other credential or authorization decision.
//! `Auto` publication: choose only among offerings that can execute now.

use awaken_config_service::{PublicationResolutionError, ResolvedPublicationModels};
use awaken_model_catalog::{OfferingStatus, ProviderCatalog};
use awaken_tenancy::ScopeId;

use super::CatalogModelPublicationResolver;

/// `Auto` names no model, so it may only choose among offerings that can
/// execute now: the first usable active offering is primary and the other
/// usable ones follow as fallbacks. An offering without an active, compatible
/// credential (for example an archived key) is skipped rather than failing the
/// publication; explicit selections keep their strict resolution.
pub(super) async fn resolve_auto_models(
    resolver: &CatalogModelPublicationResolver,
    catalog: &ProviderCatalog,
    workspace: &ScopeId,
) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
    let active = catalog
        .offerings
        .iter()
        .filter(|offering| offering.status == OfferingStatus::Active)
        .collect::<Vec<_>>();
    if active.is_empty() {
        return Err(PublicationResolutionError::MissingPrimary);
    }
    let sources = resolver
        .credentials
        .list(workspace.as_str())
        .await
        .map_err(|error| {
            PublicationResolutionError::CredentialInventoryUnavailable(error.to_string())
        })?;
    let mut usable = Vec::with_capacity(active.len());
    let mut first_unavailable = None;
    for offering in active {
        let binding = CatalogModelPublicationResolver::binding_of(offering)?;
        match resolver
            .candidate(catalog, &sources, workspace, binding.clone(), None, None)
            .await
        {
            Ok(candidate) => usable.push((binding, candidate)),
            Err(error @ PublicationResolutionError::CandidateUnavailable { .. }) => {
                first_unavailable.get_or_insert(error);
            }
            Err(error) => return Err(error),
        }
    }
    let mut usable = usable.into_iter();
    let (primary_binding, primary) = usable
        .next()
        .ok_or_else(|| first_unavailable.unwrap_or(PublicationResolutionError::MissingPrimary))?;
    Ok(ResolvedPublicationModels {
        primary,
        candidates: usable.map(|(_, candidate)| candidate).collect(),
        context_window: catalog.context_window(primary_binding.identity().1),
        max_output_tokens: catalog.max_output_tokens(primary_binding.identity().1),
    })
}
