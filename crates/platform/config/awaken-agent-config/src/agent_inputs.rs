use awaken_resource_contract::InputBinding;
use serde::{Deserialize, Serialize};

/// An Agent's authored default Environment and Resource inputs. Workspace is
/// the repository aggregate key; this value contains only Agent-local intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentInputConfig {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<AgentEnvironmentBinding>,
    #[cfg_attr(feature = "schema", schemars(with = "Vec<InputBindingSchema>"))]
    pub inputs: Vec<InputBinding>,
    pub revision: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentEnvironmentBinding {
    pub environment_id: String,
    pub revision: u64,
}

// OpenAPI projection only. The Resource contract remains independent of
// schemars and HTTP tooling.
#[cfg(feature = "schema")]
#[allow(dead_code, reason = "type-only JSON Schema projection")]
#[derive(schemars::JsonSchema)]
#[schemars(rename = "InputBinding")]
struct InputBindingSchema {
    binding_id: String,
    target: InputResourceIdSchema,
    mount_path: String,
    access: ResourceAccessSchema,
    instructions: Option<String>,
}

#[cfg(feature = "schema")]
#[allow(dead_code, reason = "type-only JSON Schema projection")]
#[derive(schemars::JsonSchema)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
#[schemars(rename = "InputResourceId")]
enum InputResourceIdSchema {
    File(String),
    MemoryStore(String),
    Repository(String),
}

#[cfg(feature = "schema")]
#[allow(dead_code, reason = "type-only JSON Schema projection")]
#[derive(schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "ResourceAccess")]
enum ResourceAccessSchema {
    ReadOnly,
    ReadWrite,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_inputs_accept_only_the_canonical_closed_shape() {
        // Grammar partition: canonical fields round-trip; the removed legacy
        // resources/version grammar and unknown future fields both fail closed.
        let canonical = AgentInputConfig {
            agent_id: "agent-a".into(),
            environment: None,
            inputs: Vec::new(),
            revision: 1,
        };
        let encoded = serde_json::to_value(&canonical).unwrap();
        assert_eq!(
            serde_json::from_value::<AgentInputConfig>(encoded).unwrap(),
            canonical
        );
        assert!(
            serde_json::from_value::<AgentInputConfig>(serde_json::json!({
                "agent_id":"agent-a", "resources":[], "version":1
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<AgentInputConfig>(serde_json::json!({
                "agent_id":"agent-a", "inputs":[], "revision":1, "extra":true
            }))
            .is_err()
        );
    }
}
