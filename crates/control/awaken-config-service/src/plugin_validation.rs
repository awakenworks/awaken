//! Publication-side port for extension-owned configuration resolution.
//!
//! JSON Schema drives discovery and forms; the extension remains the semantic
//! authority. A composition supplies one resolver catalog so ConfigService
//! never imports concrete plugins or duplicates validation and publication
//! transformation rules.

use awaken_agent_config::AgentConfig;

#[async_trait::async_trait]
pub trait PluginPublicationResolver: Send + Sync {
    fn plugin_id(&self) -> &str;
    /// Validate authored configuration and return its canonical, secret-free
    /// publication form. Implementations may resolve deployment-owned immutable
    /// references, but must not materialize credentials or perform runtime I/O.
    async fn resolve(
        &self,
        workspace: &awaken_tenancy::ScopeId,
        config: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, String>;
}

pub(crate) async fn resolve_plugin_configuration(
    resolvers: &[std::sync::Arc<dyn PluginPublicationResolver>],
    workspace: &awaken_tenancy::ScopeId,
    config: &mut AgentConfig,
) -> Result<(), crate::publication::ValidationIssue> {
    for plugin_id in config.plugin_ids.clone() {
        let mut owned = resolvers
            .iter()
            .filter(|resolver| resolver.plugin_id() == plugin_id);
        if let Some(resolver) = owned.next() {
            if owned.next().is_some() {
                return Err(crate::publication::ValidationIssue {
                    path: format!("plugin_config.{plugin_id}"),
                    message: format!("plugin `{plugin_id}` has more than one publication resolver"),
                });
            }
            let resolved = resolver
                .resolve(workspace, config.plugin_config.get(&plugin_id))
                .await
                .map_err(|message| crate::publication::ValidationIssue {
                    path: format!("plugin_config.{plugin_id}"),
                    message,
                })?;
            config.plugin_config.insert(plugin_id, resolved);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Catalog;

    #[async_trait::async_trait]
    impl PluginPublicationResolver for Catalog {
        fn plugin_id(&self) -> &str {
            "owned"
        }

        async fn resolve(
            &self,
            workspace: &awaken_tenancy::ScopeId,
            config: Option<&serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            let authored = config.ok_or_else(|| "owned config is invalid".to_string())?;
            if authored.get("valid").and_then(serde_json::Value::as_bool) != Some(true) {
                return Err("owned config is invalid".to_string());
            }
            Ok(serde_json::json!({
                "valid": authored["valid"],
                "workspace": workspace.as_str(),
            }))
        }
    }

    #[tokio::test]
    async fn active_owned_plugin_uses_one_resolution_semantics() {
        // Cause/effect graph and decision table:
        // R1 active+owned+valid -> canonical Workspace-bound value;
        // R2 active+owned+bad -> exact plugin path error;
        // R3 inactive or active+unowned -> unchanged. This proves validation and
        // publication transformation share one resolver rather than two catalogs.
        let resolvers: Vec<std::sync::Arc<dyn PluginPublicationResolver>> =
            vec![std::sync::Arc::new(Catalog)];
        let mut config = AgentConfig {
            plugin_ids: vec!["owned".into(), "external".into()],
            plugin_config: [
                ("owned".into(), serde_json::json!({ "valid": true })),
                ("external".into(), serde_json::json!({ "anything": true })),
                ("inactive".into(), serde_json::json!({ "valid": false })),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        resolve_plugin_configuration(
            &resolvers,
            &awaken_tenancy::ScopeId::from("workspace-a"),
            &mut config,
        )
        .await
        .unwrap();
        assert_eq!(
            config.plugin_config["owned"],
            serde_json::json!({ "valid": true, "workspace": "workspace-a" })
        );
        assert_eq!(
            config.plugin_config["external"],
            serde_json::json!({ "anything": true })
        );
        assert_eq!(
            config.plugin_config["inactive"],
            serde_json::json!({ "valid": false })
        );
        config
            .plugin_config
            .insert("owned".into(), serde_json::json!({ "valid": false }));
        assert_eq!(
            resolve_plugin_configuration(
                &resolvers,
                &awaken_tenancy::ScopeId::from("workspace-a"),
                &mut config,
            )
            .await
            .unwrap_err()
            .path,
            "plugin_config.owned"
        );
    }
}
