// @formal surface requirement=runtime.plugin_activation.capability_bound
// @formal reason: This source implements the repository-controlled plugin product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
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
    configurations: &[awaken_executable_agent_contract::capability::PluginConfigurationSource],
    resolvers: &[std::sync::Arc<dyn PluginPublicationResolver>],
    workspace: &awaken_tenancy::ScopeId,
    config: &mut AgentConfig,
) -> Result<(), crate::publication::ValidationIssue> {
    // Reject missing/ambiguous owners before any Workspace resolver is entered.
    // Inactive authoring sections remain untouched and do not acquire authority.
    let mut selected = std::collections::BTreeSet::new();
    for id in &config.plugin_ids {
        let issue = |code: &str, message: String| crate::publication::ValidationIssue {
            code: Some(code.into()),
            path: format!("plugin_config.{id}"),
            message,
        };
        if !selected.insert(id) {
            return Err(issue(
                "plugin_configuration_ambiguous",
                format!("plugin `{id}` is selected more than once"),
            ));
        }
        let sources = configurations
            .iter()
            .filter(|source| source.capability.id == *id)
            .collect::<Vec<_>>();
        let [source] = sources.as_slice() else {
            return Err(issue(
                if sources.is_empty() {
                    "plugin_configuration_unavailable"
                } else {
                    "plugin_configuration_ambiguous"
                },
                format!("plugin `{id}` requires exactly one advertised configuration source"),
            ));
        };
        let owners = resolvers
            .iter()
            .filter(|resolver| resolver.plugin_id() == id)
            .count();
        if owners > 1 {
            return Err(issue(
                "plugin_configuration_ambiguous",
                format!("plugin `{id}` has more than one publication resolver"),
            ));
        }
        if source.capability.schema_keys.is_empty() && config.plugin_config.contains_key(id) {
            return Err(issue(
                "plugin_configuration_unsupported",
                format!("plugin `{id}` declares no authored configuration section"),
            ));
        }
        if let Some(validate) = source.validate {
            validate(config.plugin_config.get(id))
                .map_err(|message| issue("plugin_configuration_invalid", message))?;
        } else if owners == 0 && !source.capability.schema_keys.is_empty() {
            return Err(issue(
                "plugin_configuration_unavailable",
                format!("plugin `{id}` has no configuration validator or publication resolver"),
            ));
        }
    }
    for plugin_id in config.plugin_ids.clone() {
        if let Some(resolver) = resolvers
            .iter()
            .find(|resolver| resolver.plugin_id() == plugin_id)
        {
            let resolved = resolver
                .resolve(
                    workspace,
                    &config.toolsets,
                    config.plugin_config.get(&plugin_id),
                )
                .await
                .map_err(|message| crate::publication::ValidationIssue {
                    code: Some("plugin_configuration_invalid".into()),
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
    use awaken_executable_agent_contract::capability::{
        PluginCapability, PluginConfigurationSource, PluginConfigurationValidator,
    };

    fn configuration(
        id: &str,
        validate: Option<PluginConfigurationValidator>,
        configured: bool,
    ) -> PluginConfigurationSource {
        PluginConfigurationSource {
            capability: PluginCapability {
                id: id.into(),
                schema_keys: if configured {
                    vec![id.into()]
                } else {
                    Vec::new()
                },
                config_schema: configured.then(|| serde_json::json!({"type":"object"})),
                bound: Default::default(),
            },
            validate,
        }
    }

    fn pure_configuration(value: Option<&serde_json::Value>) -> Result<(), String> {
        if value.is_some_and(|value| {
            value.get("valid").and_then(serde_json::Value::as_bool) == Some(true)
        }) {
            Ok(())
        } else {
            Err("pure owner refused configuration".into())
        }
    }

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
        // | R3a | yes, pure validator | no Workspace resolver | valid | any | E3 |
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
            &[
                configuration("owned", None, true),
                configuration("external", Some(|_| Ok(())), true),
            ],
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
            &[
                configuration("owned", None, true),
                configuration("external", Some(|_| Ok(())), true),
            ],
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

    #[tokio::test]
    async fn every_active_section_requires_one_declared_validation_owner() {
        // DT-PLUGIN-CONFIG: C1 selected id unique, C2 exactly one source,
        // C3 one pure validator or one Workspace resolver for a configurable
        // section, C4 accepted input. All true -> unchanged authored value;
        // negating any prerequisite -> field-routed refusal before resolution.
        // An inactive malformed section remains authoring residue. A declared
        // config-free plugin needs no validator but cannot consume a section.
        let workspace = awaken_tenancy::ScopeId::from("workspace");
        let base = AgentConfig {
            plugin_ids: vec!["p".into()],
            plugin_config: [
                ("p".into(), serde_json::json!({"valid":true})),
                ("inactive".into(), serde_json::json!("retained input")),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        for (sources, ids, valid, accepted) in [
            (
                vec![configuration("p", Some(pure_configuration), true)],
                vec!["p"],
                true,
                true,
            ),
            (vec![], vec!["p"], true, false),
            (
                vec![configuration("p", Some(pure_configuration), true); 2],
                vec!["p"],
                true,
                false,
            ),
            (
                vec![configuration("p", Some(pure_configuration), true)],
                vec!["p", "p"],
                true,
                false,
            ),
            (vec![configuration("p", None, true)], vec!["p"], true, false),
            (
                vec![configuration("p", None, false)],
                vec!["p"],
                true,
                false,
            ),
            (
                vec![configuration("p", Some(pure_configuration), false)],
                vec!["p"],
                true,
                false,
            ),
            (
                vec![configuration("p", Some(pure_configuration), true)],
                vec!["p"],
                false,
                false,
            ),
        ] {
            let mut config = base.clone();
            config.plugin_ids = ids.into_iter().map(str::to_owned).collect();
            config
                .plugin_config
                .insert("p".into(), serde_json::json!({"valid":valid}));
            let before = config.clone();
            let result = resolve_plugin_configuration(&sources, &[], &workspace, &mut config).await;
            assert_eq!(result.is_ok(), accepted);
            if let Err(issue) = result {
                assert_eq!(issue.path, "plugin_config.p");
            }
            assert_eq!(
                config, before,
                "pure admission never rewrites authored/default input"
            );
        }
        let mut config_free = base;
        config_free.plugin_config.remove("p");
        resolve_plugin_configuration(
            &[configuration("p", None, false)],
            &[],
            &workspace,
            &mut config_free,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn invalid_later_owner_prevents_every_workspace_resolution() {
        // R5 an earlier valid owner + later missing owner -> zero resolver
        // calls and zero partial config mutation. R6 duplicate Workspace
        // resolvers -> ambiguous ownership, never first-match selection.
        let catalog = Catalog::default();
        let seen = catalog.seen_toolsets.clone();
        let resolver: std::sync::Arc<dyn PluginPublicationResolver> = std::sync::Arc::new(catalog);
        let mut config = AgentConfig {
            plugin_ids: vec!["owned".into(), "missing".into()],
            plugin_config: [("owned".into(), serde_json::json!({"valid":true}))]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let before = config.clone();
        let workspace = awaken_tenancy::ScopeId::from("workspace");
        assert!(
            resolve_plugin_configuration(
                &[configuration("owned", None, true)],
                std::slice::from_ref(&resolver),
                &workspace,
                &mut config
            )
            .await
            .is_err()
        );
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(config, before);
        config.plugin_ids.pop();
        assert!(
            resolve_plugin_configuration(
                &[configuration("owned", None, true)],
                &[resolver.clone(), resolver],
                &workspace,
                &mut config
            )
            .await
            .is_err()
        );
        assert!(seen.lock().unwrap().is_empty());
    }
}
