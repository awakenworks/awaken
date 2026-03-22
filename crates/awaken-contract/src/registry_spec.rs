//! Serializable agent definition — pure data, no trait objects.
//!
//! `AgentSpec` is the unified agent configuration: it describes both the
//! declarative registry references (model, plugins, tools) and the runtime
//! behavior (active_plugins filtering, typed plugin sections, context policy).
//!
//! Supersedes the former `AgentProfile` — see ADR-0009.

use std::collections::{HashMap, HashSet};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::contract::inference::ContextWindowPolicy;
use crate::error::StateError;

// ---------------------------------------------------------------------------
// PluginConfigKey — compile-time binding between key string and config type
// ---------------------------------------------------------------------------

/// Typed plugin configuration key.
///
/// Binds a string key to a concrete config type at compile time.
///
/// ```ignore
/// struct PermissionConfigKey;
/// impl PluginConfigKey for PermissionConfigKey {
///     const KEY: &'static str = "permission";
///     type Config = PermissionConfig;
/// }
/// ```
pub trait PluginConfigKey: 'static + Send + Sync {
    /// Section key in the `sections` map.
    const KEY: &'static str;

    /// Typed configuration value.
    type Config: Default
        + Clone
        + Serialize
        + DeserializeOwned
        + schemars::JsonSchema
        + Send
        + Sync
        + 'static;
}

// ---------------------------------------------------------------------------
// AgentSpec
// ---------------------------------------------------------------------------

/// Serializable agent definition referencing registries by ID.
///
/// Can be saved to JSON, loaded from config files, or transmitted over the network.
/// Resolved at runtime via [`super::resolve::resolve`] into a [`super::resolve::ResolvedRun`].
///
/// Also serves as the runtime behavior configuration passed to hooks via
/// `PhaseContext.agent_spec`. Plugins read their typed config via `spec.config::<K>()`.
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
    /// Maximum continuation retries for truncated LLM responses.
    #[serde(default = "default_max_continuation_retries")]
    pub max_continuation_retries: usize,
    /// Context window management policy. `None` disables compaction and truncation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_policy: Option<ContextWindowPolicy>,
    /// PluginRegistry IDs — resolved at build time.
    #[serde(default)]
    pub plugin_ids: Vec<String>,
    /// Which plugins' hooks are active for this agent.
    /// Empty = no filtering (all hooks run).
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub active_plugins: HashSet<String>,
    /// Allowed tool IDs (whitelist). `None` = all tools.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Excluded tool IDs (blacklist). Applied after `allowed_tools`.
    #[serde(default)]
    pub excluded_tools: Option<Vec<String>>,
    /// Sub-agent delegates this agent can invoke.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delegates: Vec<AgentDelegate>,
    /// Plugin-specific configuration sections (keyed by PluginConfigKey::KEY).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub sections: HashMap<String, Value>,
}

/// A sub-agent that this agent can delegate to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDelegate {
    /// Agent ID for the tool name (`agent_run_{agent_id}`).
    pub agent_id: String,
    /// Description shown to the LLM.
    #[serde(default)]
    pub description: String,
    /// Binding: local or remote.
    #[serde(default)]
    pub binding: AgentBinding,
}

/// How to reach a delegate agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentBinding {
    /// Agent runs in the same process.
    #[default]
    Local,
    /// Agent runs on a remote A2A endpoint.
    Remote {
        base_url: String,
        #[serde(default)]
        bearer_token: Option<String>,
        #[serde(default = "default_poll_interval")]
        poll_interval_ms: u64,
        #[serde(default = "default_timeout")]
        timeout_ms: u64,
    },
}

fn default_poll_interval() -> u64 {
    2000
}

fn default_timeout() -> u64 {
    300_000
}

impl Default for AgentSpec {
    fn default() -> Self {
        Self {
            id: String::new(),
            model: String::new(),
            system_prompt: String::new(),
            max_rounds: default_max_rounds(),
            max_continuation_retries: default_max_continuation_retries(),
            context_policy: None,
            plugin_ids: Vec::new(),
            active_plugins: HashSet::new(),
            allowed_tools: None,
            excluded_tools: None,
            delegates: Vec::new(),
            sections: HashMap::new(),
        }
    }
}

fn default_max_rounds() -> usize {
    16
}

fn default_max_continuation_retries() -> usize {
    2
}

