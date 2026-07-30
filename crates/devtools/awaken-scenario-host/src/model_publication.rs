//! Test composition adapter for the scenario host's live model catalog.

use std::sync::Arc;

use awaken_runtime_contract::resolved::ModelBinding;

pub(crate) async fn scenario_model_catalog(
    model_ref: &str,
) -> Arc<dyn awaken_model_catalog::repo::CatalogRepo> {
    use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};

    let catalog: Arc<dyn CatalogRepo> = Arc::new(InMemoryCatalogRepo::new());
    catalog
        .put_provider(awaken_model_catalog::Provider {
            id: awaken_model_catalog::ProviderId::new("default"),
            slug: "default".into(),
            display_name: "Default".into(),
            version: 1,
        })
        .await
        .expect("put scenario provider");
    catalog
        .put_endpoint(awaken_model_catalog::ProtocolEndpoint {
            id: awaken_model_catalog::ProtocolEndpointId::new("ep"),
            provider_id: awaken_model_catalog::ProviderId::new("default"),
            dialect: awaken_model_catalog::ApiDialect::AnthropicMessages,
            base_url: None,
            timeout_secs: 30,
            display_name: "ep".into(),
            version: 1,
        })
        .await
        .expect("put scenario endpoint");
    catalog
        .put_offering(awaken_model_catalog::Offering {
            model_id: model_ref.into(),
            provider_id: awaken_model_catalog::ProviderId::new("default"),
            protocol_endpoint_id: awaken_model_catalog::ProtocolEndpointId::new("ep"),
            dialect: awaken_model_catalog::ApiDialect::AnthropicMessages,
            upstream_model: None,
            source: Default::default(),
            status: Default::default(),
            last_seen_at_unix_ms: None,
        })
        .await
        .expect("put scenario offering");
    catalog
}

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

#[cfg(test)]
mod tests {
    use awaken_runtime_host::ModelPublicationResolver as _;

    use super::*;

    #[tokio::test]
    async fn one_catalog_drives_explicit_and_auto_scenario_publication() {
        // Cause/effect decision table: M1 explicit model selection -> preserve
        // the authored binding; M2 Auto with one offering -> select that exact
        // offering; M3 Auto with an empty catalog -> fail closed as missing
        // primary. Both scenario compositions reuse this one catalog builder.
        let workspace = awaken_tenancy::ScopeId::from("workspace-a");
        let resolver = ScenarioHostModelResolver::new(scenario_model_catalog("echo").await);
        let explicit = ModelBinding::new("default", "echo", "default");
        let resolved = resolver
            .resolve_models(
                &workspace,
                &awaken_config_store::ModelSelection::Pinned(explicit.clone()),
                &[],
            )
            .await
            .expect("M1 explicit model");
        assert_eq!(resolved.primary.binding, explicit, "M1");

        let resolved = resolver
            .resolve_models(&workspace, &awaken_config_store::ModelSelection::Auto, &[])
            .await
            .expect("M2 Auto model");
        assert_eq!(resolved.primary.binding.model_ref, "echo", "M2");

        let empty = ScenarioHostModelResolver::new(Arc::new(
            awaken_model_catalog::repo::InMemoryCatalogRepo::new(),
        ));
        assert!(
            empty
                .resolve_models(&workspace, &awaken_config_store::ModelSelection::Auto, &[])
                .await
                .is_err(),
            "M3"
        );
    }
}
