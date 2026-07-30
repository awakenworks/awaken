//! Wire types for the `agents` resource (`beta.agents.*`): `BetaManagedAgentsAgent`
//! — a reusable, versioned agent configuration (model + system + tools +
//! mcp_servers + skills + multiagent topology) a session instantiates by id.
//!
//! Pure serde shapes. SDK unions are decoded here into tagged Rust enums before
//! the registry normalizes them into its persisted JSON projection.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use awaken_session_contract::{
    AgentTool, AgentToolConfig as ToolConfig, AgentToolDefaultConfig as ToolDefaultConfig,
    AgentToolPermissionPolicy as PermissionPolicy, CustomToolInputSchema, ObjectSchemaKind,
};

use crate::types::{ModelConfig, ModelConfigParams};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UrlMcpServer {
    pub name: String,
    pub url: String,
    #[serde(rename = "type")]
    pub kind: UrlMcpServerKind,
    #[serde(default, skip_serializing_if = "is_false")]
    pub prompts_as_skills: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
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

impl AgentSkill {
    #[must_use]
    pub fn into_binding(self) -> awaken_agent_contract::AgentSkillBinding {
        let (kind, skill_id, version) = match self {
            Self::Anthropic { skill_id, version } => (
                awaken_agent_contract::AgentSkillKind::Anthropic,
                skill_id,
                version,
            ),
            Self::Custom { skill_id, version } => (
                awaken_agent_contract::AgentSkillKind::Custom,
                skill_id,
                version,
            ),
        };
        awaken_agent_contract::AgentSkillBinding {
            kind,
            skill_id,
            version: version.unwrap_or_else(|| "latest".into()),
        }
    }

    #[must_use]
    pub fn from_binding(binding: awaken_agent_contract::AgentSkillBinding) -> Self {
        match binding.kind {
            awaken_agent_contract::AgentSkillKind::Anthropic => Self::Anthropic {
                skill_id: binding.skill_id,
                version: Some(binding.version),
            },
            awaken_agent_contract::AgentSkillKind::Custom => Self::Custom {
                skill_id: binding.skill_id,
                version: Some(binding.version),
            },
        }
    }
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
    Config(ModelConfigParams),
}

impl ModelInput {
    pub fn into_config(self) -> ModelConfigParams {
        match self {
            ModelInput::Id(id) => ModelConfigParams::new(id),
            ModelInput::Config(config) => config,
        }
    }
}

/// `AgentCreateParams` — the `POST /v1/agents` body. Every statically-known SDK
/// union is decoded before the repository is called.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct AgentUpdateParams {
    #[serde(default)]
    pub version: Option<u64>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub model: Option<ModelInput>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub description: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub system: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub metadata: Option<Option<BTreeMap<String, Option<String>>>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub mcp_servers: Option<Option<Vec<UrlMcpServer>>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub skills: Option<Option<Vec<AgentSkill>>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub tools: Option<Option<Vec<AgentTool>>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub multiagent: Option<Option<MultiagentConfig>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentRetrieveParams {
    #[serde(default)]
    pub version: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentListParams {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub page: Option<String>,
    #[serde(default, rename = "created_at[gte]")]
    pub created_at_gte: Option<String>,
    #[serde(default, rename = "created_at[lte]")]
    pub created_at_lte: Option<String>,
    #[serde(default)]
    pub include_archived: bool,
}

impl AgentListParams {
    #[must_use]
    pub fn page_query(&self) -> crate::types::PageQuery {
        crate::types::PageQuery {
            limit: self.limit,
            page: self.page.clone(),
        }
    }
}

/// `BetaManagedAgentsAgentReference` — how an agent is *referenced* (by a
/// deployment, a session): `{ id, type: "agent", version }`. The single typed form
/// of the normalized reference; the deserialize-only input form a client may send
/// (a bare id string or `{id, version?}`) is [`super::session::AgentRef`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentReference {
    pub id: String,
    #[serde(rename = "type", skip_deserializing, default = "agent_reference_type")]
    pub object_type: &'static str,
    pub version: u64,
}

fn agent_reference_type() -> &'static str {
    "agent"
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
    pub disabled_at: Option<String>,
    pub status: AgentStatus,
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

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Published,
    Disabled,
    Archived,
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
            "model": {
                "id": "acp:codex/model-1",
                "x_awaken": {
                    "acp": {
                        "mode": "plan",
                        "options": {"reasoning_effort": "high"}
                    }
                }
            },
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
        let model = parsed.model.clone().into_config();
        let acp = model
            .x_awaken
            .and_then(|extension| extension.acp)
            .expect("namespaced ACP configuration");
        assert_eq!(acp.mode.as_deref(), Some("plan"));
        assert_eq!(acp.options["reasoning_effort"], "high");
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
