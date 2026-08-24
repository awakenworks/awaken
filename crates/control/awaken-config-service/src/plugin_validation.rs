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
        toolsets: &[awaken_runtime_contract::agent_bindings::ToolsetPolicy],
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
                .resolve(
                    workspace,
                    &config.toolsets,
                    config.plugin_config.get(&plugin_id),
                )
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

    #[derive(Default)]
    struct Catalog {
        seen_toolsets: std::sync::Arc<
            std::sync::Mutex<Vec<Vec<awaken_runtime_contract::agent_bindings::ToolsetPolicy>>>,
        >,
    }

    #[async_trait::async_trait]
    impl PluginPublicationResolver for Catalog {
        fn plugin_id(&self) -> &str {
            "owned"
        }

        async fn resolve(
            &self,
            workspace: &awaken_tenancy::ScopeId,
            toolsets: &[awaken_runtime_contract::agent_bindings::ToolsetPolicy],
            config: Option<&serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            self.seen_toolsets.lock().unwrap().push(toolsets.to_vec());
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
        // Cause/effect graph: C1 plugin active; C2 exactly one resolver owns it;
        // C3 authored config is valid; C4 the authored toolset policy is exact
        // and non-empty. Effects: E1 canonical Workspace-bound value and the
        // exact C4 slice reaches the resolver, E2 exact plugin-path error, E3
        // config remains unchanged.
        //
        // | Rule | active | owner | valid | toolsets | Effect |
        // | R1 | yes | one | yes | exact non-empty | E1 |
        // | R2 | yes | one | no | exact non-empty | E2 |
        // | R3a | yes | none | any | any | E3 |
        // | R3b | no | any | any | any | E3 |
        //
        // The production resolver call is the single publication semantic
        // path; this probe captures its input instead of recreating resolution.
        use awaken_runtime_contract::agent_bindings::{
            ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
            ToolsetSource,
        };

        let exact_toolsets = vec![ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy {
                enabled: true,
                permission: ToolPermissionRequirement::AlwaysAsk,
            },
            overrides: vec![ToolPolicyOverride::new(
                "web_fetch",
                ToolExecutionPolicy::default(),
            )],
        }];
        let catalog = Catalog::default();
        let seen_toolsets = catalog.seen_toolsets.clone();
        let resolvers: Vec<std::sync::Arc<dyn PluginPublicationResolver>> =
            vec![std::sync::Arc::new(catalog)];
        let mut config = AgentConfig {
            plugin_ids: vec!["owned".into(), "external".into()],
            plugin_config: [
                ("owned".into(), serde_json::json!({ "valid": true })),
                ("external".into(), serde_json::json!({ "anything": true })),
                ("inactive".into(), serde_json::json!({ "valid": false })),
            ]
            .into_iter()
            .collect(),
            toolsets: exact_toolsets.clone(),
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
        assert_eq!(
            seen_toolsets.lock().unwrap().as_slice(),
            std::slice::from_ref(&exact_toolsets),
            "R1 forwards the exact authored non-empty toolset slice"
        );
        config
            .plugin_config
            .insert("owned".into(), serde_json::json!({ "valid": false }));
        let issue = resolve_plugin_configuration(
            &resolvers,
            &awaken_tenancy::ScopeId::from("workspace-a"),
            &mut config,
        )
        .await
        .unwrap_err();
        assert_eq!(issue.path, "plugin_config.owned");
        assert_eq!(
            seen_toolsets.lock().unwrap().as_slice(),
            &[exact_toolsets.clone(), exact_toolsets],
            "R2 forwards the same exact authored toolsets before semantic rejection"
        );
    }
}
