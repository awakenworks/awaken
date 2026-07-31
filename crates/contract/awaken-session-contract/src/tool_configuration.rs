//! Mutable neutral tool configuration owned by one Session.

use awaken_agent_contract::{ClientToolDescriptor, ToolsetPolicy};
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
}
