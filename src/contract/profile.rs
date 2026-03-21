//! Agent profile — immutable configuration preset for an agent identity.

use std::collections::HashSet;

use serde_json::Value;

/// Immutable agent configuration preset.
///
/// Resolved by loop runner from a registry using the profile_id in `ActiveAgentKey`.
/// Passed to hooks via `PhaseContext.profile`. Plugins read their own sections
/// from `profile.sections`.
#[derive(Debug, Clone, Default)]
pub struct AgentProfile {
    /// Profile identifier.
    pub id: String,
    /// Model to use for inference.
    pub model: Option<String>,
    /// System prompt.
    pub system_prompt: Option<String>,
    /// Which plugins' hooks are active for this agent.
    /// Empty = no filtering (all hooks run).
    pub active_plugins: HashSet<String>,
    /// Allowed tool IDs. None = all tools allowed.
    pub allowed_tools: Option<Vec<String>>,
    /// Plugin-specific configuration sections (keyed by plugin id).
    /// Each plugin reads its own section as JSON and interprets it.
    pub sections: std::collections::HashMap<String, Value>,
}

impl AgentProfile {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Default::default()
        }
    }

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

    #[must_use]
    pub fn with_section(mut self, key: impl Into<String>, value: Value) -> Self {
        self.sections.insert(key.into(), value);
        self
    }
}

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

/// Trait for looking up agent profiles by id.
///
/// Loop runner holds a reference to a registry. Handoff writes a profile_id
/// to `ActiveAgentKey`; loop runner resolves it at each phase boundary.
pub trait AgentRegistry: Send + Sync {
    fn get(&self, profile_id: &str) -> Option<&AgentProfile>;
}

/// Simple in-memory agent registry backed by a HashMap.
#[derive(Default)]
pub struct MapAgentRegistry {
    profiles: std::collections::HashMap<String, AgentProfile>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_profile_builder() {
        let profile = AgentProfile::new("reviewer")
            .with_model("claude-opus")
            .with_active_plugin("permission")
            .with_section("permission", serde_json::json!({"mode": "strict"}));

        assert_eq!(profile.id, "reviewer");
        assert_eq!(profile.model.as_deref(), Some("claude-opus"));
        assert!(profile.active_plugins.contains("permission"));
        assert_eq!(profile.sections["permission"]["mode"], "strict");
    }

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
