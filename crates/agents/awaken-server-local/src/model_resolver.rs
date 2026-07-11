//! The catalog-backed model resolver (ADR-0052 D5): the real adapter behind
//! `awaken_runtime_host::ModelResolver`. It maps the shared provider catalog's
//! offerings to concrete bindings — the first provider-backed offering is the primary
//! binding, the rest are pool candidates — so a `ModelSelection::Auto` config resolves
//! to a concrete, reproducible binding at publish without an operator picking a model.

use awaken_model_catalog::{Offering, ProviderCatalog};
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_host::{ModelResolver, ResolvedModel};

/// Resolves `Auto` against a snapshot of the org-shared provider catalog. Every
/// catalog offering is provider-backed (the catalog has no "scripted" flavor), so the
/// first offering is the first provider-backed model.
pub struct CatalogModelResolver {
    catalog: ProviderCatalog,
}

impl CatalogModelResolver {
    #[must_use]
    pub fn new(catalog: ProviderCatalog) -> Self {
        Self { catalog }
    }
}

/// The native binding for an offering. We mirror the server's proven native binding
/// shape (`default` provider/backend); the concrete endpoint is re-resolved by model
/// id at run time through `resolve_inference`, so `model_ref` is the load-bearing part.
fn binding_of(offering: &Offering) -> ModelBinding {
    ModelBinding::new("default", offering.model_id.clone(), "default")
}

impl ModelResolver for CatalogModelResolver {
    fn resolve_auto(&self) -> Result<ResolvedModel, String> {
        let mut offerings = self.catalog.offerings.iter();
        let primary = offerings.next().ok_or_else(|| {
            "no provider-backed model in the catalog; configure and publish a model first"
                .to_string()
        })?;
        Ok(ResolvedModel {
            primary: binding_of(primary),
            candidates: offerings.map(binding_of).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_model_catalog::{ModelApiCompat, ProtocolEndpointId, ProviderId};

    fn offering(model: &str) -> Offering {
        Offering {
            model_id: model.to_string(),
            provider_id: ProviderId::new("anthropic"),
            protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
            flavor: ModelApiCompat::AnthropicMessages,
            upstream_model: None,
        }
    }

    fn catalog(models: &[&str]) -> ProviderCatalog {
        ProviderCatalog {
            offerings: models.iter().map(|m| offering(m)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn first_offering_is_primary_and_the_rest_are_candidates() {
        let resolver = CatalogModelResolver::new(catalog(&["m-first", "m-second", "m-third"]));
        let resolved = resolver.resolve_auto().unwrap();
        assert_eq!(resolved.primary.model_ref, "m-first");
        let candidate_models: Vec<&str> = resolved
            .candidates
            .iter()
            .map(|c| c.model_ref.as_str())
            .collect();
        assert_eq!(candidate_models, vec!["m-second", "m-third"]);
    }

    #[test]
    fn single_offering_has_no_candidates() {
        let resolver = CatalogModelResolver::new(catalog(&["only"]));
        let resolved = resolver.resolve_auto().unwrap();
        assert_eq!(resolved.primary.model_ref, "only");
        assert!(resolved.candidates.is_empty());
    }

    #[test]
    fn empty_catalog_is_an_error() {
        let resolver = CatalogModelResolver::new(catalog(&[]));
        let err = resolver.resolve_auto().unwrap_err();
        assert!(err.contains("no provider-backed model"));
    }
}
