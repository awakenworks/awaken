//! Wire types for the `agents` resource (`beta.agents.*`): `BetaManagedAgentsAgent`
//! — a reusable, versioned agent configuration (model + system + tools +
//! mcp_servers + skills + multiagent topology) a session instantiates by id.
//!
//! Pure serde shapes. SDK unions are decoded here into tagged Rust enums before
//! the registry normalizes them into its persisted JSON projection.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use awaken_session_contract::AgentTool;

use crate::types::{ModelConfig, ModelConfigParams};

/// The one Managed MCP wire shape shared by Agent and Session requests:
/// `{name, type:"url", url}`. Internal sandbox-stdio bindings never become a
/// second public transport variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMcpServer {
    pub name: String,
    pub url: String,
}

impl AgentMcpServer {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Serialize for AgentMcpServer {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut value = serializer.serialize_struct("AgentMcpServer", 3)?;
        value.serialize_field("name", &self.name)?;
        value.serialize_field("type", "url")?;
        value.serialize_field("url", &self.url)?;
        value.end()
    }
}

impl<'de> Deserialize<'de> for AgentMcpServer {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            name: String,
            url: String,
            #[serde(rename = "type")]
            kind: String,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.kind != "url" {
            return Err(serde::de::Error::custom("MCP server type must be `url`"));
        }
        Ok(Self {
            name: wire.name,
            url: wire.url,
        })
    }
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
    Advisor(AdvisorRosterEntry),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdvisorRosterEntry {
    pub model: String,
    #[serde(rename = "type")]
    pub kind: AdvisorRosterEntryKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum AdvisorRosterEntryKind {
    #[serde(rename = "advisor")]
    Advisor,
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
    pub mcp_servers: Vec<AgentMcpServer>,
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
    pub mcp_servers: Option<Option<Vec<AgentMcpServer>>>,
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
    pub created_at: String,
    pub updated_at: String,
    pub name: String,
    pub description: Option<String>,
    pub model: ModelConfig,
    pub system: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub mcp_servers: Vec<AgentMcpServer>,
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
        // | non-standard extension field               | reject    |
        let valid = json!({
            "name": "typed",
            "model": "model-1;executor=acp:codex",
            "mcp_servers": [
                {"type":"url","name":"docs","url":"https://mcp.test"}
            ],
            "skills": [{"type":"custom","skill_id":"skill_1","version":"2"}],
            "tools": [
                {"type":"agent_toolset_20260401","configs":[{"name":"bash","enabled":false}]},
                {"type":"mcp_toolset","mcp_server_name":"docs"},
                {"type":"custom","name":"lookup","description":"Lookup", "input_schema":{
                    "type":"object","properties":{"id":{"type":"string"}},"additionalProperties":false
                }}
            ],
            "multiagent": {"type":"coordinator","agents":[
                "worker", {"type":"self"}, {"type":"advisor","model":"claude-opus-4-6"}
            ]},
        });
        let parsed: AgentCreateParams = serde_json::from_value(valid).expect("SDK union parses");
        let AgentTool::Custom { input_schema, .. } = &parsed.tools[2] else {
            panic!("custom tool retained its variant")
        };
        assert_eq!(input_schema.keywords["additionalProperties"], false);
        assert_eq!(parsed.mcp_servers[0].name, "docs");
        assert!(matches!(
            parsed.multiagent,
            Some(MultiagentConfig::Coordinator { ref agents })
                if matches!(agents[2], MultiagentRosterEntry::Advisor(_))
        ));
        assert_eq!(
            serde_json::to_value(&parsed.mcp_servers[0]).unwrap()["type"],
            "url"
        );

        let update: AgentUpdateParams = serde_json::from_value(json!({
            "version": 3,
            "mcp_servers": [{
                "type": "url",
                "name": "docs",
                "url": "https://mcp.test"
            }]
        }))
        .expect("update reuses the one URL MCP DTO");
        assert!(matches!(
            update.mcp_servers,
            Some(Some(ref servers)) if servers[0].url == "https://mcp.test"
        ));

        for invalid in [
            json!({"name":"x","model":"m","skills":[{"type":"unknown","skill_id":"s"}]}),
            json!({"name":"x","model":"m","mcp_servers":[{"name":"s","url":"https://x"}]}),
            json!({"name":"x","model":"m","mcp_servers":[{"type":"url","name":"s","uri":"https://x"}]}),
            json!({"name":"x","model":"m","mcp_servers":[{"type":"sandbox_stdio","name":"s","command":"tool"}]}),
            json!({"name":"x","model":"m","mcp_servers":[{"type":"url","name":"s","url":"https://x","prompts_as_skills":true}]}),
            json!({"name":"x","model":"m","tools":[{"type":"mcp_toolset","mcp_server":"s"}]}),
            json!({"name":"x","model":"m","max_steps":40}),
        ] {
            assert!(serde_json::from_value::<AgentCreateParams>(invalid).is_err());
        }
    }

    #[test]
    fn agent_response_contains_only_the_sdk_fields() {
        // Cause/effect decision table: official fields serialize; historical
        // Awaken-only lifecycle fields (`status`, `disabled_at`) have no DTO owner
        // and therefore cannot appear in the response object.
        let value = serde_json::to_value(Agent {
            id: "agent_1".into(),
            object_type: "agent",
            archived_at: None,
            created_at: "2026-08-15T00:00:00Z".into(),
            updated_at: "2026-08-15T00:00:00Z".into(),
            name: "assistant".into(),
            description: None,
            model: ModelConfig::new("claude-sonnet-4-6"),
            system: None,
            metadata: BTreeMap::new(),
            mcp_servers: Vec::new(),
            skills: Vec::new(),
            tools: Vec::new(),
            multiagent: None,
            version: 1,
        })
        .unwrap();
        assert!(value.get("status").is_none());
        assert!(value.get("disabled_at").is_none());
        assert_eq!(value["type"], "agent");
    }
}
