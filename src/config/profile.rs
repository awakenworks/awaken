//! Agent profiles, OS config, active config, and run overrides.

use std::collections::{BTreeSet, HashMap};

use super::spec::{ConfigMap, ConfigSlot};

/// Named agent configuration preset.
#[derive(Debug, Clone, Default)]
pub struct AgentProfile {
    pub id: String,
    pub active_plugins: BTreeSet<String>,
    pub config: ConfigMap,
}

impl AgentProfile {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            active_plugins: BTreeSet::new(),
            config: ConfigMap::new(),
        }
    }

    #[must_use]
    pub fn activate(mut self, plugin_id: impl Into<String>) -> Self {
        self.active_plugins.insert(plugin_id.into());
        self
    }

    #[must_use]
    pub fn configure<C: ConfigSlot>(mut self, value: C::Value) -> Self {
        self.config.set::<C>(value);
        self
    }
}

/// Global OS-level configuration: defaults and named profiles.
#[derive(Debug, Clone, Default)]
pub struct OsConfig {
    pub defaults: ConfigMap,
    pub profiles: HashMap<String, AgentProfile>,
}

impl OsConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_default<C: ConfigSlot>(&mut self, value: C::Value) {
        self.defaults.set::<C>(value);
    }

    pub fn register_profile(&mut self, profile: AgentProfile) {
        self.profiles.insert(profile.id.clone(), profile);
    }
}

/// Runtime-mutable baseline configuration (not sole truth — one input to resolution).
#[derive(Debug, Clone, Default)]
pub struct ActiveConfig {
    pub active_plugins: BTreeSet<String>,
    pub config: ConfigMap,
}

impl ActiveConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn activate(&mut self, plugin_id: impl Into<String>) {
        self.active_plugins.insert(plugin_id.into());
    }

    pub fn deactivate(&mut self, plugin_id: &str) {
        self.active_plugins.remove(plugin_id);
    }

    pub fn is_active(&self, plugin_id: &str) -> bool {
        self.active_plugins.contains(plugin_id)
    }

    pub fn set<C: ConfigSlot>(&mut self, value: C::Value) {
        self.config.set::<C>(value);
    }

    pub fn reset<C: ConfigSlot>(&mut self) {
        self.config.remove::<C>();
    }

    /// Replace entire active config from a profile.
    pub fn load_profile(&mut self, profile: &AgentProfile) {
        self.active_plugins = profile.active_plugins.clone();
        self.config = profile.config.clone();
    }

    /// Merge a profile on top (additive, only overrides values that profile has).
    pub fn apply_profile(&mut self, profile: &AgentProfile) {
        for id in &profile.active_plugins {
            self.active_plugins.insert(id.clone());
        }
        self.config.merge_from(&profile.config);
    }
}

/// Per-call overrides (not persisted, single run_phase_with invocation).
#[derive(Debug, Clone, Default)]
pub struct RunOverrides {
    pub activate_plugins: BTreeSet<String>,
    pub deactivate_plugins: BTreeSet<String>,
    pub config: ConfigMap,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    struct TestConfig;
    impl ConfigSlot for TestConfig {
        const KEY: &'static str = "test";
        type Value = TestSettings;
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    struct TestSettings {
        value: String,
    }

    #[test]
    fn agent_profile_builder() {
        let profile = AgentProfile::new("analyst")
            .activate("permission")
            .activate("reminder")
            .configure::<TestConfig>(TestSettings {
                value: "custom".into(),
            });

        assert_eq!(profile.id, "analyst");
        assert!(profile.active_plugins.contains("permission"));
        assert!(profile.active_plugins.contains("reminder"));
        assert_eq!(profile.config.get::<TestConfig>().unwrap().value, "custom");
    }

    #[test]
    fn os_config_defaults_and_profiles() {
        let mut os = OsConfig::new();
        os.set_default::<TestConfig>(TestSettings {
            value: "global".into(),
        });
        os.register_profile(AgentProfile::new("admin").activate("mcp"));

        assert_eq!(os.defaults.get::<TestConfig>().unwrap().value, "global");
        assert!(os.profiles.contains_key("admin"));
        assert!(os.profiles["admin"].active_plugins.contains("mcp"));
    }

    #[test]
    fn active_config_activate_deactivate() {
        let mut active = ActiveConfig::new();
        active.activate("permission");
        active.activate("mcp");
        assert!(active.is_active("permission"));
        assert!(active.is_active("mcp"));

        active.deactivate("mcp");
        assert!(!active.is_active("mcp"));
        assert!(active.is_active("permission"));
    }

    #[test]
    fn active_config_load_profile_replaces() {
        let mut active = ActiveConfig::new();
        active.activate("old_plugin");
        active.set::<TestConfig>(TestSettings {
            value: "old".into(),
        });

        let profile = AgentProfile::new("new")
            .activate("new_plugin")
            .configure::<TestConfig>(TestSettings {
                value: "new".into(),
            });

        active.load_profile(&profile);
        assert!(!active.is_active("old_plugin"));
        assert!(active.is_active("new_plugin"));
        assert_eq!(active.config.get::<TestConfig>().unwrap().value, "new");
    }

    #[test]
    fn active_config_apply_profile_merges() {
        let mut active = ActiveConfig::new();
        active.activate("base_plugin");
        active.set::<TestConfig>(TestSettings {
            value: "base".into(),
        });

        let profile = AgentProfile::new("overlay")
            .activate("extra_plugin")
            .configure::<TestConfig>(TestSettings {
                value: "overlay".into(),
            });

        active.apply_profile(&profile);
        // Base plugin preserved
        assert!(active.is_active("base_plugin"));
        // Extra added
        assert!(active.is_active("extra_plugin"));
        // Config overridden
        assert_eq!(active.config.get::<TestConfig>().unwrap().value, "overlay");
    }

    #[test]
    fn active_config_set_and_reset() {
        let mut active = ActiveConfig::new();
        active.set::<TestConfig>(TestSettings {
            value: "set".into(),
        });
        assert!(active.config.contains::<TestConfig>());

        active.reset::<TestConfig>();
        assert!(!active.config.contains::<TestConfig>());
    }
}
