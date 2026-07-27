//! The config-plane agent-projection port (ADR-0043).

/// The config-plane projection of an agent: the runtime-authoritative fields the
/// managed wire shows. A neutral view so `/v1/agents` presents an agent authored on
/// the config plane (`/v1/config/agents`) as a *projection* of that single truth
/// rather than a second copy — the "retreat to projection" direction (ADR-0043).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMcpServerView {
    pub name: String,
    pub url: String,
    /// Exact credential source selected by the published Agent snapshot.
    pub credential_source_id: Option<String>,
    /// Published source revision. It is checked again at materialization so a
    /// rotated or replaced source fails closed instead of silently drifting.
    pub credential_revision: Option<u64>,
}

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Exact model-visible contract for a tool executed by the protocol client.
/// JSON Schema remains open by definition; identity, description, and ownership
/// are otherwise typed and frozen in the published Agent snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentClientToolView {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

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
    /// Serialization-only reference to a server-executed tool resolved by the
    /// configuration plane. Managed writes must use the complete `Custom`
    /// shape, so this variant cannot weaken client-tool admission.
    #[serde(rename = "custom", skip_deserializing)]
    CustomReference { name: String },
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
                AgentTool::Custom { .. } | AgentTool::CustomReference { .. } => return None,
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

pub struct AgentConfigView {
    pub model: Option<String>,
    pub system: Option<String>,
    pub tool_ids: Vec<String>,
    /// Resolved availability/confirmation policies frozen in the publication.
    pub toolsets: Vec<awaken_agent_contract::ToolsetPolicy>,
    pub client_tools: Vec<AgentClientToolView>,
    /// Direct MCP servers inherited by Sessions of this published Agent.
    pub mcp_servers: Vec<AgentMcpServerView>,
    /// The delivered Skills selected by this Agent. Empty is an intentional empty
    /// selection for newly published configs, not "all global skills".
    pub skill_ids: Vec<String>,
    /// Published Agent ids this Agent may invoke through `agent_run`.
    pub delegate_ids: Vec<String>,
    /// Resources bound to the published Agent. The runtime mounts these at Session
    /// preparation; protocol projections expose the same effective inputs.
    pub resources: Vec<awaken_resource_contract::InputBinding>,
    /// Exact Agent-default Environment binding. `None` means callers must supply
    /// an Environment explicitly; it never means `env_local`.
    pub environment: Option<AgentEnvironmentBindingView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEnvironmentBindingView {
    pub environment_id: String,
    pub revision: u64,
}

/// A source of config-plane agent projections. A **port**: the host implements it
/// over its `ConfigService` (the managed crate cannot depend on the host), so the
/// managed adapter reads the neutral config truth without naming it.
pub trait AgentConfigSource: Send + Sync {
    /// The config-plane view of `agent_id` installed in `workspace_id`. There is no
    /// scope-free fallback: a projection missing its trusted Workspace must fail
    /// closed instead of searching another tenant's installed catalog.
    fn agent_view_in(&self, workspace_id: &str, agent_id: &str) -> Option<AgentConfigView>;

    /// Whether this is a known aggregate that must not start new execution
    /// (currently: archived). A missing view alone may still denote an unmanaged
    /// compatibility id, so lifecycle denial needs a distinct signal.
    fn agent_unavailable_in(&self, _workspace_id: &str, _agent_id: &str) -> bool {
        false
    }
}
