use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[cfg(test)]
use awaken_contract::config_loader::load_config_from_str;
use awaken_contract::config_loader::{ConfigLoadError, load_config_from_file};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StarterPromptOverrides {
    pub agents: HashMap<String, AgentPromptOverride>,
}

impl StarterPromptOverrides {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigLoadError> {
        load_config_from_file(path)
    }

    #[cfg(test)]
    pub fn from_str(content: &str, ext: Option<&str>) -> Result<Self, ConfigLoadError> {
        load_config_from_str(content, ext)
    }

    pub fn agent_system_prompt(&self, agent_id: &str) -> Option<&str> {
        self.agents
            .get(agent_id)
            .and_then(|o| o.system_prompt.as_deref())
    }

    pub fn agent_catalog(&self, agent_id: &str) -> Option<&str> {
        self.agents.get(agent_id).and_then(|o| o.catalog.as_deref())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentPromptOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_agent_overrides_from_json() {
        let parsed = StarterPromptOverrides::from_str(
            r#"{
                "agents": {
                    "default": {
                        "system_prompt": "Default agent prompt"
                    },
                    "a2ui": {
                        "system_prompt": "You build procurement UIs.",
                        "catalog": "catalog://procurement-ui"
                    },
                    "json-render-ui": {
                        "catalog": "Card: container\nTable: data records"
                    }
                }
            }"#,
            Some("json"),
        )
        .unwrap();

        assert_eq!(
            parsed.agent_system_prompt("default"),
            Some("Default agent prompt")
        );
        assert_eq!(
            parsed.agent_system_prompt("a2ui"),
            Some("You build procurement UIs.")
        );
        assert_eq!(
            parsed.agent_catalog("a2ui"),
            Some("catalog://procurement-ui")
        );
        assert_eq!(
            parsed.agent_catalog("json-render-ui"),
            Some("Card: container\nTable: data records")
        );
        assert_eq!(parsed.agent_catalog("default"), None);
    }

    #[test]
    fn empty_config_has_no_overrides() {
        let parsed = StarterPromptOverrides::from_str("{}", Some("json")).unwrap();
        assert!(parsed.agents.is_empty());
        assert_eq!(parsed.agent_system_prompt("any"), None);
        assert_eq!(parsed.agent_catalog("any"), None);
    }

    #[test]
    fn agent_override_serializes_cleanly() {
        let overrides = StarterPromptOverrides {
            agents: HashMap::from([(
                "a2ui".into(),
                AgentPromptOverride {
                    system_prompt: Some("custom prompt".into()),
                    catalog: None,
                },
            )]),
        };
        let val = serde_json::to_value(&overrides).unwrap();
        // catalog should be absent (skip_serializing_if)
        assert_eq!(
            val["agents"]["a2ui"]["system_prompt"],
            json!("custom prompt")
        );
        assert!(val["agents"]["a2ui"].get("catalog").is_none());
    }
}
