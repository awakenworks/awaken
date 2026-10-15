#[cfg(test)]
fn validate_agent_tools(tools: &[AgentTool]) -> Result<(), String> {
    compile_agent_tools(tools)
        .map(|_| ())
        .map_err(|error| error.message)
}
#[cfg(test)]
fn toolset_policies(tools: &[AgentTool]) -> Vec<ToolsetPolicy> {
    compile_agent_tools(tools)
        .expect("valid wire fixture")
        .toolsets
}
#[cfg(test)]
mod tests {
    use super::*;
    fn project_valid_toolsets(policies: &[ToolsetPolicy]) -> Vec<AgentTool> {
        resolved_toolsets(policies).expect("valid frozen fixture")
    }
    use awaken_session_configuration_contract::{
        AGENT_TOOLSET_TOOL_IDS, is_agent_toolset_member, is_controlled_modification_member,
    };
    #[test]
    fn official_agent_toolset_has_one_exact_eight_member_source() {
        // Cause/effect graph: the official versioned toolset declaration is the
        // closed membership source; policy normalization must materialize every
        // member once and must not create a second WebSearch representation.
        //
        // Decision table:
        // | Rule | authored overrides      | effects                         |
        // | T1   | none                    | exact eight unique defaults     |
        // | T2   | web_search disabled     | same member, disabled once      |
        // | T3   | unknown name            | admission rejects; no policy   |
        // Constraints/invariants: the versioned declaration is the sole member
        // authority and each official name is materialized exactly once.
        assert_eq!(
            AGENT_TOOLSET_TOOL_IDS,
            [
                "bash",
                "read",
                "write",
                "edit",
                "glob",
                "grep",
                "web_fetch",
                "web_search",
            ],
            "T1"
        );
        // Controlled-modification cause/effect table: bash/write/edit -> ask
        // authoring metadata; every other official member -> ordinary metadata;
        // unknown ids -> false. All adapters project this one closed enum fact.
        assert_eq!(
            agent_toolset_members()
                .filter(|name| is_controlled_modification_member(name))
                .collect::<Vec<_>>(),
            vec!["bash", "write", "edit"],
            "T1 controlled projection"
        );
        assert!(!is_controlled_modification_member("parallel_web_search"));
        let policies = toolset_policies(&[AgentTool::AgentToolset20260401 {
            configs: vec![AgentToolConfig {
                name: "web_search".into(),
                kind: None,
                enabled: Some(false),
                permission_policy: None,
                allowed_domains: None,
                blocked_domains: None,
                max_content_tokens: None,
                url_sources: None,
                user_location: None,
            }],
            default_config: None,
        }]);
        let overrides = &policies[0].overrides;
        assert_eq!(overrides.len(), 8, "T1");
        assert_eq!(
            overrides
                .iter()
                .filter(|entry| entry.name == "web_search")
                .count(),
            1,
            "T2"
        );
        assert!(
            !overrides
                .iter()
                .find(|entry| entry.name == "web_search")
                .expect("official member")
                .policy
                .enabled,
            "T2"
        );
        let unknown = AgentTool::AgentToolset20260401 {
            configs: vec![AgentToolConfig {
                name: "parallel_web_search".into(),
                kind: None,
                enabled: None,
                permission_policy: None,
                allowed_domains: None,
                blocked_domains: None,
                max_content_tokens: None,
                url_sources: None,
                user_location: None,
            }],
            default_config: None,
        };
        assert!(!is_agent_toolset_member("parallel_web_search"), "T3");
        assert_eq!(
            validate_agent_tools(std::slice::from_ref(&unknown)).unwrap_err(),
            "unknown agent tool `parallel_web_search`",
            "T3"
        );
    }

