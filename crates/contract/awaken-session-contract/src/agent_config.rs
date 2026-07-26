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
