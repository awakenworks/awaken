//! Neutral, resolved tool-family policy shared by authoring and execution.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolsetPolicy {
    pub source: ToolsetSource,
    pub default: ToolExecutionPolicy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<ToolPolicyOverride>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolsetSource {
    Agent,
    Mcp { server_name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExecutionPolicy {
    pub enabled: bool,
    pub permission: ToolPermissionRequirement,
}

impl Default for ToolExecutionPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            permission: ToolPermissionRequirement::AlwaysAllow,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPermissionRequirement {
    AlwaysAllow,
    AlwaysAsk,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPolicyOverride {
    pub name: String,
    pub policy: ToolExecutionPolicy,
}

impl ToolsetPolicy {
    #[must_use]
    pub fn policy_for(&self, name: &str) -> ToolExecutionPolicy {
        self.overrides
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.policy)
            .unwrap_or(self.default)
    }
}
