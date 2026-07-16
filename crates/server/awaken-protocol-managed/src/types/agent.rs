//! Wire types for the `agents` resource (`beta.agents.*`): `BetaManagedAgentsAgent`
//! — a reusable, versioned agent configuration (model + system + tools +
//! mcp_servers + skills + multiagent topology) a session instantiates by id.
//!
//! Pure serde shapes. The composite sub-fields the SDK models as their own union
//! shapes (the normalized `model` config, tools, mcp_servers, skills, multiagent)
//! stay opaque `Value`s. The store, the version history, and both projections
//! (registry record and config-plane view) live in `routes::agents_registry`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::ModelConfig;

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

/// `AgentCreateParams` — the `POST /v1/agents` body. The composite fields the SDK
/// models as unions (`mcp_servers`, `skills`, `tools`, `multiagent`) stay opaque
/// `Value`s.
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
    pub mcp_servers: Vec<Value>,
    #[serde(default)]
    pub skills: Vec<Value>,
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub multiagent: Option<Value>,
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
    pub mcp_servers: Option<Vec<Value>>,
    #[serde(default)]
    pub skills: Option<Vec<Value>>,
    #[serde(default)]
    pub tools: Option<Vec<Value>>,
    #[serde(default)]
    pub multiagent: Option<Value>,
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
    // Opaque SDK unions passed through verbatim (`mcp_servers` = MCP server defs,
    // `tools` = the built-in/custom/MCP tool union, `skills`, and the `multiagent`
    // coordinator roster) — reproducing each buys nothing this surface constructs.
    pub mcp_servers: Vec<Value>,
    pub skills: Vec<Value>,
    pub tools: Vec<Value>,
    pub multiagent: Option<Value>,
    pub version: u64,
}
