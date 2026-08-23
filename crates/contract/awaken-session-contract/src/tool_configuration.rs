//! Mutable neutral tool configuration owned by one Session.

use awaken_agent_contract::{
    ClientToolDescriptor, ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride,
    ToolsetPolicy, ToolsetSource,
};
use serde::{Deserialize, Serialize};

/// Current Session tool policy independent of any public protocol encoding.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionToolConfiguration {
    #[serde(default)]
    pub toolsets: Vec<ToolsetPolicy>,
    #[serde(default)]
    pub client_tools: Vec<ClientToolDescriptor>,
}

impl SessionToolConfiguration {
    /// Derive the neutral Session policy from the Runtime's advertised surface.
    /// Wire adapters may project this value but never reproduce the policy fold.
    #[must_use]
    pub fn from_capabilities(capabilities: &crate::AgentCapabilities) -> Self {
        let toolsets = (!capabilities.builtin_tools.is_empty())
            .then(|| ToolsetPolicy {
                source: ToolsetSource::Agent,
                default: ToolExecutionPolicy::default(),
                overrides: crate::agent_toolset_members()
                    .filter_map(|name| {
                        match capabilities
                            .builtin_tools
                            .iter()
                            .find(|tool| tool.name == name)
                        {
                            None => Some(ToolPolicyOverride::new(
                                name,
                                ToolExecutionPolicy {
                                    enabled: false,
                                    permission: ToolPermissionRequirement::AlwaysAllow,
                                },
                            )),
                            Some(tool) if tool.ask => Some(ToolPolicyOverride::new(
                                name,
                                ToolExecutionPolicy {
                                    enabled: true,
                                    permission: ToolPermissionRequirement::AlwaysAsk,
                                },
                            )),
                            Some(_) => None,
                        }
                    })
                    .collect(),
            })
            .into_iter()
            .collect();
        Self {
            toolsets,
            client_tools: capabilities
                .custom_tools
                .iter()
                .map(|tool| ClientToolDescriptor {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_tool_configuration_persists_neutral_policy_and_client_tools() {
        // Cause/effect decision table: R1 absent fields -> explicit empty policy;
        // R2 neutral toolsets/client descriptors -> exact round-trip; R3 a public
        // protocol-only field -> reject rather than persist a wire representation.
        assert_eq!(
            serde_json::from_str::<SessionToolConfiguration>("{}").unwrap(),
            SessionToolConfiguration::default()
        );
        let configuration = SessionToolConfiguration {
            toolsets: Vec::new(),
            client_tools: vec![ClientToolDescriptor {
                name: "client_tool".into(),
                description: "Client owned".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
        };
        let encoded = serde_json::to_vec(&configuration).unwrap();
        assert_eq!(
            serde_json::from_slice::<SessionToolConfiguration>(&encoded).unwrap(),
            configuration
        );
        assert!(
            serde_json::from_value::<SessionToolConfiguration>(serde_json::json!({
                "agent_toolset_20260401": []
            }))
            .is_err()
        );
    }

    #[test]
    fn runtime_capabilities_have_one_neutral_policy_fold() {
        // Cause/effect decision table: R1 no built-ins => no Agent toolset;
        // R2 registered auto-allowed built-in => default policy with no override;
        // R3 missing members => disabled overrides; R4 confirmation-gated member
        // => enabled/AlwaysAsk override; R5 custom tool => exact client descriptor.
        // Together the rules ensure every wire adapter projects one shared policy.
        let empty =
            SessionToolConfiguration::from_capabilities(&crate::AgentCapabilities::default());
        assert!(empty.toolsets.is_empty(), "R1");

        let capabilities = crate::AgentCapabilities {
            builtin_tools: vec![
                crate::BuiltinTool {
                    name: "bash".into(),
                    ask: false,
                },
                crate::BuiltinTool {
                    name: "read".into(),
                    ask: true,
                },
            ],
            custom_tools: vec![crate::CustomTool {
                name: "client".into(),
                description: "client-owned".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            ..Default::default()
        };
        let configuration = SessionToolConfiguration::from_capabilities(&capabilities);
        let policy = &configuration.toolsets[0];
        assert!(
            !policy.overrides.iter().any(|item| item.name == "bash"),
            "R2"
        );
        assert!(
            policy.overrides.iter().any(|item| {
                item.name == "read"
                    && item.policy.enabled
                    && item.policy.permission == ToolPermissionRequirement::AlwaysAsk
            }),
            "R4"
        );
        assert!(
            policy
                .overrides
                .iter()
                .any(|item| { item.name == "write" && !item.policy.enabled }),
            "R3"
        );
        assert_eq!(configuration.client_tools[0].name, "client", "R5");
    }
}
