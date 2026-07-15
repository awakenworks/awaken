//! The catalog-backed model resolver (ADR-0052 D5): the real adapter behind
//! `awaken_runtime_host::ModelResolver`. It maps the shared provider catalog's
//! offerings to concrete bindings — the first provider-backed offering is the primary
//! binding, the rest are pool candidates — so a `ModelSelection::Auto` config resolves
//! to a concrete, reproducible binding at publish without an operator picking a model.

use std::sync::Arc;

use awaken_model_catalog::repo::CatalogRepo;
use awaken_model_catalog::{Offering, ProviderCatalog};
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_host::{ModelResolver, ResolvedModel};

/// The catalog the resolver reads. Either a frozen `ProviderCatalog` snapshot (the
/// original constructor, still used by tests) or a live `CatalogRepo` re-read on every
/// resolve — so an offering an operator adds AFTER startup is visible without a restart.
enum CatalogSource {
    Static(ProviderCatalog),
    Live(Arc<dyn CatalogRepo>),
}

/// Resolves `Auto` against a snapshot of the org-shared provider catalog. Every
/// catalog offering is provider-backed (the catalog has no "scripted" dialect), so the
/// first offering is the first provider-backed model.
pub struct CatalogModelResolver {
    source: CatalogSource,
}

impl CatalogModelResolver {
    /// Resolve against a frozen catalog snapshot (deterministic; used by tests).
    #[must_use]
    pub fn new(catalog: ProviderCatalog) -> Self {
        Self {
            source: CatalogSource::Static(catalog),
        }
    }

    /// Resolve against the LIVE catalog repo: every resolve re-reads a fresh snapshot,
    /// so a model published after startup is picked up without re-seeding the resolver.
    #[must_use]
    pub fn from_repo(repo: Arc<dyn CatalogRepo>) -> Self {
        Self {
            source: CatalogSource::Live(repo),
        }
    }

    /// A fresh catalog snapshot for this resolve. The `ModelResolver` trait methods are
    /// SYNC but resolve happens inside async publish handlers on a multi-threaded tokio
    /// runtime, so we bridge the async `CatalogRepo::snapshot()` via `block_in_place` +
    /// `Handle::block_on` (valid only on a multi-thread runtime). The static path just
    /// clones its frozen snapshot.
    fn snapshot(&self) -> Result<ProviderCatalog, String> {
        match &self.source {
            CatalogSource::Static(catalog) => Ok(catalog.clone()),
            CatalogSource::Live(repo) => tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(repo.snapshot())
            })
            .map_err(|e| e.to_string()),
        }
    }
}

/// The native binding for an offering. We mirror the server's proven native binding
/// shape (`default` provider/backend); the concrete endpoint is re-resolved by model
/// id at run time through `resolve_inference`, so `model_ref` is the load-bearing part.
fn binding_of(offering: &Offering) -> ModelBinding {
    ModelBinding::new("default", offering.model_id.clone(), "default")
}

impl ModelResolver for CatalogModelResolver {
    /// The model's published context window from the catalog's `ModelAttributes` (E: the
    /// source an agent's compaction window and the ACP auto-compact window derive from).
    fn context_window(&self, model_id: &str) -> Option<u32> {
        self.snapshot().ok()?.context_window(model_id)
    }

    fn resolve_auto(&self) -> Result<ResolvedModel, String> {
        let catalog = self.snapshot()?;
        let mut offerings = catalog.offerings.iter();
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
    use awaken_model_catalog::{ApiDialect, ProtocolEndpointId, ProviderId};

    fn offering(model: &str) -> Offering {
        Offering {
            model_id: model.to_string(),
            provider_id: ProviderId::new("anthropic"),
            protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
            dialect: ApiDialect::AnthropicMessages,
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
