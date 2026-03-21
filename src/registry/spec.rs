//! Serializable agent definition — pure data, no trait objects.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Serializable agent definition referencing registries by ID.
///
/// Can be saved to JSON, loaded from config files, or transmitted over the network.
/// Resolved at runtime via [`super::resolve::resolve`] into a [`super::resolve::ResolvedRun`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    /// Unique agent identifier.
    pub id: String,
    /// ModelRegistry ID — resolved to a [`super::traits::ModelEntry`].
    pub model: String,
    /// System prompt sent to the LLM.
    pub system_prompt: String,
    /// Maximum inference rounds before the agent stops.
    #[serde(default = "default_max_rounds")]
    pub max_rounds: usize,
    /// PluginRegistry IDs — resolved at build time.
    #[serde(default)]
    pub plugin_ids: Vec<String>,
    /// Allowed tool IDs (whitelist). `None` = all tools.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Excluded tool IDs (blacklist). Applied after `allowed_tools`.
    #[serde(default)]
    pub excluded_tools: Option<Vec<String>>,
    /// Plugin-specific configuration sections (keyed by plugin config key).
    #[serde(default)]
    pub sections: HashMap<String, Value>,
}

fn default_max_rounds() -> usize {
    16
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_spec_serde_roundtrip() {
        let spec = AgentSpec {
            id: "coder".into(),
            model: "claude-opus".into(),
            system_prompt: "You are a coding assistant.".into(),
            max_rounds: 8,
            plugin_ids: vec!["permission".into(), "logging".into()],
            allowed_tools: Some(vec!["read_file".into(), "write_file".into()]),
            excluded_tools: Some(vec!["delete_file".into()]),
            sections: {
                let mut m = HashMap::new();
                m.insert("permission".into(), json!({"mode": "strict"}));
                m
            },
        };

        let json_str = serde_json::to_string(&spec).unwrap();
        let parsed: AgentSpec = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed.id, "coder");
        assert_eq!(parsed.model, "claude-opus");
        assert_eq!(parsed.system_prompt, "You are a coding assistant.");
        assert_eq!(parsed.max_rounds, 8);
        assert_eq!(parsed.plugin_ids, vec!["permission", "logging"]);
        assert_eq!(
            parsed.allowed_tools,
            Some(vec!["read_file".into(), "write_file".into()])
        );
        assert_eq!(parsed.excluded_tools, Some(vec!["delete_file".into()]));
        assert_eq!(parsed.sections["permission"]["mode"], "strict");
    }

    #[test]
    fn agent_spec_defaults() {
        let json_str = r#"{"id":"min","model":"m","system_prompt":"sp"}"#;
        let spec: AgentSpec = serde_json::from_str(json_str).unwrap();

        assert_eq!(spec.max_rounds, 16);
        assert!(spec.plugin_ids.is_empty());
        assert!(spec.allowed_tools.is_none());
        assert!(spec.excluded_tools.is_none());
        assert!(spec.sections.is_empty());
    }
}