    #[test]
    fn managed_toolset_permission_defaults_and_overrides_match_the_wire_contract() {
        // Cause/effect graph: C1 Agent vs MCP source selects the Managed default;
        // C2 an explicit toolset default replaces it; C3 an exact tool config
        // replaces that default. Effects are the one normalized policies used by
        // model visibility, Native authorization, and ACP authorization.
        //
        // | Rule | source | explicit default | exact override | effect |
        // | P1 | Agent | absent | absent | always_allow |
        // | P2 | MCP | absent | absent | always_ask |
        // | P3 | MCP | always_allow | absent | always_allow |
        // | P4 | MCP | always_allow | always_ask | always_ask |
        // FMECA: treating P2 like P1 silently authorizes tools added later by an
        // MCP server; applying P3 after P4 discards the most-specific authoring.
        // Constraints/invariants: exact-tool overrides outrank toolset defaults,
        // and an absent MCP policy remains fail-closed as always-ask.
        use awaken_agent_contract::ToolPermissionRequirement::{AlwaysAllow, AlwaysAsk};

        // Rules D1-D18 exhaust source x optional enabled x optional permission.
        // Each explicit axis replaces only itself; both absent axes retain the
        // exact source default. This native table cross-checks the same pure
        // production resolver as the finite Kani harness.
        for (source, inherited_permission) in [
            (awaken_agent_contract::ToolsetSource::Agent, AlwaysAllow),
            (
                awaken_agent_contract::ToolsetSource::Mcp {
                    server_name: "matrix".into(),
                },
                AlwaysAsk,
            ),
        ] {
            for enabled in [None, Some(false), Some(true)] {
                for permission_policy in [
                    None,
                    Some(AgentToolPermissionPolicy::AlwaysAllow {}),
                    Some(AgentToolPermissionPolicy::AlwaysAsk {}),
                    Some(AgentToolPermissionPolicy::Auto {}),
                ] {
                    let configured = (enabled.is_some() || permission_policy.is_some()).then_some(
                        AgentToolDefaultConfig {
                            enabled,
                            permission_policy,
                        },
                    );
                    let resolved = managed_toolset_default_policy(&source, configured.as_ref());
                    let expected_permission = match permission_policy {
                        Some(AgentToolPermissionPolicy::AlwaysAllow {}) => AlwaysAllow,
                        Some(AgentToolPermissionPolicy::AlwaysAsk {}) => AlwaysAsk,
                        // `auto` is carried, never resolved here: the wire value
                        // is the author's intent and this projection is the
                        // round-trip carrier for it.
                        Some(AgentToolPermissionPolicy::Auto {}) => {
                            awaken_agent_contract::ToolPermissionRequirement::Auto
                        }
                        None => inherited_permission,
                    };
                    assert_eq!(resolved.enabled, enabled.unwrap_or(true), "D1-D18 enabled");
                    assert_eq!(
                        resolved.permission, expected_permission,
                        "D1-D18 permission"
                    );
                }
            }
        }

        let policies = toolset_policies(&[
            AgentTool::AgentToolset20260401 {
                configs: Vec::new(),
                default_config: None,
            },
            AgentTool::McpToolset {
                mcp_server_name: "safe-default".into(),
                configs: Vec::new(),
                default_config: None,
            },
            AgentTool::McpToolset {
                mcp_server_name: "overridden".into(),
                configs: vec![AgentToolConfig {
                    name: "delete_issue".into(),
                    kind: None,
                    enabled: None,
                    permission_policy: Some(AgentToolPermissionPolicy::AlwaysAsk {}),
                    allowed_domains: None,
                    blocked_domains: None,
                    max_content_tokens: None,
                    url_sources: None,
                    user_location: None,
                }],
                default_config: Some(AgentToolDefaultConfig {
                    enabled: None,
                    permission_policy: Some(AgentToolPermissionPolicy::AlwaysAllow {}),
                }),
            },
        ]);

        assert_eq!(policies[0].default.permission, AlwaysAllow, "P1");
        assert_eq!(policies[1].default.permission, AlwaysAsk, "P2");
        assert_eq!(
            policies[1].policy_for("future_tool").permission,
            AlwaysAsk,
            "P2"
        );
        assert_eq!(policies[2].default.permission, AlwaysAllow, "P3");
        assert_eq!(
            policies[2].policy_for("delete_issue").permission,
            AlwaysAsk,
            "P4"
        );
    }

