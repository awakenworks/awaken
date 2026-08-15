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

/// Admit an execution request against the published tool policy. The returned
/// requirement is never stronger than the authored one: asking when automatic
/// execution was allowed is a safe narrowing, while skipping a required ask is
/// rejected.
#[must_use]
pub const fn select_tool_policy(
    registered: bool,
    exact_tool_match: bool,
    authored: ToolExecutionPolicy,
    requested: ToolPermissionRequirement,
) -> Option<ToolExecutionPolicy> {
    if !registered || !exact_tool_match || !authored.enabled {
        return None;
    }
    if matches!(authored.permission, ToolPermissionRequirement::AlwaysAsk)
        && matches!(requested, ToolPermissionRequirement::AlwaysAllow)
    {
        return None;
    }
    Some(ToolExecutionPolicy {
        enabled: true,
        permission: requested,
    })
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

    /// Select a policy for a call only after the executable registry supplies
    /// its canonical tool name. Missing registration and alias/name mismatch
    /// fail closed before the default toolset policy can grant authority.
    #[must_use]
    pub fn policy_for_registered_call(
        &self,
        registered_name: Option<&str>,
        requested_name: &str,
        requested_permission: ToolPermissionRequirement,
    ) -> Option<ToolExecutionPolicy> {
        select_tool_policy(
            registered_name.is_some(),
            registered_name == Some(requested_name),
            self.policy_for(requested_name),
            requested_permission,
        )
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    fn symbolic_permission(value: bool) -> ToolPermissionRequirement {
        if value {
            ToolPermissionRequirement::AlwaysAllow
        } else {
            ToolPermissionRequirement::AlwaysAsk
        }
    }

    #[kani::proof]
    fn tool_policy_selector_fails_closed_for_unregistered_widened_or_mismatched_calls() {
        let registered: bool = kani::any();
        let exact_tool_match: bool = kani::any();
        let enabled: bool = kani::any();
        let authored_permission = symbolic_permission(kani::any());
        let requested = symbolic_permission(kani::any());
        let selected = select_tool_policy(
            registered,
            exact_tool_match,
            ToolExecutionPolicy {
                enabled,
                permission: authored_permission,
            },
            requested,
        );
        let widened = authored_permission == ToolPermissionRequirement::AlwaysAsk
            && requested == ToolPermissionRequirement::AlwaysAllow;
        let expected = registered && exact_tool_match && enabled && !widened;

        assert_eq!(selected.is_some(), expected);
        if let Some(policy) = selected {
            assert!(policy.enabled);
            assert_eq!(policy.permission, requested);
            assert!(!widened);
        }
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

    #[test]
    fn registered_tool_policy_selector_is_exact_and_non_widening() {
        let policy = ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy {
                enabled: true,
                permission: ToolPermissionRequirement::AlwaysAsk,
            },
            overrides: Vec::new(),
        };
        assert!(
            policy
                .policy_for_registered_call(
                    Some("read"),
                    "read",
                    ToolPermissionRequirement::AlwaysAsk,
                )
                .is_some()
        );
        assert!(
            policy
                .policy_for_registered_call(None, "read", ToolPermissionRequirement::AlwaysAsk,)
                .is_none(),
            "unregistered"
        );
        assert!(
            policy
                .policy_for_registered_call(
                    Some("write"),
                    "read",
                    ToolPermissionRequirement::AlwaysAsk,
                )
                .is_none(),
            "mismatched identity"
        );
        assert!(
            policy
                .policy_for_registered_call(
                    Some("read"),
                    "read",
                    ToolPermissionRequirement::AlwaysAllow,
                )
                .is_none(),
            "authority widening"
        );
    }
}
