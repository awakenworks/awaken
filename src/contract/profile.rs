//! Agent profile, plugin config keys, and agent registry.

use std::collections::{HashMap, HashSet};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::StateError;

// ---------------------------------------------------------------------------
// PluginConfigKey — compile-time binding between key string and config type
// ---------------------------------------------------------------------------

/// Typed plugin configuration key.
///
/// Parallel to `StateKey` but for plugin configuration on `AgentSpec`/`AgentProfile`.
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
    type Config: Default + Clone + Serialize + DeserializeOwned + Send + Sync + 'static;
}

// ---------------------------------------------------------------------------
// AgentProfile
// ---------------------------------------------------------------------------

/// Immutable agent configuration preset.
///
/// Resolved by loop runner from a registry using the profile_id in `ActiveAgentKey`.
/// Passed to hooks via `PhaseContext.profile`. Plugins read their own typed config
/// via `profile.config::<K>()`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentProfile {
    /// Profile identifier.
    pub id: String,
    /// Model to use for inference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// System prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Which plugins' hooks are active for this agent.
    /// Empty = no filtering (all hooks run).
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub active_plugins: HashSet<String>,
    /// Allowed tool IDs. None = all tools allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    /// Plugin-specific configuration sections (keyed by PluginConfigKey::KEY).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub sections: HashMap<String, Value>,
}

impl AgentProfile {
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
        self.model = Some(model.into());
        self
    }

    #[must_use]
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
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

// ---------------------------------------------------------------------------
// RunInput
// ---------------------------------------------------------------------------

/// Per-run caller input — immutable for the duration of a run.
///
/// Contains user overrides and run identity. Set once at `run_agent_loop` entry.
#[derive(Debug, Clone, Default)]
pub struct RunInput {
    /// Override model for this run.
    pub model_override: Option<String>,
    /// Override max rounds for this run.
    pub max_rounds_override: Option<usize>,
    /// Run identity (thread_id, run_id, etc).
    pub identity: super::identity::RunIdentity,
}

// ---------------------------------------------------------------------------
// AgentRegistry
// ---------------------------------------------------------------------------

/// Trait for looking up agent profiles by id.
pub trait AgentRegistry: Send + Sync {
    fn get(&self, profile_id: &str) -> Option<&AgentProfile>;
}

/// Simple in-memory agent registry backed by a HashMap.
#[derive(Default)]
pub struct MapAgentRegistry {
    profiles: HashMap<String, AgentProfile>,
}

impl MapAgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, profile: AgentProfile) {
        self.profiles.insert(profile.id.clone(), profile);
    }
}

impl AgentRegistry for MapAgentRegistry {
    fn get(&self, profile_id: &str) -> Option<&AgentProfile> {
        self.profiles.get(profile_id)
    }
}

// ---------------------------------------------------------------------------
// ActiveAgentKey
// ---------------------------------------------------------------------------

/// StateKey for the active agent profile ID. Handoff writes this.
pub struct ActiveAgentKey;

impl crate::state::StateKey for ActiveAgentKey {
    const KEY: &'static str = "__runtime.active_agent";
    type Value = Option<String>;
    type Update = Option<String>;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value = update;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Test config types --

    struct ModelNameKey;
    impl PluginConfigKey for ModelNameKey {
        const KEY: &'static str = "model_name";
        type Config = ModelNameConfig;
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    struct ModelNameConfig {
        pub name: String,
    }

    struct PermKey;
    impl PluginConfigKey for PermKey {
        const KEY: &'static str = "permission";
        type Config = PermConfig;
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    struct PermConfig {
        pub mode: String,
    }

    // -- AgentProfile tests --

    #[test]
    fn profile_typed_config_roundtrip() {
        let profile = AgentProfile::new("test")
            .with_config::<ModelNameKey>(ModelNameConfig {
                name: "opus".into(),
            })
            .unwrap()
            .with_config::<PermKey>(PermConfig {
                mode: "strict".into(),
            })
            .unwrap();

        let model: ModelNameConfig = profile.config::<ModelNameKey>().unwrap();
        assert_eq!(model.name, "opus");

        let perm: PermConfig = profile.config::<PermKey>().unwrap();
        assert_eq!(perm.mode, "strict");
    }

    #[test]
    fn profile_missing_config_returns_default() {
        let profile = AgentProfile::new("test");
        let model: ModelNameConfig = profile.config::<ModelNameKey>().unwrap();
        assert_eq!(model, ModelNameConfig::default());
    }

    #[test]
    fn profile_config_serializes_to_json() {
        let profile = AgentProfile::new("coder")
            .with_model("sonnet")
            .with_config::<ModelNameKey>(ModelNameConfig {
                name: "custom".into(),
            })
            .unwrap();

        let json = serde_json::to_string(&profile).unwrap();
        let parsed: AgentProfile = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.id, "coder");
        assert_eq!(parsed.model.as_deref(), Some("sonnet"));

        let model: ModelNameConfig = parsed.config::<ModelNameKey>().unwrap();
        assert_eq!(model.name, "custom");
    }

    #[test]
    fn profile_multiple_configs_independent() {
        let mut profile = AgentProfile::new("test");
        profile
            .set_config::<ModelNameKey>(ModelNameConfig { name: "a".into() })
            .unwrap();
        profile
            .set_config::<PermKey>(PermConfig { mode: "b".into() })
            .unwrap();

        // Update one doesn't affect the other
        profile
            .set_config::<ModelNameKey>(ModelNameConfig {
                name: "updated".into(),
            })
            .unwrap();

        let model: ModelNameConfig = profile.config::<ModelNameKey>().unwrap();
        assert_eq!(model.name, "updated");

        let perm: PermConfig = profile.config::<PermKey>().unwrap();
        assert_eq!(perm.mode, "b");
    }

    #[test]
    fn profile_with_section_raw_json_still_works() {
        let profile =
            AgentProfile::new("test").with_section("custom", serde_json::json!({"key": "value"}));
        assert_eq!(profile.sections["custom"]["key"], "value");
    }

    #[test]
    fn profile_builder() {
        let profile = AgentProfile::new("reviewer")
            .with_model("claude-opus")
            .with_active_plugin("permission")
            .with_config::<PermKey>(PermConfig {
                mode: "strict".into(),
            })
            .unwrap();

        assert_eq!(profile.id, "reviewer");
        assert_eq!(profile.model.as_deref(), Some("claude-opus"));
        assert!(profile.active_plugins.contains("permission"));
    }

    // -- Registry tests --

    #[test]
    fn map_agent_registry_lookup() {
        let mut registry = MapAgentRegistry::new();
        registry.register(AgentProfile::new("fast").with_model("gpt-4o-mini"));

        assert!(registry.get("fast").is_some());
        assert_eq!(
            registry.get("fast").unwrap().model.as_deref(),
            Some("gpt-4o-mini")
        );
        assert!(registry.get("nonexistent").is_none());
    }

    // -- ActiveAgentKey tests --

    #[test]
    fn active_agent_key_apply() {
        use crate::state::StateKey;
        let mut val: Option<String> = None;
        ActiveAgentKey::apply(&mut val, Some("reviewer".into()));
        assert_eq!(val.as_deref(), Some("reviewer"));
        ActiveAgentKey::apply(&mut val, None);
        assert!(val.is_none());
    }
}
