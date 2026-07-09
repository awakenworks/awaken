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
    /// Normalized `BetaManagedAgentsModelConfig` (`{id, speed?}`).
    pub model: Value,
    pub system: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub mcp_servers: Vec<Value>,
    pub skills: Vec<Value>,
    pub tools: Vec<Value>,
    pub multiagent: Option<Value>,
    pub version: u64,
}