impl AgentSpec {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Default::default()
        }
    }

    // -- Typed config access --

    /// Read a typed plugin config section.
    /// Returns `Config::default()` if the section is missing.
    /// Returns error if the section exists but fails to deserialize.
    pub fn config<K: PluginConfigKey>(&self) -> Result<K::Config, StateError> {
        match self.sections.get(K::KEY) {
            Some(value) => {
                serde_json::from_value(value.clone()).map_err(|e| StateError::KeyDecode {
                    key: K::KEY.into(),
                    message: e.to_string(),
                })
            }
            None => Ok(K::Config::default()),
        }
    }

    /// Set a typed plugin config section.
    pub fn set_config<K: PluginConfigKey>(&mut self, config: K::Config) -> Result<(), StateError> {
        let value = serde_json::to_value(config).map_err(|e| StateError::KeyEncode {
            key: K::KEY.into(),
            message: e.to_string(),
        })?;
        self.sections.insert(K::KEY.to_string(), value);
        Ok(())
    }

    // -- Builder methods --

    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    #[must_use]
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    #[must_use]
    pub fn with_max_rounds(mut self, n: usize) -> Self {
        self.max_rounds = n;
        self
    }

    #[must_use]
    pub fn with_active_plugin(mut self, plugin_id: impl Into<String>) -> Self {
        self.active_plugins.insert(plugin_id.into());
        self
    }

    /// Set a typed plugin config section (builder variant).
    pub fn with_config<K: PluginConfigKey>(
        mut self,
        config: K::Config,
    ) -> Result<Self, StateError> {
        self.set_config::<K>(config)?;
        Ok(self)
    }

    /// Set a raw JSON section (for tests or untyped usage).
    #[must_use]
    pub fn with_section(mut self, key: impl Into<String>, value: Value) -> Self {
        self.sections.insert(key.into(), value);
        self
    }
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
            ..Default::default()
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
        assert_eq!(spec.max_continuation_retries, 2);
        assert!(spec.context_policy.is_none());
        assert!(spec.plugin_ids.is_empty());
        assert!(spec.active_plugins.is_empty());
        assert!(spec.allowed_tools.is_none());
        assert!(spec.excluded_tools.is_none());
        assert!(spec.sections.is_empty());
    }

    // -- Typed config tests (merged from AgentProfile) --

    struct ModelNameKey;
    impl PluginConfigKey for ModelNameKey {
        const KEY: &'static str = "model_name";
        type Config = ModelNameConfig;
    }

    #[derive(
        Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
    )]
    struct ModelNameConfig {
        pub name: String,
    }

    struct PermKey;
    impl PluginConfigKey for PermKey {
        const KEY: &'static str = "permission";
        type Config = PermConfig;
    }

    #[derive(
        Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
    )]
    struct PermConfig {
        pub mode: String,
    }

    #[test]
    fn typed_config_roundtrip() {
        let spec = AgentSpec::new("test")
            .with_config::<ModelNameKey>(ModelNameConfig {
                name: "opus".into(),
            })
            .unwrap()
            .with_config::<PermKey>(PermConfig {
                mode: "strict".into(),
            })
            .unwrap();

        let model: ModelNameConfig = spec.config::<ModelNameKey>().unwrap();
        assert_eq!(model.name, "opus");

        let perm: PermConfig = spec.config::<PermKey>().unwrap();
        assert_eq!(perm.mode, "strict");
    }

    #[test]
    fn missing_config_returns_default() {
        let spec = AgentSpec::new("test");
        let model: ModelNameConfig = spec.config::<ModelNameKey>().unwrap();
        assert_eq!(model, ModelNameConfig::default());
    }

    #[test]
    fn config_serializes_to_json() {
        let spec = AgentSpec::new("coder")
            .with_model("sonnet")
            .with_config::<ModelNameKey>(ModelNameConfig {
                name: "custom".into(),
            })
            .unwrap();

        let json = serde_json::to_string(&spec).unwrap();
        let parsed: AgentSpec = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.id, "coder");
        assert_eq!(parsed.model, "sonnet");

        let model: ModelNameConfig = parsed.config::<ModelNameKey>().unwrap();
        assert_eq!(model.name, "custom");
    }

    #[test]
    fn multiple_configs_independent() {
        let mut spec = AgentSpec::new("test");
        spec.set_config::<ModelNameKey>(ModelNameConfig { name: "a".into() })
            .unwrap();
        spec.set_config::<PermKey>(PermConfig { mode: "b".into() })
            .unwrap();

        // Update one doesn't affect the other
        spec.set_config::<ModelNameKey>(ModelNameConfig {
            name: "updated".into(),
        })
        .unwrap();

        let model: ModelNameConfig = spec.config::<ModelNameKey>().unwrap();
        assert_eq!(model.name, "updated");

        let perm: PermConfig = spec.config::<PermKey>().unwrap();
        assert_eq!(perm.mode, "b");
    }

    #[test]
    fn with_section_raw_json_still_works() {
        let spec =
            AgentSpec::new("test").with_section("custom", serde_json::json!({"key": "value"}));
        assert_eq!(spec.sections["custom"]["key"], "value");
    }

    #[test]
    fn builder() {
        let spec = AgentSpec::new("reviewer")
            .with_model("claude-opus")
            .with_active_plugin("permission")
            .with_config::<PermKey>(PermConfig {
                mode: "strict".into(),
            })
            .unwrap();

        assert_eq!(spec.id, "reviewer");
        assert_eq!(spec.model, "claude-opus");
        assert!(spec.active_plugins.contains("permission"));
    }
}
