//! Control-side adapter for WebSearch publication validation.

use awaken_config_service::PluginPublicationResolver;

pub(crate) struct WebSearchPublicationResolver {
    providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
}

impl WebSearchPublicationResolver {
    pub(crate) fn new(providers: awaken_ext_builtin_tools::WebSearchProviderRegistry) -> Self {
        Self { providers }
    }
}

#[async_trait::async_trait]
impl PluginPublicationResolver for WebSearchPublicationResolver {
    fn plugin_id(&self) -> &str {
        awaken_ext_builtin_tools::WEB_SEARCH_PLUGIN_ID
    }

    async fn resolve(
        &self,
        _workspace: &awaken_tenancy::ScopeId,
        config: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        awaken_ext_builtin_tools::WebSearchPlugin::new(self.providers.clone(), None)
            .validate_config(config)
            .map_err(|error| error.to_string())?;
        Ok(config
            .cloned()
            .expect("validated WebSearch config is present"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publication_validation_follows_provider_semantics() {
        // Causes: C1 free provider with complete config; C2 paid provider lacks
        // credential pin. Effects: E1 unchanged frozen config; E2 rejection.
        // R1 C1 -> E1; R2 C2 -> E2. No credential is materialized.
        let resolver = WebSearchPublicationResolver::new(
            awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins(),
        );
        assert_eq!(
            resolver
                .resolve(
                    &awaken_tenancy::ScopeId::from("workspace-a"),
                    Some(&serde_json::json!({ "provider_id": "duckduckgo", "options": {} })),
                )
                .await,
            Ok(serde_json::json!({ "provider_id": "duckduckgo", "options": {} })),
            "R1"
        );
        assert!(
            resolver
                .resolve(
                    &awaken_tenancy::ScopeId::from("workspace-a"),
                    Some(&serde_json::json!({ "provider_id": "brave", "options": {} })),
                )
                .await
                .is_err(),
            "R2"
        );
    }
}
