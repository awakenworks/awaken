// @formal entrypoint runtime_capability.bounded_iterator_uses_the_exact_eight_member_projection
// @formal entrypoint tool_policy.managed_agent_and_mcp_defaults_are_source_exact_and_configurable
//! Neutral tool defaults, membership, MCP pairing and authored presets.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
/// Resolve the source-specific neutral defaults independently on both axes.
pub fn toolset_default_policy(
    source: &awaken_agent_contract::ToolsetSource,
    enabled: Option<bool>,
    permission: Option<awaken_agent_contract::ToolPermissionRequirement>,
) -> awaken_agent_contract::ToolExecutionPolicy {
    use awaken_agent_contract::{ToolExecutionPolicy, ToolPermissionRequirement, ToolsetSource};
    ToolExecutionPolicy {
        enabled: enabled.unwrap_or(true),
        permission: permission.unwrap_or(match source {
            ToolsetSource::Agent => ToolPermissionRequirement::AlwaysAllow,
            ToolsetSource::Mcp { .. } => ToolPermissionRequirement::AlwaysAsk,
        }),
    }
}
/// Closed discriminant for the official versioned Runtime capability surface.
/// The finite enum keeps symbolic proofs away from unbounded string comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinToolsetMember {
    Bash,
    Read,
    Write,
    Edit,
    Glob,
    Grep,
    WebFetch,
    WebSearch,
}

impl BuiltinToolsetMember {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Read => "read",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::Glob => "glob",
            Self::Grep => "grep",
            Self::WebFetch => "web_fetch",
            Self::WebSearch => "web_search",
        }
    }

    /// Whether the controlled-modification authoring projection requires human
    /// approval for this capability. This versioned member enum is the single
    /// owner; UI capability metadata and Assistant authoring only project it.
    #[must_use]
    pub const fn controlled_modification(self) -> bool {
        matches!(self, Self::Bash | Self::Write | Self::Edit)
    }
}

const AGENT_TOOLSET_MEMBERS: [BuiltinToolsetMember; 8] = [
    BuiltinToolsetMember::Bash,
    BuiltinToolsetMember::Read,
    BuiltinToolsetMember::Write,
    BuiltinToolsetMember::Edit,
    BuiltinToolsetMember::Glob,
    BuiltinToolsetMember::Grep,
    BuiltinToolsetMember::WebFetch,
    BuiltinToolsetMember::WebSearch,
];

/// Stable builtin capability membership shared by Session capability projections.
pub const AGENT_TOOLSET_TOOL_IDS: [&str; 8] = agent_toolset_member_ids();

const fn agent_toolset_member_ids() -> [&'static str; 8] {
    let mut ids = [""; 8];
    let mut index = 0;
    while index < AGENT_TOOLSET_MEMBERS.len() {
        ids[index] = AGENT_TOOLSET_MEMBERS[index].as_str();
        index += 1;
    }
    ids
}

/// Allocation-free traversal of the complete versioned Runtime capability
/// projection. Keeping the iterator beside the closed array prevents adapters
/// from maintaining a second list or accidentally omitting the final member.
#[must_use]
pub fn agent_toolset_members() -> impl ExactSizeIterator<Item = &'static str> + DoubleEndedIterator
{
    bounded_agent_toolset_members().map(BuiltinToolsetMember::as_str)
}

#[must_use]
fn bounded_agent_toolset_members() -> core::array::IntoIter<BuiltinToolsetMember, 8> {
    AGENT_TOOLSET_MEMBERS.into_iter()
}

#[must_use]
pub fn is_agent_toolset_member(name: &str) -> bool {
    agent_toolset_members().any(|member| member == name)
}

