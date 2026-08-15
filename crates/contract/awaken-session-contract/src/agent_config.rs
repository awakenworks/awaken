//! Managed Agents tool wire vocabulary retained temporarily by Session storage.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Durable Managed Agent tool projection. This is the single serialized owner
/// shared by the wire adapter and Session store; JSON Schema keywords remain the
/// only deliberately open part of the value.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentTool {
    #[serde(rename = "agent_toolset_20260401")]
    AgentToolset20260401 {
        #[serde(default)]
        configs: Vec<AgentToolConfig>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_config: Option<AgentToolDefaultConfig>,
    },
    McpToolset {
        mcp_server_name: String,
        #[serde(default)]
        configs: Vec<AgentToolConfig>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_config: Option<AgentToolDefaultConfig>,
    },
    Custom {
        name: String,
        description: String,
        input_schema: CustomToolInputSchema,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CustomToolInputSchema {
    #[serde(rename = "type")]
    pub kind: ObjectSchemaKind,
    #[serde(flatten)]
    pub keywords: BTreeMap<String, serde_json::Value>,
}

impl CustomToolInputSchema {
    pub fn from_value(value: serde_json::Value) -> Result<Self, String> {
        serde_json::from_value(value).map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ObjectSchemaKind {
    #[serde(rename = "object")]
    Object,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentToolPermissionPolicy {
    AlwaysAllow,
    AlwaysAsk,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentToolConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_policy: Option<AgentToolPermissionPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentToolDefaultConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_policy: Option<AgentToolPermissionPolicy>,
}

/// Closed discriminant for the official versioned Runtime capability surface.
/// The finite enum keeps symbolic proofs away from unbounded string comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentToolsetMember {
    Bash,
    Read,
    Write,
    Edit,
    Glob,
    Grep,
    WebFetch,
    WebSearch,
}

impl AgentToolsetMember {
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
}

const AGENT_TOOLSET_MEMBERS: [AgentToolsetMember; 8] = [
    AgentToolsetMember::Bash,
    AgentToolsetMember::Read,
    AgentToolsetMember::Write,
    AgentToolsetMember::Edit,
    AgentToolsetMember::Glob,
    AgentToolsetMember::Grep,
    AgentToolsetMember::WebFetch,
    AgentToolsetMember::WebSearch,
];

/// The official Managed Agent toolset's versioned membership. It lives beside
/// the wire value so every adapter normalizes the same closed set.
pub const AGENT_TOOLSET_TOOL_IDS: [&str; 8] = [
    AgentToolsetMember::Bash.as_str(),
    AgentToolsetMember::Read.as_str(),
    AgentToolsetMember::Write.as_str(),
    AgentToolsetMember::Edit.as_str(),
    AgentToolsetMember::Glob.as_str(),
    AgentToolsetMember::Grep.as_str(),
    AgentToolsetMember::WebFetch.as_str(),
    AgentToolsetMember::WebSearch.as_str(),
];

/// Allocation-free traversal of the complete versioned Runtime capability
/// projection. Keeping the iterator beside the closed array prevents adapters
/// from maintaining a second list or accidentally omitting the final member.
#[must_use]
pub fn agent_toolset_members() -> impl ExactSizeIterator<Item = &'static str> + DoubleEndedIterator
{
    bounded_agent_toolset_members().map(AgentToolsetMember::as_str)
}

#[must_use]
fn bounded_agent_toolset_members() -> core::array::IntoIter<AgentToolsetMember, 8> {
    AGENT_TOOLSET_MEMBERS.into_iter()
}

#[must_use]
pub fn is_agent_toolset_member(name: &str) -> bool {
    agent_toolset_members().any(|member| member == name)
}

/// Normalize the durable wire vocabulary into the one neutral execution policy.
#[must_use]
pub fn toolset_policies(tools: &[AgentTool]) -> Vec<awaken_agent_contract::ToolsetPolicy> {
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };

    let permission = |value: Option<AgentToolPermissionPolicy>,
                      fallback: ToolPermissionRequirement| match value {
        Some(AgentToolPermissionPolicy::AlwaysAllow) => ToolPermissionRequirement::AlwaysAllow,
        Some(AgentToolPermissionPolicy::AlwaysAsk) => ToolPermissionRequirement::AlwaysAsk,
        None => fallback,
    };
    let default_permission = |source: &ToolsetSource| match source {
        ToolsetSource::Agent => ToolPermissionRequirement::AlwaysAllow,
        // Managed Agents defaults MCP to confirmation so a tool added later by
        // the remote server cannot silently gain execution authority.
        ToolsetSource::Mcp { .. } => ToolPermissionRequirement::AlwaysAsk,
    };
    tools
        .iter()
        .filter_map(|tool| {
            let (source, configs, default_config) = match tool {
                AgentTool::AgentToolset20260401 {
                    configs,
                    default_config,
                } => (ToolsetSource::Agent, configs, default_config.as_ref()),
                AgentTool::McpToolset {
                    mcp_server_name,
                    configs,
                    default_config,
                } => (
                    ToolsetSource::Mcp {
                        server_name: mcp_server_name.clone(),
                    },
                    configs,
                    default_config.as_ref(),
                ),
                AgentTool::Custom { .. } => return None,
            };
            let default = ToolExecutionPolicy {
                enabled: default_config
                    .and_then(|value| value.enabled)
                    .unwrap_or(true),
                permission: permission(
                    default_config.and_then(|value| value.permission_policy),
                    default_permission(&source),
                ),
            };
            let overrides = match &source {
                ToolsetSource::Agent => {
                    let mut resolved = agent_toolset_members()
                        .map(|name| {
                            let config = configs.iter().find(|config| config.name == name);
                            ToolPolicyOverride {
                                name: name.to_string(),
                                policy: ToolExecutionPolicy {
                                    enabled: config
                                        .and_then(|value| value.enabled)
                                        .unwrap_or(default.enabled),
                                    permission: config
                                        .and_then(|value| value.permission_policy)
                                        .map(|value| permission(Some(value), default.permission))
                                        .unwrap_or(default.permission),
                                },
                            }
                        })
                        .collect::<Vec<_>>();
                    resolved.extend(
                        configs
                            .iter()
                            .filter(|config| !is_agent_toolset_member(&config.name))
                            .map(|config| ToolPolicyOverride {
                                name: config.name.clone(),
                                policy: ToolExecutionPolicy {
                                    enabled: config.enabled.unwrap_or(default.enabled),
                                    permission: config
                                        .permission_policy
                                        .map(|value| permission(Some(value), default.permission))
                                        .unwrap_or(default.permission),
                                },
                            }),
                    );
                    resolved
                }
                ToolsetSource::Mcp { .. } => configs
                    .iter()
                    .map(|config| ToolPolicyOverride {
                        name: config.name.clone(),
                        policy: ToolExecutionPolicy {
                            enabled: config.enabled.unwrap_or(default.enabled),
                            permission: config
                                .permission_policy
                                .map(|value| permission(Some(value), default.permission))
                                .unwrap_or(default.permission),
                        },
                    })
                    .collect(),
            };
            Some(ToolsetPolicy {
                source,
                default,
                overrides,
            })
        })
        .collect()
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(9)]
fn bounded_runtime_capability_iterator_uses_exact_member_projection() {
    let expected = [
        AgentToolsetMember::Bash,
        AgentToolsetMember::Read,
        AgentToolsetMember::Write,
        AgentToolsetMember::Edit,
        AgentToolsetMember::Glob,
        AgentToolsetMember::Grep,
        AgentToolsetMember::WebFetch,
        AgentToolsetMember::WebSearch,
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
#[kani::proof]
fn runtime_capability_member_projection_is_total_exact_and_bounded() {
    let index: u8 = kani::any();
    kani::assume(index <= 8);
    let projected = bounded_agent_toolset_members().nth(usize::from(index));
    let expected = match index {
        0 => Some(AgentToolsetMember::Bash),
        1 => Some(AgentToolsetMember::Read),
        2 => Some(AgentToolsetMember::Write),
        3 => Some(AgentToolsetMember::Edit),
        4 => Some(AgentToolsetMember::Glob),
        5 => Some(AgentToolsetMember::Grep),
        6 => Some(AgentToolsetMember::WebFetch),
        7 => Some(AgentToolsetMember::WebSearch),
        _ => None,
    };
    assert_eq!(projected, expected);
}

/// Project a resolved policy back to its canonical durable wire value.
#[must_use]
pub fn resolved_toolsets(policies: &[awaken_agent_contract::ToolsetPolicy]) -> Vec<AgentTool> {
    use awaken_agent_contract::{ToolPermissionRequirement, ToolsetSource};

    let permission = |value| match value {
        ToolPermissionRequirement::AlwaysAllow => AgentToolPermissionPolicy::AlwaysAllow,
        ToolPermissionRequirement::AlwaysAsk => AgentToolPermissionPolicy::AlwaysAsk,
    };
    policies
        .iter()
        .map(|policy| {
            let configs = policy
                .overrides
                .iter()
                .filter(|entry| entry.policy != policy.default)
                .map(|entry| AgentToolConfig {
                    name: entry.name.clone(),
                    enabled: Some(entry.policy.enabled),
                    permission_policy: Some(permission(entry.policy.permission)),
                })
                .collect();
            let default_config = Some(AgentToolDefaultConfig {
                enabled: Some(policy.default.enabled),
                permission_policy: Some(permission(policy.default.permission)),
            });
            match &policy.source {
                ToolsetSource::Agent => AgentTool::AgentToolset20260401 {
                    configs,
                    default_config,
                },
                ToolsetSource::Mcp { server_name } => AgentTool::McpToolset {
                    mcp_server_name: server_name.clone(),
                    configs,
                    default_config,
                },
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // | T3   | unknown name            | retained for admission reject  |
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
        let policies = toolset_policies(&[AgentTool::AgentToolset20260401 {
            configs: vec![AgentToolConfig {
                name: "web_search".into(),
                enabled: Some(false),
                permission_policy: None,
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
        assert!(!is_agent_toolset_member("parallel_web_search"), "T3");
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
        use awaken_agent_contract::ToolPermissionRequirement::{AlwaysAllow, AlwaysAsk};

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
                    enabled: None,
                    permission_policy: Some(AgentToolPermissionPolicy::AlwaysAsk),
                }],
                default_config: Some(AgentToolDefaultConfig {
                    enabled: None,
                    permission_policy: Some(AgentToolPermissionPolicy::AlwaysAllow),
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
}
