//! Neutral, resolved tool-family policy shared by authoring and execution.

use serde::{Deserialize, Serialize};

/// Model-visible tool executed by the protocol client rather than by a Host
/// executor. Identity, description, and schema are frozen with an executable
/// Agent publication and may be copied into a Session's neutral tool policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_tool_descriptor_round_trips_without_execution_ownership() {
        // Cause/effect decision table: R1 name+description+open JSON Schema are
        // preserved exactly; R2 no executor, permission, or backend field can be
        // authored because those concerns are absent from the canonical value.
        let descriptor = ClientToolDescriptor {
            name: "client_search".into(),
            description: "Search in the client".into(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let encoded = serde_json::to_vec(&descriptor).unwrap();
        assert_eq!(
            serde_json::from_slice::<ClientToolDescriptor>(&encoded).unwrap(),
            descriptor
        );
        let object = serde_json::to_value(descriptor).unwrap();
        assert_eq!(object.as_object().unwrap().len(), 3);
    }
}
