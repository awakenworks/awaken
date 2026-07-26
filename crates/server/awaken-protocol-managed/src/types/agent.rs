//! Wire types for the `agents` resource (`beta.agents.*`): `BetaManagedAgentsAgent`
//! — a reusable, versioned agent configuration (model + system + tools +
//! mcp_servers + skills + multiagent topology) a session instantiates by id.
//!
//! Pure serde shapes. SDK unions are decoded here into tagged Rust enums before
//! the registry normalizes them into its persisted JSON projection.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::ModelConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UrlMcpServer {
    pub name: String,
    pub url: String,
    #[serde(rename = "type")]
    pub kind: UrlMcpServerKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum UrlMcpServerKind {
    #[serde(rename = "url")]
    Url,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentSkill {
    Anthropic {
        skill_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
    Custom {
        skill_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PermissionPolicy {
    AlwaysAllow,
    AlwaysAsk,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_policy: Option<PermissionPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDefaultConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_policy: Option<PermissionPolicy>,
}

/// JSON Schema is intentionally extensible: schema keywords and property names
/// are defined by the caller, not by the Managed Agents protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomToolInputSchema {
    #[serde(rename = "type")]
    pub kind: ObjectSchemaKind,
    #[serde(flatten)]
    pub keywords: BTreeMap<String, Value>,
}

impl CustomToolInputSchema {
    pub fn from_value(value: Value) -> Result<Self, String> {
        serde_json::from_value(value).map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ObjectSchemaKind {
    #[serde(rename = "object")]
    Object,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentTool {
    #[serde(rename = "agent_toolset_20260401")]
    AgentToolset20260401 {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        configs: Vec<ToolConfig>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_config: Option<ToolDefaultConfig>,
    },
    McpToolset {
        mcp_server_name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        configs: Vec<ToolConfig>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_config: Option<ToolDefaultConfig>,
    },
    Custom {
        name: String,
        description: String,
        input_schema: CustomToolInputSchema,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MultiagentRosterEntry {
    Id(String),
    Reference(AgentRosterReference),
    SelfReference(SelfRosterReference),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRosterReference {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: AgentRosterReferenceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum AgentRosterReferenceKind {
    #[serde(rename = "agent")]
    Agent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelfRosterReference {
    #[serde(rename = "type")]
    pub kind: SelfRosterReferenceKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SelfRosterReferenceKind {
    #[serde(rename = "self")]
    SelfReference,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MultiagentConfig {
    Coordinator { agents: Vec<MultiagentRosterEntry> },
}

/// A client's `model` input: a bare id string or a full `{id, speed?}` config
/// (the SDK's `string | BetaManagedAgentsModelConfig`). Normalized to the shared
/// [`ModelConfig`] via [`ModelInput::into_config`].
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ModelInput {
    Id(String),
    Config(ModelConfig),
}

impl ModelInput {
    pub fn into_config(self) -> ModelConfig {
        match self {
            ModelInput::Id(id) => ModelConfig::new(id),
            ModelInput::Config(config) => config,
        }
    }
}

/// `AgentCreateParams` — the `POST /v1/agents` body. Every statically-known SDK
/// union is decoded before the repository is called.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentCreateParams {
    pub name: String,
    pub model: ModelInput,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub mcp_servers: Vec<UrlMcpServer>,
    #[serde(default)]
    pub skills: Vec<AgentSkill>,
    #[serde(default)]
    pub tools: Vec<AgentTool>,
    #[serde(default)]
    pub multiagent: Option<MultiagentConfig>,
}

/// `AgentUpdateParams` — a partial update under optimistic concurrency: `version`
/// must match the agent's current version. Other fields replace when present.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentUpdateParams {
    pub version: u64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub model: Option<ModelInput>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub metadata: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub mcp_servers: Option<Vec<UrlMcpServer>>,
    #[serde(default)]
    pub skills: Option<Vec<AgentSkill>>,
    #[serde(default)]
    pub tools: Option<Vec<AgentTool>>,
    #[serde(default)]
    pub multiagent: Option<MultiagentConfig>,
}

/// `BetaManagedAgentsAgentReference` — how an agent is *referenced* (by a
/// deployment, a session): `{ id, type: "agent", version }`. The single typed form
/// of the normalized reference; the deserialize-only input form a client may send
/// (a bare id string or `{id, version?}`) is [`super::session::AgentRef`].
#[derive(Debug, Clone, Serialize)]
pub struct AgentReference {
    pub id: String,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub version: u64,
}

impl AgentReference {
    pub fn new(id: impl Into<String>, version: u64) -> Self {
        Self {
            id: id.into(),
            object_type: "agent",
            version,
        }
    }

    /// Normalize a client's input reference ([`super::session::AgentRef`], a bare id,
    /// `{id, version?}`, or an `agent_with_overrides` object) into the wire
    /// reference — `version` defaults to 1. The reference identifies the base agent
    /// and version; any per-session overrides are applied separately.
    pub fn from_input(input: &super::session::AgentRef) -> Self {
        Self::new(input.id(), input.version().unwrap_or(1) as u64)
    }
}

/// `BetaManagedAgentsAgent` — an agent configuration at a given version.
#[derive(Debug, Clone, Serialize)]
pub struct Agent {
    pub id: String,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub name: String,
    pub description: Option<String>,
    pub model: ModelConfig,
    pub system: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub mcp_servers: Vec<UrlMcpServer>,
    pub skills: Vec<AgentSkill>,
    pub tools: Vec<AgentTool>,
    pub multiagent: Option<MultiagentConfig>,
    pub version: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn managed_agent_composites_follow_the_sdk_tagged_unions() {
        // Causal graph:
        // official SDK JSON -> tagged Managed DTO -> config-domain normalization.
        //
        // Decision table:
        // | input                                      | admission |
        // | every known discriminator + exact fields   | accept    |
        // | unknown discriminator                      | reject    |
        // | known discriminator + misspelled field     | reject    |
        // | custom input_schema extension keyword      | preserve  |
        let valid = json!({
            "name": "typed",
            "model": "model-1",
            "mcp_servers": [{"type":"url","name":"docs","url":"https://mcp.test"}],
            "skills": [{"type":"custom","skill_id":"skill_1","version":"2"}],
            "tools": [
                {"type":"agent_toolset_20260401","configs":[{"name":"bash","enabled":false}]},
                {"type":"mcp_toolset","mcp_server_name":"docs"},
                {"type":"custom","name":"lookup","description":"Lookup", "input_schema":{
                    "type":"object","properties":{"id":{"type":"string"}},"additionalProperties":false
                }}
            ],
            "multiagent": {"type":"coordinator","agents":["worker",{"type":"self"}]}
        });
        let parsed: AgentCreateParams = serde_json::from_value(valid).expect("SDK union parses");
        let AgentTool::Custom { input_schema, .. } = &parsed.tools[2] else {
            panic!("custom tool retained its variant")
        };
        assert_eq!(input_schema.keywords["additionalProperties"], false);

        for invalid in [
            json!({"name":"x","model":"m","skills":[{"type":"unknown","skill_id":"s"}]}),
            json!({"name":"x","model":"m","mcp_servers":[{"type":"url","name":"s","uri":"https://x"}]}),
            json!({"name":"x","model":"m","tools":[{"type":"mcp_toolset","mcp_server":"s"}]}),
        ] {
            assert!(serde_json::from_value::<AgentCreateParams>(invalid).is_err());
        }
    }
}