/// Preserve Agent overrides that the closed Managed wire cannot represent.
/// `current` is the version-fenced durable authority; `replacement` contains the
/// caller's complete wire-authored canonical policy. The caller supplies ids
/// proved by its current scoped catalog and typed semantic role, so retired,
/// unknown, client, or dynamic name collisions cannot survive merely because
/// the closed wire cannot represent them. This function never creates an opaque
/// override from wire input and never changes an admitted override's bytes.
pub fn preserve_runtime_agent_overrides(
    current: &[awaken_agent_contract::ToolsetPolicy],
    replacement: &mut Vec<awaken_agent_contract::ToolsetPolicy>,
    allowed_runtime_ids: &std::collections::BTreeSet<String>,
) {
    use awaken_agent_contract::ToolsetSource;

    let Some(current_agent) = current
        .iter()
        .find(|toolset| toolset.source == ToolsetSource::Agent)
    else {
        return;
    };
    let opaque = current_agent
        .overrides
        .iter()
        .filter(|entry| {
            !is_agent_toolset_member(&entry.name)
                && allowed_runtime_ids.contains(entry.name.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    if opaque.is_empty() {
        return;
    }
    if let Some(replacement_agent) = replacement
        .iter_mut()
        .find(|toolset| toolset.source == ToolsetSource::Agent)
    {
        replacement_agent.overrides.extend(opaque);
    } else {
        replacement.push(awaken_agent_contract::ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: current_agent.default,
            overrides: opaque,
        });
    }
}

#[must_use]
pub fn is_controlled_modification_member(name: &str) -> bool {
    AGENT_TOOLSET_MEMBERS
        .iter()
        .copied()
        .find(|member| member.as_str() == name)
        .is_some_and(BuiltinToolsetMember::controlled_modification)
}

/// Transient authoring intent. This value is consumed while producing the one
/// typed Toolset policy and is never stored in an Agent configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPermissionPreset {
    ControlledModifications,
}

/// Borrowed fields of the existing AgentConfig authority. Keeping the pure
/// projection beside the closed member roster lets HTTP, Web, and Assistant
/// entry points share one transform without introducing another config type.
pub struct AgentPermissionPresetTarget<'a> {
    pub tool_ids: &'a mut Vec<String>,
    pub toolsets: &'a mut Vec<awaken_agent_contract::ToolsetPolicy>,
    pub mcp_servers: &'a [awaken_runtime_contract::agent_bindings::AgentMcpServerBinding],
    pub plugin_ids: &'a mut Vec<String>,
    pub plugin_config: &'a mut BTreeMap<String, serde_json::Value>,
    /// Runtime-only ids proven by the caller's current scoped catalog and typed
    /// semantic roles. Historical opaque names are not compatibility authority.
    pub allowed_runtime_override_ids: &'a std::collections::BTreeSet<String>,
}

