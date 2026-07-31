//! Config-plane validation for the built-in WebSearch plugin.

use crate::PluginPublicationResolver;

/// Validates authored WebSearch configuration against the same provider
/// registry used by execution, without materializing credentials.
pub struct WebSearchPublicationResolver {
    providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
}

impl WebSearchPublicationResolver {
    #[must_use]
    pub fn new(providers: awaken_ext_builtin_tools::WebSearchProviderRegistry) -> Self {
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
        // Cause/effect decision table:
        // R1 owned free provider + complete config -> unchanged frozen config;
        // R2 paid provider without an exact credential pin -> reject;
        // R3 resolver identity -> the canonical WebSearch plugin id.
        // Validation never materializes a credential or invokes a provider.
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
            Ok(serde_json::json!({ "provider_id": "duckduckgo", "options": {} }))
        );
        assert!(
            resolver
                .resolve(
                    &awaken_tenancy::ScopeId::from("workspace-a"),
                    Some(&serde_json::json!({ "provider_id": "brave", "options": {} })),
                )
                .await
                .is_err()
        );
        assert_eq!(
            resolver.plugin_id(),
            awaken_ext_builtin_tools::WEB_SEARCH_PLUGIN_ID
        );
    }
}
