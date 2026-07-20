//! Published Agent bindings consumed by execution adapters.
//!
//! Agent authoring owns the flexible external wire unions. Compilation
//! normalizes the executable subset into this small contract and stores it in the
//! resolved spec's config map. Runtime code consumes only this normalized form, so it
//! never needs to understand draft JSON or reach back into the control plane.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Reserved resolved-config section carrying executable Agent bindings.
pub const AGENT_BINDINGS_CONFIG_KEY: &str = "awaken.agent_bindings";

/// One direct HTTP MCP server inherited by Sessions of the published Agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMcpServerBinding {
    pub name: String,
    pub url: String,
}

/// The normalized, executable subset of Agent integration configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentBindings {
    #[serde(default)]
    pub mcp_servers: Vec<AgentMcpServerBinding>,
    #[serde(default)]
    pub skill_ids: Vec<String>,
}

impl AgentBindings {
    /// Decode bindings from a published resolved config. `None` distinguishes an
    /// older publication (which keeps the legacy global-skill behavior) from a new
    /// publication that intentionally selected no skills.
    #[must_use]
    pub fn from_config(config: &BTreeMap<String, serde_json::Value>) -> Option<Self> {
        serde_json::from_value(config.get(AGENT_BINDINGS_CONFIG_KEY)?.clone()).ok()
    }

    /// Stamp normalized bindings into the resolved config, overriding any authored
    /// value under the reserved key.
    pub fn insert_into(self, config: &mut BTreeMap<String, serde_json::Value>) {
        config.insert(
            AGENT_BINDINGS_CONFIG_KEY.to_string(),
            serde_json::to_value(self).expect("AgentBindings always serializes"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_binding_is_distinct_from_legacy_absence() {
        let mut config = BTreeMap::new();
        assert_eq!(AgentBindings::from_config(&config), None);
        AgentBindings::default().insert_into(&mut config);
        assert_eq!(
            AgentBindings::from_config(&config),
            Some(AgentBindings::default())
        );
    }
}
