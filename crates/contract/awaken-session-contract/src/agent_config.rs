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

/// The official Managed Agent toolset's versioned membership. It lives beside
/// the wire value so every adapter normalizes the same closed set.
pub const AGENT_TOOLSET_TOOL_IDS: [&str; 8] = [
    "bash",
    "read",
    "write",
    "edit",
    "glob",
    "grep",
    "web_fetch",
    "web_search",
];

#[must_use]
pub fn is_agent_toolset_member(name: &str) -> bool {
    AGENT_TOOLSET_TOOL_IDS.contains(&name)
}

/// Normalize the durable wire vocabulary into the one neutral execution policy.
#[must_use]
pub fn toolset_policies(tools: &[AgentTool]) -> Vec<awaken_agent_contract::ToolsetPolicy> {
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };

    let permission = |value: Option<AgentToolPermissionPolicy>| match value
        .unwrap_or(AgentToolPermissionPolicy::AlwaysAllow)
    {
        AgentToolPermissionPolicy::AlwaysAllow => ToolPermissionRequirement::AlwaysAllow,
        AgentToolPermissionPolicy::AlwaysAsk => ToolPermissionRequirement::AlwaysAsk,
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
                permission: permission(default_config.and_then(|value| value.permission_policy)),
            };
            let overrides = match &source {
                ToolsetSource::Agent => {
                    let mut resolved = AGENT_TOOLSET_TOOL_IDS
                        .into_iter()
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
                                        .map(|value| permission(Some(value)))
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
                                        .map(|value| permission(Some(value)))
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
                                .map(|value| permission(Some(value)))
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
}
