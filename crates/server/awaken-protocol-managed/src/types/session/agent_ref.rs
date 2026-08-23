//! Managed Session Agent references and per-Session overrides.

use awaken_session_contract::AgentTool;
use serde::Deserialize;

use super::ModelConfig;
use crate::types::agent::{AgentMcpServer, AgentSkill};

/// The resolved `model` axis of an `agent_with_overrides` session reference.
#[derive(Debug, Clone)]
pub enum ModelOverride {
    Absent,
    Set(ModelConfig),
}

/// The two exact object forms accepted by the Managed SDK. The discriminator is
/// mandatory and closed; plain references cannot accidentally carry overrides.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentRefObject {
    Agent {
        id: String,
        #[serde(default)]
        version: Option<u64>,
    },
    AgentWithOverrides {
        id: String,
        #[serde(default)]
        mcp_servers: Option<Vec<AgentMcpServer>>,
        #[serde(
            default,
            deserialize_with = "crate::types::presence::optional_non_null"
        )]
        model: Option<crate::types::agent::ModelInput>,
        #[serde(default)]
        skills: Option<Vec<AgentSkill>>,
        #[serde(default, deserialize_with = "crate::types::presence::double_option")]
        system: Option<Option<String>>,
        #[serde(default)]
        tools: Option<Vec<AgentTool>>,
        #[serde(default)]
        version: Option<u64>,
    },
}

impl AgentRefObject {
    fn id(&self) -> &str {
        match self {
            Self::Agent { id, .. } | Self::AgentWithOverrides { id, .. } => id,
        }
    }

    fn version(&self) -> Option<u64> {
        match self {
            Self::Agent { version, .. } | Self::AgentWithOverrides { version, .. } => *version,
        }
    }
}

/// `agent` in a create-session request — the SDK's
/// `string | {id, type:'agent', version?} | {id, type:'agent_with_overrides',
/// version?, model?, system?, tools?, ...}` (`BetaManagedAgentsAgentParams`).
/// Untagged: a JSON string is [`AgentRef::Id`]; a JSON object is [`AgentRef::Object`],
/// whose `type` then selects plain-reference vs. overrides. Per-session runtime
/// selection still travels in the session `metadata` bag (see
/// [`super::SessionCreateParams`]); the model rides the official override object.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum AgentRef {
    Id(String),
    Object(Box<AgentRefObject>),
}

impl AgentRef {
    pub fn id(&self) -> &str {
        match self {
            AgentRef::Id(id) => id,
            AgentRef::Object(object) => object.id(),
        }
    }

    /// The Agent version the client pinned, if any (`None` = latest).
    pub fn version(&self) -> Option<u64> {
        match self {
            AgentRef::Id(_) => None,
            AgentRef::Object(object) => object.version(),
        }
    }

    /// The single-Session model override. Only an `agent_with_overrides` object
    /// carries one; every other form reports [`ModelOverride::Absent`].
    pub fn model_override(&self) -> ModelOverride {
        match self {
            AgentRef::Object(object) => match object.as_ref() {
                AgentRefObject::AgentWithOverrides {
                    model: Some(input), ..
                } => ModelOverride::Set(input.clone().into_config().into_session_override()),
                _ => ModelOverride::Absent,
            },
            _ => ModelOverride::Absent,
        }
    }

    pub fn mcp_servers_override(&self) -> Option<&[AgentMcpServer]> {
        match self {
            AgentRef::Object(object) => match object.as_ref() {
                AgentRefObject::AgentWithOverrides {
                    mcp_servers: Some(servers),
                    ..
                } => Some(servers),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn skills_override(&self) -> Option<&[AgentSkill]> {
        match self {
            AgentRef::Object(object) => match object.as_ref() {
                AgentRefObject::AgentWithOverrides {
                    skills: Some(skills),
                    ..
                } => Some(skills),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn tools_override(&self) -> Option<&[AgentTool]> {
        match self {
            AgentRef::Object(object) => match object.as_ref() {
                AgentRefObject::AgentWithOverrides {
                    tools: Some(tools), ..
                } => Some(tools),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn system_override(&self) -> Option<Option<&str>> {
        match self {
            AgentRef::Object(object) => match object.as_ref() {
                AgentRefObject::AgentWithOverrides { system, .. } => {
                    system.as_ref().map(|value| value.as_deref())
                }
                _ => None,
            },
            _ => None,
        }
    }

    pub(super) fn validate_sdk_limits(&self) -> Result<(), String> {
        if self.version() == Some(0) {
            return Err("agent version must be greater than or equal to 1".into());
        }
        if self
            .system_override()
            .flatten()
            .is_some_and(|system| system.chars().count() > 100_000)
        {
            return Err("agent system override supports at most 100000 characters".into());
        }
        if self
            .mcp_servers_override()
            .is_some_and(|items| items.len() > 20)
        {
            return Err("agent mcp_servers override supports at most 20 entries".into());
        }
        if self.skills_override().is_some_and(|items| items.len() > 20) {
            return Err("agent skills override supports at most 20 entries".into());
        }
        if self.tools_override().is_some_and(|items| items.len() > 128) {
            return Err("agent tools override supports at most 128 entries".into());
        }
        if let Some(tools) = self.tools_override() {
            awaken_session_contract::validate_agent_tools(tools)?;
        }
        Ok(())
    }
}