/// Ensure every authored MCP binding has one typed fail-closed Toolset. A
/// historical generic permission document cannot be inferred and is rejected
/// before mutation; fresh bindings receive the canonical default-ask policy.
pub fn ensure_typed_mcp_toolset_policies(
    toolsets: &mut Vec<awaken_agent_contract::ToolsetPolicy>,
    mcp_servers: &[awaken_runtime_contract::agent_bindings::AgentMcpServerBinding],
    has_legacy_permission: bool,
) -> Result<(), String> {
    use awaken_agent_contract::{ToolsetPolicy, ToolsetSource};

    let typed_servers = toolsets
        .iter()
        .filter_map(|toolset| match &toolset.source {
            ToolsetSource::Mcp { server_name } => Some(server_name.as_str()),
            ToolsetSource::Agent => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    let missing = mcp_servers
        .iter()
        .filter(|server| !typed_servers.contains(server.name.as_str()))
        .map(|server| server.name.clone())
        .collect::<Vec<_>>();
    if has_legacy_permission && !missing.is_empty() {
        return Err(format!(
            "permission migration_required: MCP servers {missing:?} have no typed MCP policy; historical plugin_config.permission semantics cannot be inferred"
        ));
    }
    for server_name in missing {
        let source = ToolsetSource::Mcp { server_name };
        toolsets.push(ToolsetPolicy {
            default: toolset_default_policy(&source, None, None),
            source,
            overrides: Vec::new(),
        });
    }
    Ok(())
}

/// Project one transient preset into the canonical typed Toolset authority.
/// Bash is enabled; write/edit retain their authored enablement; all three
/// require confirmation.
/// Git subcommands remain opaque Bash data; this transform does not parse or
/// invent a Git-specific capability.
pub fn apply_agent_permission_preset(
    target: AgentPermissionPresetTarget<'_>,
    preset: AgentPermissionPreset,
) -> Result<(), String> {
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };

    ensure_typed_mcp_toolset_policies(
        target.toolsets,
        target.mcp_servers,
        target.plugin_config.contains_key("permission"),
    )?;

    let existing = target
        .toolsets
        .iter()
        .find(|toolset| toolset.source == ToolsetSource::Agent)
        .cloned();
    let selected_exact = target
        .tool_ids
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut overrides = Vec::new();
    for member in bounded_agent_toolset_members() {
        let name = member.as_str();
        let existing_override = existing
            .as_ref()
            .and_then(|toolset| toolset.overrides.iter().find(|entry| entry.name == name));
        let existing_policy = existing.as_ref().map(|toolset| toolset.policy_for(name));
        let previously_enabled =
            selected_exact.contains(name) || existing_policy.is_some_and(|policy| policy.enabled);
        let enabled = match (preset, member) {
            (AgentPermissionPreset::ControlledModifications, BuiltinToolsetMember::Bash) => true,
            _ => previously_enabled,
        };

        if !member.controlled_modification()
            && let Some(existing_override) = existing_override
        {
            overrides.push(existing_override.clone());
            continue;
        }
        if !enabled && !member.controlled_modification() {
            continue;
        }
        let permission = match (preset, member) {
            (
                AgentPermissionPreset::ControlledModifications,
                BuiltinToolsetMember::Bash
                | BuiltinToolsetMember::Write
                | BuiltinToolsetMember::Edit,
            ) => ToolPermissionRequirement::AlwaysAsk,
            _ => existing_policy
                .map(|policy| policy.permission)
                .unwrap_or(ToolPermissionRequirement::AlwaysAllow),
        };
        overrides.push(ToolPolicyOverride::with_optional_configuration(
            name,
            ToolExecutionPolicy {
                enabled,
                permission,
            },
            existing
                .as_ref()
                .and_then(|toolset| toolset.configuration_for(name))
                .cloned(),
        ));
    }
    target
        .tool_ids
        .retain(|name| !is_agent_toolset_member(name));
    let mut replacement = vec![ToolsetPolicy {
        source: ToolsetSource::Agent,
        default: ToolExecutionPolicy {
            enabled: false,
            permission: ToolPermissionRequirement::AlwaysAllow,
        },
        overrides,
    }];
    preserve_runtime_agent_overrides(
        target.toolsets,
        &mut replacement,
        target.allowed_runtime_override_ids,
    );
    target
        .toolsets
        .retain(|toolset| toolset.source != ToolsetSource::Agent);
    target
        .toolsets
        .insert(0, replacement.pop().expect("one Agent replacement"));
    target.plugin_config.remove("permission");
    target.plugin_ids.retain(|id| id != "permission");
    Ok(())
}

/// Validate the shared MCP ownership invariant after an API adapter has applied
/// replacement/inheritance semantics: each effective server has exactly one
/// effective MCP ToolSet, and no ToolSet points outside that server set.
pub fn validate_mcp_toolset_pairing(
    server_names: &[String],
    toolset_server_names: &[String],
) -> Result<(), String> {
    let servers = server_names
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    if servers.len() != server_names.len() {
        return Err("mcp_server names must be unique".into());
    }
    let toolsets = toolset_server_names
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    if toolsets.len() != toolset_server_names.len() {
        return Err("each mcp_server must be referenced by exactly one mcp_toolset".into());
    }
    if servers != toolsets {
        return Err("each mcp_server must be referenced by exactly one mcp_toolset".into());
    }
    Ok(())
}

#[cfg(kani)]
/// @obligation runtime_capability.bounded_iterator_uses_the_exact_eight_member_projection
#[kani::proof]
#[kani::unwind(9)]
fn bounded_runtime_capability_iterator_uses_exact_member_projection() {
    let expected = [
        BuiltinToolsetMember::Bash,
        BuiltinToolsetMember::Read,
        BuiltinToolsetMember::Write,
        BuiltinToolsetMember::Edit,
        BuiltinToolsetMember::Glob,
        BuiltinToolsetMember::Grep,
        BuiltinToolsetMember::WebFetch,
        BuiltinToolsetMember::WebSearch,
    ];
    let mut projected = bounded_agent_toolset_members();
    let mut index = 0usize;
    while index < expected.len() {
        assert_eq!(projected.next(), Some(expected[index]));
        index += 1;
    }
    assert_eq!(projected.next(), None);
}

#[cfg(kani)]
/// @obligation runtime_capability.bounded_iterator_uses_the_exact_eight_member_projection
#[kani::proof]
fn runtime_capability_member_projection_is_total_exact_and_bounded() {
    let index: u8 = kani::any();
    kani::assume(index <= 8);
    let projected = bounded_agent_toolset_members().nth(usize::from(index));
    let expected = match index {
        0 => Some(BuiltinToolsetMember::Bash),
        1 => Some(BuiltinToolsetMember::Read),
        2 => Some(BuiltinToolsetMember::Write),
        3 => Some(BuiltinToolsetMember::Edit),
        4 => Some(BuiltinToolsetMember::Glob),
        5 => Some(BuiltinToolsetMember::Grep),
        6 => Some(BuiltinToolsetMember::WebFetch),
        7 => Some(BuiltinToolsetMember::WebSearch),
        _ => None,
    };
    assert_eq!(projected, expected);
}

#[cfg(kani)]
/// @obligation tool_policy.managed_agent_and_mcp_defaults_are_source_exact_and_configurable
#[kani::proof]
fn managed_toolset_default_resolution_is_source_exact_and_configurable() {
    use awaken_agent_contract::{ToolPermissionRequirement, ToolsetSource};

    // Cause/effect decision table: five symbolic booleans encode source,
    // enabled presence/value, and permission presence/value (2^5 paths, with
    // absent-value duplicates). They collapse to the 2 x 3 x 3 = 18 semantic
    // rules: absent axes inherit source defaults, while present axes replace
    // only their exact enabled or permission result.
    let mcp_source: bool = kani::any();
    let enabled_present: bool = kani::any();
    let enabled_value: bool = kani::any();
    let permission_present: bool = kani::any();
    let permission_ask: bool = kani::any();
    let source = if mcp_source {
        ToolsetSource::Mcp {
            server_name: String::new(),
        }
    } else {
        ToolsetSource::Agent
    };
    let resolved = toolset_default_policy(
        &source,
        enabled_present.then_some(enabled_value),
        permission_present.then_some(if permission_ask {
            ToolPermissionRequirement::AlwaysAsk
        } else {
            ToolPermissionRequirement::AlwaysAllow
        }),
    );
    let expected_permission = if permission_present {
        if permission_ask {
            ToolPermissionRequirement::AlwaysAsk
        } else {
            ToolPermissionRequirement::AlwaysAllow
        }
    } else if mcp_source {
        ToolPermissionRequirement::AlwaysAsk
    } else {
        ToolPermissionRequirement::AlwaysAllow
    };

    assert_eq!(resolved.enabled, !enabled_present || enabled_value);
    assert_eq!(resolved.permission, expected_permission);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opaque_runtime_override_preservation_has_one_closed_member_owner() {
        // Cause/effect table:
        // | rule | current opaque | replacement Agent | effect |
        // | O1   | allowed agent_run + retired names | present | retain agent_run only |
        // | O2   | allowed agent_run + retired names | absent  | add owner with agent_run only |
        // | O3   | no allowed Runtime id             | either  | replacement unchanged |
        // Canonical members remain wire-owned, retired/unknown names cannot
        // survive by opacity, and MCP policies are never touched.
        use awaken_agent_contract::{
            ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
            ToolsetSource,
        };

        let default = ToolExecutionPolicy {
            enabled: false,
            permission: ToolPermissionRequirement::AlwaysAllow,
        };
        let canonical = ToolPolicyOverride::new("read", ToolExecutionPolicy::default());
        let opaque = ToolPolicyOverride::with_optional_configuration(
            "agent_run",
            ToolExecutionPolicy {
                enabled: true,
                permission: ToolPermissionRequirement::AlwaysAsk,
            },
            Some(serde_json::json!({ "runtime": "exact" })),
        );
        let current = vec![ToolsetPolicy {
            source: ToolsetSource::Agent,
            default,
            overrides: vec![
                canonical,
                opaque.clone(),
                ToolPolicyOverride::new("delete", ToolExecutionPolicy::default()),
                ToolPolicyOverride::new("custom_dynamic", ToolExecutionPolicy::default()),
            ],
        }];
        let mcp = ToolsetPolicy {
            source: ToolsetSource::Mcp {
                server_name: "docs".into(),
            },
            default: ToolExecutionPolicy::default(),
            overrides: Vec::new(),
        };
        let mut present = vec![
            ToolsetPolicy {
                source: ToolsetSource::Agent,
                default,
                overrides: vec![ToolPolicyOverride::new(
                    "write",
                    ToolExecutionPolicy::default(),
                )],
            },
            mcp.clone(),
        ];
        let allowed = std::collections::BTreeSet::from(["agent_run".to_string()]);
        preserve_runtime_agent_overrides(&current, &mut present, &allowed);
        assert_eq!(present[0].overrides.len(), 2, "O1 canonical is not revived");
        assert_eq!(present[0].overrides[1], opaque, "O1 exact opaque");
        assert_eq!(present[1], mcp, "O1 MCP unchanged");

        let mut absent = vec![mcp.clone()];
        preserve_runtime_agent_overrides(&current, &mut absent, &allowed);
        assert_eq!(absent[0], mcp, "O2 MCP unchanged");
        assert_eq!(absent[1].source, ToolsetSource::Agent, "O2");
        assert_eq!(absent[1].default, default, "O2");
        assert_eq!(absent[1].overrides, vec![opaque], "O2");

        let mut unchanged = vec![mcp.clone()];
        preserve_runtime_agent_overrides(
            &[ToolsetPolicy {
                source: ToolsetSource::Agent,
                default,
                overrides: vec![ToolPolicyOverride::new(
                    "read",
                    ToolExecutionPolicy::default(),
                )],
            }],
            &mut unchanged,
            &std::collections::BTreeSet::new(),
        );
        assert_eq!(unchanged, vec![mcp], "O3");
    }

    #[test]
    fn effective_mcp_pairing_covers_the_complete_bijection_table() {
        // This is shared by Agent authoring and Session-local replacements.
        // The table covers absence, exact pairing, missing, dangling, and both
        // duplicate axes so an adapter cannot accidentally weaken the contract.
        for (servers, toolsets, valid) in [
            (vec![], vec![], true),
            (vec!["docs"], vec!["docs"], true),
            (vec!["docs"], vec![], false),
            (vec![], vec!["docs"], false),
            (vec!["docs", "docs"], vec!["docs"], false),
            (vec!["docs"], vec!["docs", "docs"], false),
        ] {
            let servers = servers.into_iter().map(str::to_string).collect::<Vec<_>>();
            let toolsets = toolsets.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert_eq!(
                validate_mcp_toolset_pairing(&servers, &toolsets).is_ok(),
                valid,
                "servers={servers:?}, toolsets={toolsets:?}"
            );
        }
    }

    #[test]
    fn controlled_preset_has_one_typed_fail_closed_projection() {
        // Cause/effect graph: C1 a selected generic Bash exists; C2 read/write
        // are selected; C3 retired permission-plugin residue exists. Effects:
        // E1 Bash and write remain enabled+ask; E2 read stays enabled+allow; E3
        // no command-specific pseudo member is invented; E4 legacy residue is
        // removed; E5 replay is stable. The adjacent roster test owns exact
        // eight-member vocabulary coverage.
        //
        // Decision table:
        // | rule | member             | selected | enabled | permission |
        // | P1   | bash               | yes      | true    | ask        |
        // | P2   | read               | yes      | true    | allow      |
        // | P3   | write              | yes      | true    | ask        |
        use awaken_agent_contract::{ToolPermissionRequirement, ToolsetSource};

        let mut tool_ids = vec![
            "bash".into(),
            "read".into(),
            "write".into(),
            "custom".into(),
        ];
        let mut toolsets = Vec::new();
        let mut plugin_ids = vec!["permission".into(), "memory".into()];
        let mut plugin_config = BTreeMap::from([
            (
                "permission".into(),
                serde_json::json!({ "default_behavior": "ask" }),
            ),
            ("memory".into(), serde_json::json!({ "enabled": true })),
        ]);
        let allowed_runtime_override_ids = std::collections::BTreeSet::new();
        let apply = |tool_ids: &mut Vec<String>,
                     toolsets: &mut Vec<awaken_agent_contract::ToolsetPolicy>,
                     plugin_ids: &mut Vec<String>,
                     plugin_config: &mut BTreeMap<String, serde_json::Value>| {
            apply_agent_permission_preset(
                AgentPermissionPresetTarget {
                    tool_ids,
                    toolsets,
                    mcp_servers: &[],
                    plugin_ids,
                    plugin_config,
                    allowed_runtime_override_ids: &allowed_runtime_override_ids,
                },
                AgentPermissionPreset::ControlledModifications,
            )
            .expect("preset projection")
        };
        apply(
            &mut tool_ids,
            &mut toolsets,
            &mut plugin_ids,
            &mut plugin_config,
        );
        let agent = toolsets
            .iter()
            .find(|toolset| toolset.source == ToolsetSource::Agent)
            .expect("one Agent Toolset");
        for (rule, name, enabled, permission) in [
            ("P1", "bash", true, ToolPermissionRequirement::AlwaysAsk),
            ("P2", "read", true, ToolPermissionRequirement::AlwaysAllow),
            ("P3", "write", true, ToolPermissionRequirement::AlwaysAsk),
        ] {
            let policy = agent.policy_for(name);
            assert_eq!(
                (policy.enabled, policy.permission),
                (enabled, permission),
                "{rule}"
            );
        }
        assert_eq!(tool_ids, ["custom"], "P1-P3 use one typed owner");
        assert!(
            agent
                .overrides
                .iter()
                .all(|entry| !matches!(entry.name.as_str(), "push" | "delete")),
            "official vocabulary has no Git-specialized member"
        );
        assert_eq!(plugin_ids, ["memory"], "E4");
        assert!(!plugin_config.contains_key("permission"), "E4");
        let stable = (
            tool_ids.clone(),
            toolsets.clone(),
            plugin_ids.clone(),
            plugin_config.clone(),
        );
        apply(
            &mut tool_ids,
            &mut toolsets,
            &mut plugin_ids,
            &mut plugin_config,
        );
        assert_eq!(
            (tool_ids, toolsets, plugin_ids, plugin_config),
            stable,
            "E5"
        );
    }
}