    #[test]
    fn managed_projection_omits_runtime_only_agent_policy_members() {
        // Cause/effect graph: C1 a neutral Session Agent policy contains one
        // official Managed member and C2 one Runtime-only member. E1 the official
        // member is projected with its closed discriminant; E2 the Runtime-only
        // member remains authorized by the source policy but is absent from the
        // Managed wire value.
        //
        // Decision table:
        // | Rule | policy member | Managed member | wire effect |
        // | M1   | read          | yes            | typed config |
        // | M2   | custom_runtime_tool | no       | omitted      |
        // Constraint: Session policy is execution authority; the versioned
        // Managed declaration is the sole public membership authority.
        let policy = awaken_agent_contract::ToolsetPolicy {
            source: awaken_agent_contract::ToolsetSource::Agent,
            default: awaken_agent_contract::ToolExecutionPolicy {
                enabled: false,
                permission: awaken_agent_contract::ToolPermissionRequirement::AlwaysAllow,
            },
            overrides: ["read", "custom_runtime_tool"]
                .into_iter()
                .map(|name| {
                    awaken_agent_contract::ToolPolicyOverride::new(
                        name,
                        awaken_agent_contract::ToolExecutionPolicy::default(),
                    )
                })
                .collect(),
        };

        let projected = project_valid_toolsets(std::slice::from_ref(&policy));
        let AgentTool::AgentToolset20260401 { configs, .. } = &projected[0] else {
            panic!("M1 Agent toolset projection");
        };
        assert_eq!(configs.len(), 1, "M1/M2");
        assert_eq!(configs[0].name, "read", "M1");
        assert_eq!(configs[0].kind, Some(AgentToolsetMember::Read), "M1");
        assert!(
            policy.policy_for("custom_runtime_tool").enabled,
            "M2 source policy"
        );
    }

    #[test]
    fn web_tool_discriminants_and_nullable_boundaries_match_sdk_input() {
        // Cause/effect graph: optional non-null discriminants and domain lists
        // enter the closed per-tool validator; nullable max_content_tokens enters
        // normalization. Invalid presence/type combinations fail before policy
        // construction, while a zero numeric cap survives the exact projection.
        //
        // Decision table:
        // | Rule | type | allowed_domains | max | effect |
        // | W1 | omitted | valid list | 0 | accepted, typed output keeps 0 |
        // | W2 | null | omitted | omitted | serde reject |
        // | W3 | matching | null | omitted | serde reject |
        // | W4 | mismatched | omitted | omitted | validation reject |
        // | W5 | matching | allowlist | 0 | durable JSON shape unchanged |
        // Constraints/invariants: optional is not nullable, discriminants must
        // match their member name, numeric zero remains supplied, and the one
        // opaque durable slot preserves the pre-migration wire representation.
        let authored: AgentTool = serde_json::from_value(serde_json::json!({
            "type": "agent_toolset_20260401",
            "configs": [{
                "name": "web_fetch",
                "allowed_domains": ["docs.example.com"],
                "max_content_tokens": 0
            }]
        }))
        .expect("W1");
        validate_agent_tools(std::slice::from_ref(&authored)).expect("W1");
        let policies = toolset_policies(&[authored]);
        assert_eq!(
            policies[0].configuration_for("web_fetch"),
            Some(&serde_json::json!({
                "type":"web_fetch",
                "domains":{"type":"allow", "domains":["docs.example.com"]},
                "max_content_tokens":0
            })),
            "W5"
        );
        let projected = project_valid_toolsets(&policies);
        let AgentTool::AgentToolset20260401 { configs, .. } = &projected[0] else {
            panic!("W1 Agent toolset projection");
        };
        assert_eq!(configs[0].kind, Some(AgentToolsetMember::WebFetch), "W1");
        assert_eq!(configs[0].max_content_tokens, Some(0), "W1");

        for (rule, config) in [
            (
                "W2",
                serde_json::json!({ "name": "web_fetch", "type": null }),
            ),
            (
                "W3",
                serde_json::json!({
                    "name": "web_fetch", "type": "web_fetch", "allowed_domains": null
                }),
            ),
        ] {
            assert!(
                serde_json::from_value::<AgentToolConfig>(config).is_err(),
                "{rule}"
            );
        }
        let mismatched: AgentToolConfig = serde_json::from_value(serde_json::json!({
            "name": "web_fetch", "type": "web_search"
        }))
        .expect("W4 decodes before cross-field validation");
        assert!(
            validate_agent_tools(&[AgentTool::AgentToolset20260401 {
                configs: vec![mismatched],
                default_config: None,
            }])
            .is_err(),
            "W4"
        );
    }

