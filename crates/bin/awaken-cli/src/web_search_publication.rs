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
        toolsets: &[awaken_runtime_contract::agent_bindings::ToolsetPolicy],
        config: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        awaken_ext_builtin_tools::WebSearchPlugin::new(self.providers.clone(), None)
            .with_execution_configuration(
                awaken_ext_builtin_tools::web_search_execution_configuration(toolsets)?,
            )
            .validate_config(config)
            .map_err(|error| error.to_string())?;
        Ok(config
            .cloned()
            .expect("validated WebSearch config is present"))
    }
}

pub(crate) struct WebFetchPublicationResolver {
    providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
}

impl WebFetchPublicationResolver {
    pub(crate) fn new(providers: awaken_ext_builtin_tools::WebSearchProviderRegistry) -> Self {
        Self { providers }
    }
}

#[async_trait::async_trait]
impl PluginPublicationResolver for WebFetchPublicationResolver {
    fn plugin_id(&self) -> &str {
        awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID
    }

    async fn resolve(
        &self,
        _workspace: &awaken_tenancy::ScopeId,
        toolsets: &[awaken_runtime_contract::agent_bindings::ToolsetPolicy],
        config: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        awaken_ext_builtin_tools::WebFetchPlugin::new(self.providers.clone(), None)
            .with_execution_configuration(
                awaken_ext_builtin_tools::web_fetch_execution_configuration(toolsets)?,
            )
            .validate_config(config)
            .map_err(|error| error.to_string())?;
        Ok(config
            .cloned()
            .unwrap_or_else(awaken_ext_builtin_tools::WebFetchPlugin::default_config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_policy(
        name: &str,
        configuration: serde_json::Value,
    ) -> awaken_runtime_contract::agent_bindings::ToolsetPolicy {
        awaken_runtime_contract::agent_bindings::ToolsetPolicy {
            source: awaken_runtime_contract::agent_bindings::ToolsetSource::Agent,
            default: awaken_runtime_contract::agent_bindings::ToolExecutionPolicy::default(),
            overrides: vec![awaken_runtime_contract::agent_bindings::ToolPolicyOverride::with_optional_configuration(
                name,
                awaken_runtime_contract::agent_bindings::ToolExecutionPolicy::default(),
                Some(configuration),
            )],
        }
    }

    #[tokio::test]
    async fn publication_validation_follows_provider_semantics() {
        // Causes: C1 host provider with complete config; C2 paid host provider
        // lacks a credential pin; C3 provider-server route plus a restrictive
        // Agent execution policy. Effects: E1 unchanged frozen config; E2/E3
        // rejection before publication. R1=C1=>E1; R2=C2=>E2; R3/R4=C3 for
        // Search/Fetch=>E3. The extension's one plugin validator owns both
        // publication and runtime semantics; no credential is materialized.
        //
        // | Rule | realization | policy | Effect |
        // | R1 | host/free | none | frozen unchanged |
        // | R2 | host/paid | missing credential | reject |
        // | R3 | provider Search | user location | reject |
        // | R4 | provider Fetch | content cap | reject |
        let resolver = WebSearchPublicationResolver::new(
            awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins(),
        );
        assert_eq!(
            resolver
                .resolve(
                    &awaken_tenancy::ScopeId::from("workspace-a"),
                    &[],
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
                    &[],
                    Some(&serde_json::json!({ "provider_id": "brave", "options": {} })),
                )
                .await
                .is_err(),
            "R2"
        );
        assert!(
            resolver
                .resolve(
                    &awaken_tenancy::ScopeId::from("workspace-a"),
                    &[agent_policy(
                        "web_search",
                        serde_json::json!({
                            "type": "web_search",
                            "user_location": {"country": "US"}
                        }),
                    )],
                    Some(&serde_json::json!({ "provider_id": "openrouter", "options": {} })),
                )
                .await
                .is_err(),
            "R3"
        );
        let fetch = WebFetchPublicationResolver::new(
            awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins(),
        );
        assert!(
            fetch
                .resolve(
                    &awaken_tenancy::ScopeId::from("workspace-a"),
                    &[agent_policy(
                        "web_fetch",
                        serde_json::json!({"type": "web_fetch", "max_content_tokens": 512}),
                    )],
                    Some(&serde_json::json!({ "provider_id": "openrouter", "options": {} })),
                )
                .await
                .is_err(),
            "R4"
        );
    }
}
