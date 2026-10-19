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
    use awaken_executable_agent_contract::capability::PluginConfigurationSource;
    // Expand only the Agent's declared servers through the same source directory.
    // These disposable exact rows are a projection, never a second catalog.
    let mut exact = Vec::new();
    for source in configurations {
        match source {
            PluginConfigurationSource::Exact { .. } => exact.push(source.clone()),
            PluginConfigurationSource::McpServers {
                capability_for,
                validate,
            } => {
                for server in &config.mcp_servers {
                    // A failed pure metadata projection cannot advertise an
                    // exact instance. It does not validate the complete binding:
                    // the original Agent compiler still owns that error/class.
                    if let Ok(capability) = capability_for(&server.name) {
                        exact.push(PluginConfigurationSource::Exact {
                            capability: Box::new(capability),
                            validate: Some(*validate),
                        });
                    }
                }
            }
        }
    }
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
        let sources = exact
            .iter()
            .filter_map(|source| match source {
                PluginConfigurationSource::Exact {
                    capability,
                    validate,
                } if capability.id == *id => Some((capability, *validate)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let [(capability, validate)] = sources.as_slice() else {
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
        if capability.schema_keys.is_empty() && config.plugin_config.contains_key(id) {
            return Err(issue(
                "plugin_configuration_unsupported",
                format!("plugin `{id}` declares no authored configuration section"),
            ));
        }
        if let Some(validate) = validate {
            validate(config.plugin_config.get(id))
                .map_err(|message| issue("plugin_configuration_invalid", message))?;
        } else if owners == 0 && !capability.schema_keys.is_empty() {
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
        PluginConfigurationSource::Exact {
            capability: Box::new(PluginCapability {
                id: id.into(),
                schema_keys: if configured {
                    vec![id.into()]
                } else {
                    Vec::new()
                },
                config_schema: configured.then(|| serde_json::json!({"type":"object"})),
                bound: Default::default(),
            }),
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
    fn mcp_source_case() -> AgentConfig {
        use crate::config_service::mcp_server_binding;
        let policy = serde_json::json!({"notifications":{
            "notifications/phone/input":{"event_id":"/id","delivery":{"kind":"message","text":"/text"}},
            "notifications/phone/interrupt":{"event_id":"/id","delivery":{"kind":"cancel_response","input_method":"notifications/phone/input","input_event_id":"/target"}}
        }});
        AgentConfig {
            plugin_ids: vec!["mcp:phone".into()],
            mcp_servers: vec![mcp_server_binding("phone")],
            plugin_config: [("mcp:phone".into(), policy.clone())].into_iter().collect(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn declared_mcp_instances_keep_exact_ownership_and_one_runtime_parser() {
        // S1 declared ASCII/Unicode + exact selected id + real valid/default
        // parser -> accepted unchanged; S2 undeclared/unknown -> unavailable;
        // S3 duplicate selected/source/name -> ambiguous; S4 empty -> no
        // advertisable instance, never a new structural validation result.
        use crate::config_service::{mcp_configuration_source, mcp_server_binding};
        let workspace = awaken_tenancy::ScopeId::from("workspace");
        let base = mcp_source_case();
        let policy = base.plugin_config["mcp:phone"].clone();
        for (declared, selected, source_count, repeat, expected) in [
            ("phone", "mcp:phone", 1, false, None),
            ("智客/phone", "mcp:智客/phone", 1, false, None),
            (
                "phone",
                "mcp:other",
                1,
                false,
                Some("plugin_configuration_unavailable"),
            ),
            (
                "phone",
                "unknown",
                1,
                false,
                Some("plugin_configuration_unavailable"),
            ),
            (
                "phone",
                "mcp:phone",
                1,
                true,
                Some("plugin_configuration_ambiguous"),
            ),
            (
                "phone",
                "mcp:phone",
                2,
                false,
                Some("plugin_configuration_ambiguous"),
            ),
            (
                "",
                "mcp:",
                1,
                false,
                Some("plugin_configuration_unavailable"),
            ),
        ] {
            let mut config = base.clone();
            config.mcp_servers = vec![mcp_server_binding(declared)];
            config.plugin_ids = vec![selected.into(); if repeat { 2 } else { 1 }];
            config.plugin_config = [(selected.into(), policy.clone())].into_iter().collect();
            let before = config.clone();
            let sources = vec![mcp_configuration_source(); source_count];
            let result = resolve_plugin_configuration(&sources, &[], &workspace, &mut config).await;
            assert_eq!(
                result
                    .as_ref()
                    .err()
                    .and_then(|issue| issue.code.as_deref()),
                expected
            );
            assert_eq!(config, before, "S1-S4 never rewrite source/config/defaults");
        }
        let mut duplicate = base.clone();
        duplicate.mcp_servers.push(mcp_server_binding("phone"));
        assert_eq!(
            resolve_plugin_configuration(
                &[mcp_configuration_source()],
                &[],
                &workspace,
                &mut duplicate
            )
            .await
            .unwrap_err()
            .code
            .as_deref(),
            Some("plugin_configuration_ambiguous")
        );
        let mut no_declaration = base.clone();
        no_declaration.mcp_servers.clear();
        assert_eq!(
            resolve_plugin_configuration(
                &[mcp_configuration_source()],
                &[],
                &workspace,
                &mut no_declaration
            )
            .await
            .unwrap_err()
            .code
            .as_deref(),
            Some("plugin_configuration_unavailable")
        );
        let mut missing = base.clone();
        missing.plugin_config.clear();
        let before = missing.clone();
        resolve_plugin_configuration(&[mcp_configuration_source()], &[], &workspace, &mut missing)
            .await
            .unwrap();
        assert_eq!(missing, before, "S1 absent config stays absent");
    }

    #[tokio::test]
    async fn mcp_parser_refusals_and_inactive_sections_keep_their_original_owner() {
        // S5 bad pointer/unknown/cancel target absent -> real parser refusal;
        // S6 inactive malformed residue stays; S7 other validators still run.
        // These are the original S5-S7 inputs/assertions, without a copied DSL.
        use crate::config_service::mcp_configuration_source;
        let workspace = awaken_tenancy::ScopeId::from("workspace");
        let base = mcp_source_case();
        let policy = base.plugin_config["mcp:phone"].clone();
        let mut bad_pointer = policy.clone();
        bad_pointer["notifications"]["notifications/phone/input"]["event_id"] = "id".into();
        let mut missing_target = policy.clone();
        missing_target["notifications"]
            .as_object_mut()
            .unwrap()
            .remove("notifications/phone/input");
        for value in [
            serde_json::Value::Null,
            serde_json::json!({"unknown":true}),
            bad_pointer,
            missing_target,
        ] {
            let mut config = base.clone();
            config.plugin_config.insert("mcp:phone".into(), value);
            let before = config.clone();
            let issue = resolve_plugin_configuration(
                &[mcp_configuration_source()],
                &[],
                &workspace,
                &mut config,
            )
            .await
            .unwrap_err();
            assert_eq!(issue.code.as_deref(), Some("plugin_configuration_invalid"));
            assert_eq!(issue.path, "plugin_config.mcp:phone");
            assert_eq!(config, before, "S5 real parser refusal leaves input intact");
        }
        let sources = [
            mcp_configuration_source(),
            configuration("owned", Some(pure_configuration), true),
        ];
        let mut config = base;
        config.plugin_ids = vec!["owned".into()];
        config
            .plugin_config
            .insert("mcp:phone".into(), serde_json::json!({"unknown":true}));
        config
            .plugin_config
            .insert("owned".into(), serde_json::json!({"valid":true}));
        let before = config.clone();
        resolve_plugin_configuration(&sources, &[], &workspace, &mut config)
            .await
            .unwrap();
        assert_eq!(config, before, "S6 inactive malformed section is retained");
        config
            .plugin_config
            .insert("owned".into(), serde_json::json!({"valid":false}));
        assert_eq!(
            resolve_plugin_configuration(&sources, &[], &workspace, &mut config)
                .await
                .unwrap_err()
                .path,
            "plugin_config.owned",
            "S7 unrelated validators are not bypassed"
        );
    }

    #[tokio::test]
    async fn mcp_metadata_failures_preserve_exact_overlap_and_first_refusal_order() {
        // S3 Exact/family overlap remains ambiguous, failed projection cannot
        // hide a successful Exact. S8 unrelated invalid binding metadata cannot
        // preempt active missing/duplicate/validator errors. Full binding
        // legality remains at the existing complete Agent compiler (S9).
        use crate::config_service::{mcp_configuration_source, mcp_server_binding};
        let workspace = awaken_tenancy::ScopeId::from("workspace");
        let base = mcp_source_case();
        let PluginConfigurationSource::McpServers {
            capability_for,
            validate,
        } = mcp_configuration_source()
        else {
            unreachable!("fixture is dynamic")
        };
        let exact_phone = PluginConfigurationSource::Exact {
            capability: Box::new(capability_for("phone").unwrap()),
            validate: Some(validate),
        };
        let mut overlap = base.clone();
        assert_eq!(
            resolve_plugin_configuration(
                &[mcp_configuration_source(), exact_phone.clone()],
                &[],
                &workspace,
                &mut overlap
            )
            .await
            .unwrap_err()
            .code
            .as_deref(),
            Some("plugin_configuration_ambiguous"),
            "S3 Exact/family overlap stays ambiguous"
        );
        let mut projected = base.clone();
        resolve_plugin_configuration(
            &[
                PluginConfigurationSource::McpServers {
                    capability_for: |_| Err("not advertisable".into()),
                    validate,
                },
                exact_phone,
            ],
            &[],
            &workspace,
            &mut projected,
        )
        .await
        .unwrap();
        assert_eq!(
            projected, base,
            "a failed metadata projection cannot hide a successful Exact"
        );
        // S8 metadata from an unrelated invalid binding cannot preempt the
        // selected Exact's original missing/duplicate/validator decisions, nor
        // invent a new structural error before credentials/resolvers/compiler.
        for (ids, valid, code) in [
            (
                vec!["missing"],
                true,
                Some("plugin_configuration_unavailable"),
            ),
            (
                vec!["owned", "owned"],
                true,
                Some("plugin_configuration_ambiguous"),
            ),
            (vec!["owned"], false, Some("plugin_configuration_invalid")),
            (vec!["owned"], true, None),
        ] {
            let mut config = base.clone();
            config.plugin_ids = ids.into_iter().map(str::to_owned).collect();
            config.mcp_servers = vec![mcp_server_binding("")];
            config
                .plugin_config
                .insert("owned".into(), serde_json::json!({"valid":valid}));
            let before = config.clone();
            let result = resolve_plugin_configuration(
                &[
                    mcp_configuration_source(),
                    configuration("owned", Some(pure_configuration), true),
                ],
                &[],
                &workspace,
                &mut config,
            )
            .await;
            assert_eq!(
                result
                    .as_ref()
                    .err()
                    .and_then(|issue| issue.code.as_deref()),
                code
            );
            assert_eq!(
                config, before,
                "S8 original Exact first-refusal is unchanged"
            );
        }
    }
}
