//! Test composition adapter for the scenario host's live model catalog.

use std::sync::Arc;

use awaken_runtime_contract::resolved::ModelBinding;

pub(crate) struct ScenarioHostModelResolver {
    catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo>,
}

impl ScenarioHostModelResolver {
    pub(crate) fn new(catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo>) -> Self {
        Self { catalog }
    }
}

#[async_trait::async_trait]
impl awaken_runtime_host::ModelPublicationResolver for ScenarioHostModelResolver {
    async fn resolve_models(
        &self,
        _workspace: &awaken_tenancy::ScopeId,
        selection: &awaken_config_store::ModelSelection,
        fallbacks: &[ModelBinding],
    ) -> Result<
        awaken_runtime_host::ResolvedPublicationModels,
        awaken_runtime_host::PublicationResolutionError,
    > {
        let catalog = self.catalog.snapshot().await.map_err(|error| {
            awaken_runtime_host::PublicationResolutionError::CatalogUnavailable(error.to_string())
        })?;
        let (primary, fallbacks) = if let Some(primary) = selection.resolved() {
            (primary.clone(), fallbacks.to_vec())
        } else {
            let mut offerings = catalog.offerings.iter();
            let primary = offerings
                .next()
                .ok_or(awaken_runtime_host::PublicationResolutionError::MissingPrimary)?;
            let binding = |offering: &awaken_model_catalog::Offering| {
                ModelBinding::new(&offering.provider_id.0, &offering.model_id, "genai")
            };
            (binding(primary), offerings.map(binding).collect())
        };
        let context_window = catalog.context_window(&primary.model_ref);
        let max_output_tokens = catalog.max_output_tokens(&primary.model_ref);
        Ok(awaken_runtime_host::ResolvedPublicationModels::host(
            primary,
            fallbacks,
            context_window,
            max_output_tokens,
        ))
    }
}
