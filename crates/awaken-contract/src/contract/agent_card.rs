//! A2A Agent Card — describes a remote agent's capabilities for discovery.
//!
//! Fetched from `{base_url}/.well-known/agent.json` per the A2A protocol.

use serde::{Deserialize, Serialize};

/// A2A Agent Card — describes a remote agent's capabilities.
///
/// Fetched from `{base_url}/.well-known/agent.json` during agent discovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCard {
    /// Agent identifier.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Description of what this agent does.
    #[serde(default)]
    pub description: String,
    /// Supported capabilities/skills.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Base URL for this agent's A2A endpoint.
    pub url: String,
    /// Authentication requirements.
    #[serde(default)]
    pub auth: Option<AgentCardAuth>,
}

/// Authentication requirement declared in an agent card.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentCardAuth {
    /// Bearer token authentication.
    Bearer,
    /// API key in a custom header.
    ApiKey {
        /// Header name for the API key.
        header: String,
    },
    /// No authentication required.
    None,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_card_serde_roundtrip() {
        let card = AgentCard {
            id: "remote-coder".into(),
            name: "Remote Coder".into(),
            description: "A remote coding agent".into(),
            capabilities: vec!["code".into(), "review".into()],
            url: "https://remote.example.com".into(),
            auth: Some(AgentCardAuth::Bearer),
        };
        let json = serde_json::to_string(&card).unwrap();
        let parsed: AgentCard = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "remote-coder");
        assert_eq!(parsed.name, "Remote Coder");
        assert_eq!(parsed.capabilities.len(), 2);
        assert!(matches!(parsed.auth, Some(AgentCardAuth::Bearer)));
    }

    #[test]
    fn agent_card_minimal_deserialize() {
        let json = json!({
            "id": "agent-1",
            "name": "Agent One",
            "url": "https://example.com"
        });
        let card: AgentCard = serde_json::from_value(json).unwrap();
        assert_eq!(card.id, "agent-1");
        assert!(card.description.is_empty());
        assert!(card.capabilities.is_empty());
        assert!(card.auth.is_none());
    }

    #[test]
    fn agent_card_auth_api_key() {
        let json = json!({
            "id": "agent-2",
            "name": "Agent Two",
            "url": "https://example.com",
            "auth": {"type": "api_key", "header": "X-API-Key"}
        });
        let card: AgentCard = serde_json::from_value(json).unwrap();
        match card.auth {
            Some(AgentCardAuth::ApiKey { header }) => assert_eq!(header, "X-API-Key"),
            other => panic!("expected ApiKey, got: {other:?}"),
        }
    }

    #[test]
    fn agent_card_auth_none_variant() {
        let json = json!({
            "id": "agent-3",
            "name": "Agent Three",
            "url": "https://example.com",
            "auth": {"type": "none"}
        });
        let card: AgentCard = serde_json::from_value(json).unwrap();
        assert!(matches!(card.auth, Some(AgentCardAuth::None)));
    }
}
