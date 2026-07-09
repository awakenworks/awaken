//! Wire types for the `agents` resource (`beta.agents.*`): `BetaManagedAgentsAgent`
//! — a reusable, versioned agent configuration (model + system + tools +
//! mcp_servers + skills + multiagent topology) a session instantiates by id.
//!
//! Pure serde shapes. The composite sub-fields the SDK models as their own union
//! shapes (the normalized `model` config, tools, mcp_servers, skills, multiagent)
//! stay opaque `Value`s. The store, the version history, and both projections
//! (registry record and config-plane view) live in `routes::agents_registry`.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::types::ModelConfig;

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
    pub mcp_servers: Vec<Value>,
    pub skills: Vec<Value>,
    pub tools: Vec<Value>,
    pub multiagent: Option<Value>,
    pub version: u64,
}