    #[test]
    fn sdk_url_sources_configuration_projection_decision_table() {
        // P1 omitted/null -> no execution restriction; a resolved WebFetch
        // config still emits the SDK-required nullable response key.
        // P2 shorthand/object -> one normalized closed configuration; P3 every
        // reference belongs to this publication's custom/server roster.
        // P4 all-none, wrong member, unknown key/name -> reject before publish.
        // Existing toolset compiler/projector own both directions, not a second
        // response serializer. Fingerprinting sees the same canonical policy.
        let custom: AgentTool = serde_json::from_value(serde_json::json!({
            "type":"custom","name":"lookup","description":"lookup","input_schema":{"type":"object"}
        }))
        .unwrap();
        let tool: AgentTool = serde_json::from_value(serde_json::json!({
            "type":"agent_toolset_20260401","configs":[{"name":"web_fetch","url_sources":{
                "user_input":"none","server_tool_results":"none","client_tool_results":{"type":"only","tools":[{"type":"tool_reference","name":"lookup"}]}
            }}]
        })).unwrap();
        let policies = compile_agent_tools(&[tool, custom]).unwrap().toolsets;
        let response = serde_json::to_value(project_valid_toolsets(&policies)).unwrap();
        let cfg = response[0]["configs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == "web_fetch")
            .unwrap();
        assert_eq!(
            cfg["url_sources"]["user_input"],
            serde_json::json!({"type":"none"})
        );
        assert_eq!(cfg["url_sources"]["client_tool_results"]["type"], "only");
        for (sources, accepted) in [
            (serde_json::json!(null), true),
            (serde_json::json!({}), true),
            (
                serde_json::json!({"user_input":"none","server_tool_results":"none","client_tool_results":"none"}),
                false,
            ),
            (
                serde_json::json!({"client_tool_results":{"type":"only","tools":[{"type":"tool_reference","name":"missing"}]}}),
                false,
            ),
        ] {
            let tool:AgentTool=serde_json::from_value(serde_json::json!({"type":"agent_toolset_20260401","configs":[{"name":"web_fetch","permission_policy":{"type":"always_ask"},"url_sources":sources}]})).unwrap();
            assert_eq!(
                validate_agent_tools(std::slice::from_ref(&tool)).is_ok(),
                accepted
            );
            if accepted {
                let response =
                    serde_json::to_value(project_valid_toolsets(&toolset_policies(&[tool])))
                        .unwrap();
                let cfg = response[0]["configs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["name"] == "web_fetch")
                    .unwrap();
                assert!(cfg.as_object().unwrap().contains_key("url_sources"));
            }
        }
        let wrong:AgentTool=serde_json::from_value(serde_json::json!({"type":"agent_toolset_20260401","configs":[{"name":"bash","url_sources":null}]})).unwrap();
        assert!(validate_agent_tools(&[wrong]).is_err());
        // P5 a same-named MCP member keeps its own response contract: it cannot
        // acquire Agent-Web-only fields through the shared toolset projector.
        let mcp: AgentTool = serde_json::from_value(serde_json::json!({
        "type":"mcp_toolset", "mcp_server_name":"peer", "configs":[{"name":"web_fetch", "enabled":false}]
    }))
    .unwrap();
        let response =
            serde_json::to_value(project_valid_toolsets(&toolset_policies(&[mcp]))).unwrap();
        assert!(
            !response[0]["configs"][0]
                .as_object()
                .unwrap()
                .contains_key("url_sources")
        );
    }
}

#[cfg(test)]
mod boundary_regressions {
    use super::*;
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPolicyOverride, ToolsetPolicy, ToolsetSource,
    };
    use serde_json::json;

    #[test]
    fn all_duplicates_are_admitted_before_first_wins_and_roster_is_complete() {
        let valid = json!({"type":"agent_toolset_20260401","configs":[{"name":"web_fetch","max_content_tokens":0},{"name":"web_fetch","max_content_tokens":9}]});
        let tool: AgentTool = serde_json::from_value(valid.clone()).unwrap();
        let compiled = compile_agent_tools(&[tool]).unwrap();
        let fetch = compiled.toolsets[0]
            .overrides
            .iter()
            .find(|entry| entry.name == "web_fetch")
            .unwrap();
        assert_eq!(
            fetch.configuration.as_ref().unwrap()["max_content_tokens"],
            0
        );
        let mut invalid = valid;
        invalid["configs"][1] = json!({"name":"web_fetch","allowed_domains":[]});
        let error = compile_agent_tools(&[serde_json::from_value(invalid).unwrap()]).unwrap_err();
        assert_eq!(error.path, "tools[0].configs[1].allowed_domains");
        assert_eq!(error.code, "invalid_web_domain_count");

        let fetch: AgentTool = serde_json::from_value(json!({"type":"agent_toolset_20260401","configs":[{"name":"web_fetch","url_sources":{"client_tool_results":{"type":"only","tools":[{"type":"tool_reference","name":"lookup"}]}}}]})).unwrap();
        assert_eq!(
            compile_agent_tools(std::slice::from_ref(&fetch))
                .unwrap_err()
                .code,
            "web_source_tool_unavailable"
        );
        let custom: AgentTool = serde_json::from_value(json!({"type":"custom","name":"lookup","description":"lookup","input_schema":{"type":"object","properties":{},"x-future":{"kept":true}}})).unwrap();
        let compiled = compile_agent_tools(&[fetch, custom]).unwrap();
        assert_eq!(
            compiled.client_tools[0].input_schema["x-future"],
            json!({"kept":true})
        );
    }

    #[test]
    fn projection_only_interprets_owned_web_settings_and_is_fallible() {
        let opaque = json!({"type":"future","domains":7,"url_sources":{"unknown":true}});
        for (source, name) in [
            (ToolsetSource::Agent, "read"),
            (
                ToolsetSource::Mcp {
                    server_name: "remote".into(),
                },
                "web_fetch",
            ),
        ] {
            let policies = vec![ToolsetPolicy {
                source,
                default: ToolExecutionPolicy::default(),
                overrides: vec![ToolPolicyOverride::with_optional_configuration(
                    name,
                    ToolExecutionPolicy::default(),
                    Some(opaque.clone()),
                )],
            }];
            let before = policies.clone();
            assert!(resolved_toolsets(&policies).is_ok());
            assert_eq!(policies, before);
        }
        let malformed = vec![ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy::default(),
            overrides: vec![ToolPolicyOverride::with_optional_configuration(
                "web_fetch",
                ToolExecutionPolicy::default(),
                Some(opaque),
            )],
        }];
        assert_eq!(
            resolved_toolsets(&malformed).unwrap_err().code,
            "web_configuration_owner_mismatch"
        );
    }
}
