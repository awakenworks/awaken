//! Agent handoff extension — dynamic same-thread agent switching.
//!
//! Manages agent variant switching within a running agent loop:
//!
//! 1. `HandoffState` tracks active and requested agent variants.
//! 2. `HandoffPlugin` reads state and applies agent overlays dynamically.
//! 3. `AgentOverlay` defines per-variant overrides (system prompt, model, tools).
//!
//! No run termination or re-resolution occurs — handoff is instant.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use crate::state::{MutationBatch, StateKey, StateKeyOptions};
use awaken_contract::StateError;
use awaken_contract::registry_spec::AgentSpec;

/// Stable plugin ID for handoff.
pub const HANDOFF_PLUGIN_ID: &str = "agent_handoff";

// ---------------------------------------------------------------------------
// AgentOverlay
// ---------------------------------------------------------------------------

/// Dynamic agent spec overlay applied during handoff.
///
/// Each field, when `Some`, overrides the base agent configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentOverlay {
    /// Override the system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Override the model ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Whitelist of allowed tool IDs (None = all tools allowed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    /// Explicit tool IDs to exclude.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_tools: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// HandoffState + HandoffAction
// ---------------------------------------------------------------------------

/// Persisted handoff state — tracks the active agent variant and any pending request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HandoffState {
    /// The currently active agent variant (`None` = base configuration).
    pub active_agent: Option<String>,
    /// A handoff requested by the tool, pending activation.
    pub requested_agent: Option<String>,
}

/// Action type for the `HandoffState` reducer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HandoffAction {
    /// Request a handoff to another agent variant.
    Request { agent: String },
    /// Activate the handoff (consumed by the plugin).
    Activate { agent: String },
    /// Clear all handoff state (return to base agent).
    Clear,
}

impl HandoffState {
    fn reduce(&mut self, action: HandoffAction) {
        match action {
            HandoffAction::Request { agent } => {
                self.requested_agent = Some(agent);
            }
            HandoffAction::Activate { agent } => {
                self.active_agent = Some(agent);
                self.requested_agent = None;
            }
            HandoffAction::Clear => {
                self.active_agent = None;
                self.requested_agent = None;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ActiveAgentKey (StateKey)
// ---------------------------------------------------------------------------

/// State key for the handoff state.
pub struct ActiveAgentKey;

impl StateKey for ActiveAgentKey {
    const KEY: &'static str = "agent_handoff";
    type Value = HandoffState;
    type Update = HandoffAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.reduce(update);
    }
}

// ---------------------------------------------------------------------------
// HandoffPlugin
// ---------------------------------------------------------------------------

/// Dynamic agent handoff plugin.
///
/// Applies agent overlays dynamically within the running agent loop.
/// Configured with a map of agent variant name -> overlay.
pub struct HandoffPlugin {
    overlays: HashMap<String, AgentOverlay>,
}

impl HandoffPlugin {
    /// Create a new handoff plugin with the given agent variant overlays.
    pub fn new(overlays: HashMap<String, AgentOverlay>) -> Self {
        Self { overlays }
    }

    /// Get the overlay for a given agent variant.
    pub fn overlay(&self, agent: &str) -> Option<&AgentOverlay> {
        self.overlays.get(agent)
    }

    /// Get the effective agent ID from the handoff state.
    pub fn effective_agent(state: &HandoffState) -> Option<&String> {
        state
            .requested_agent
            .as_ref()
            .or(state.active_agent.as_ref())
    }
}

impl Plugin for HandoffPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: HANDOFF_PLUGIN_ID,
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<ActiveAgentKey>(StateKeyOptions::default())?;
        Ok(())
    }

    fn on_activate(
        &self,
        _agent_spec: &AgentSpec,
        _patch: &mut MutationBatch,
    ) -> Result<(), StateError> {
        Ok(())
    }

    fn on_deactivate(&self, patch: &mut MutationBatch) -> Result<(), StateError> {
        patch.update::<ActiveAgentKey>(HandoffAction::Clear);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Action constructors
// ---------------------------------------------------------------------------

/// Create a handoff request mutation.
pub fn request_handoff(agent: impl Into<String>) -> HandoffAction {
    HandoffAction::Request {
        agent: agent.into(),
    }
}

/// Create a handoff activation mutation.
pub fn activate_handoff(agent: impl Into<String>) -> HandoffAction {
    HandoffAction::Activate {
        agent: agent.into(),
    }
}

/// Create a clear-handoff mutation.
pub fn clear_handoff() -> HandoffAction {
    HandoffAction::Clear
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateStore;

    #[test]
    fn default_state_is_all_none() {
        let state = HandoffState::default();
        assert!(state.active_agent.is_none());
        assert!(state.requested_agent.is_none());
    }

    #[test]
    fn request_sets_requested_agent() {
        let mut state = HandoffState::default();
        state.reduce(HandoffAction::Request {
            agent: "fast".into(),
        });
        assert!(state.active_agent.is_none());
        assert_eq!(state.requested_agent.as_deref(), Some("fast"));
    }

    #[test]
    fn activate_sets_active_and_clears_requested() {
        let mut state = HandoffState {
            active_agent: None,
            requested_agent: Some("fast".into()),
        };
        state.reduce(HandoffAction::Activate {
            agent: "fast".into(),
        });
        assert_eq!(state.active_agent.as_deref(), Some("fast"));
        assert!(state.requested_agent.is_none());
    }

    #[test]
    fn clear_resets_both() {
        let mut state = HandoffState {
            active_agent: Some("fast".into()),
            requested_agent: Some("deep".into()),
        };
        state.reduce(HandoffAction::Clear);
        assert!(state.active_agent.is_none());
        assert!(state.requested_agent.is_none());
    }

    #[test]
    fn roundtrip_serialization() {
        let state = HandoffState {
            active_agent: Some("fast".into()),
            requested_agent: None,
        };
        let json = serde_json::to_value(&state).unwrap();
        let back: HandoffState = serde_json::from_value(json).unwrap();
        assert_eq!(state.active_agent, back.active_agent);
        assert_eq!(state.requested_agent, back.requested_agent);
    }

    #[test]
    fn action_roundtrip() {
        let action = HandoffAction::Request {
            agent: "fast".into(),
        };
        let json = serde_json::to_value(&action).unwrap();
        let back: HandoffAction = serde_json::from_value(json).unwrap();
        assert_eq!(action, back);
    }

    #[test]
    fn plugin_descriptor() {
        let plugin = HandoffPlugin::new(HashMap::new());
        assert_eq!(plugin.descriptor().name, HANDOFF_PLUGIN_ID);
    }

    #[test]
    fn plugin_registers_state_key() {
        let plugin = HandoffPlugin::new(HashMap::new());
        let store = StateStore::new();
        store.install_plugin(plugin).unwrap();
        // Key should be registered
        let registry = store.registry.lock();
        assert!(registry.keys_by_name.contains_key("agent_handoff"));
    }

    #[test]
    fn effective_agent_prefers_requested() {
        let state = HandoffState {
            active_agent: Some("slow".into()),
            requested_agent: Some("fast".into()),
        };
        assert_eq!(
            HandoffPlugin::effective_agent(&state).map(String::as_str),
            Some("fast")
        );
    }

    #[test]
    fn effective_agent_falls_back_to_active() {
        let state = HandoffState {
            active_agent: Some("slow".into()),
            requested_agent: None,
        };
        assert_eq!(
            HandoffPlugin::effective_agent(&state).map(String::as_str),
            Some("slow")
        );
    }

    #[test]
    fn effective_agent_none_when_empty() {
        let state = HandoffState::default();
        assert!(HandoffPlugin::effective_agent(&state).is_none());
    }

    #[test]
    fn overlay_lookup() {
        let mut overlays = HashMap::new();
        overlays.insert(
            "fast".to_string(),
            AgentOverlay {
                model: Some("claude-haiku".into()),
                system_prompt: Some("You are in fast mode.".into()),
                ..Default::default()
            },
        );
        let plugin = HandoffPlugin::new(overlays);
        assert!(plugin.overlay("fast").is_some());
        assert!(plugin.overlay("missing").is_none());
    }

    #[test]
    fn handoff_state_via_store() {
        let store = StateStore::new();
        let plugin = HandoffPlugin::new(HashMap::new());
        store.install_plugin(plugin).unwrap();

        // Request handoff
        let mut patch = store.begin_mutation();
        patch.update::<ActiveAgentKey>(request_handoff("fast"));
        store.commit(patch).unwrap();

        let state = store.read::<ActiveAgentKey>().unwrap();
        assert_eq!(state.requested_agent.as_deref(), Some("fast"));
        assert!(state.active_agent.is_none());

        // Activate
        let mut patch = store.begin_mutation();
        patch.update::<ActiveAgentKey>(activate_handoff("fast"));
        store.commit(patch).unwrap();

        let state = store.read::<ActiveAgentKey>().unwrap();
        assert_eq!(state.active_agent.as_deref(), Some("fast"));
        assert!(state.requested_agent.is_none());

        // Clear
        let mut patch = store.begin_mutation();
        patch.update::<ActiveAgentKey>(clear_handoff());
        store.commit(patch).unwrap();

        let state = store.read::<ActiveAgentKey>().unwrap();
        assert!(state.active_agent.is_none());
        assert!(state.requested_agent.is_none());
    }

    // -----------------------------------------------------------------------
    // Migrated from uncarve: additional handoff state tests
    // -----------------------------------------------------------------------

    #[test]
    fn request_overwrites_previous_request() {
        let mut state = HandoffState::default();
        state.reduce(HandoffAction::Request {
            agent: "first".into(),
        });
        state.reduce(HandoffAction::Request {
            agent: "second".into(),
        });
        assert_eq!(state.requested_agent.as_deref(), Some("second"));
    }

    #[test]
    fn activate_different_agent_than_requested() {
        let mut state = HandoffState {
            active_agent: None,
            requested_agent: Some("fast".into()),
        };
        // Can activate a different agent than requested
        state.reduce(HandoffAction::Activate {
            agent: "slow".into(),
        });
        assert_eq!(state.active_agent.as_deref(), Some("slow"));
        assert!(state.requested_agent.is_none());
    }

    #[test]
    fn request_does_not_affect_active() {
        let mut state = HandoffState {
            active_agent: Some("current".into()),
            requested_agent: None,
        };
        state.reduce(HandoffAction::Request {
            agent: "next".into(),
        });
        assert_eq!(state.active_agent.as_deref(), Some("current"));
        assert_eq!(state.requested_agent.as_deref(), Some("next"));
    }

    #[test]
    fn activate_replaces_active() {
        let mut state = HandoffState {
            active_agent: Some("old".into()),
            requested_agent: Some("new".into()),
        };
        state.reduce(HandoffAction::Activate {
            agent: "new".into(),
        });
        assert_eq!(state.active_agent.as_deref(), Some("new"));
        assert!(state.requested_agent.is_none());
    }

    #[test]
    fn clear_on_already_empty_is_noop() {
        let mut state = HandoffState::default();
        state.reduce(HandoffAction::Clear);
        assert!(state.active_agent.is_none());
        assert!(state.requested_agent.is_none());
    }

    #[test]
    fn action_all_variants_serialization() {
        let actions = vec![
            HandoffAction::Request {
                agent: "test".into(),
            },
            HandoffAction::Activate {
                agent: "test".into(),
            },
            HandoffAction::Clear,
        ];
        for action in actions {
            let json = serde_json::to_value(&action).unwrap();
            let back: HandoffAction = serde_json::from_value(json).unwrap();
            assert_eq!(action, back);
        }
    }

    #[test]
    fn overlay_default_is_all_none() {
        let overlay = AgentOverlay::default();
        assert!(overlay.system_prompt.is_none());
        assert!(overlay.model.is_none());
        assert!(overlay.allowed_tools.is_none());
        assert!(overlay.excluded_tools.is_none());
    }

    #[test]
    fn overlay_serialization_roundtrip() {
        let overlay = AgentOverlay {
            system_prompt: Some("You are helpful".into()),
            model: Some("gpt-4".into()),
            allowed_tools: Some(vec!["search".into(), "read".into()]),
            excluded_tools: Some(vec!["delete".into()]),
        };
        let json = serde_json::to_value(&overlay).unwrap();
        let back: AgentOverlay = serde_json::from_value(json).unwrap();
        assert_eq!(back.system_prompt.as_deref(), Some("You are helpful"));
        assert_eq!(back.model.as_deref(), Some("gpt-4"));
        assert_eq!(back.allowed_tools.as_ref().unwrap().len(), 2);
        assert_eq!(back.excluded_tools.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn plugin_overlay_returns_configured_overlay() {
        let mut overlays = HashMap::new();
        overlays.insert(
            "fast".to_string(),
            AgentOverlay {
                model: Some("haiku".into()),
                ..Default::default()
            },
        );
        overlays.insert(
            "deep".to_string(),
            AgentOverlay {
                model: Some("opus".into()),
                ..Default::default()
            },
        );
        let plugin = HandoffPlugin::new(overlays);
        assert_eq!(
            plugin.overlay("fast").unwrap().model.as_deref(),
            Some("haiku")
        );
        assert_eq!(
            plugin.overlay("deep").unwrap().model.as_deref(),
            Some("opus")
        );
        assert!(plugin.overlay("nonexistent").is_none());
    }

    #[test]
    fn handoff_full_lifecycle_via_store() {
        let store = StateStore::new();
        store
            .install_plugin(HandoffPlugin::new(HashMap::new()))
            .unwrap();

        // Initial: no active, no requested
        let state = store.read::<ActiveAgentKey>();
        assert!(state.is_none() || state.unwrap().active_agent.is_none());

        // Request fast
        let mut patch = store.begin_mutation();
        patch.update::<ActiveAgentKey>(request_handoff("fast"));
        store.commit(patch).unwrap();

        let state = store.read::<ActiveAgentKey>().unwrap();
        assert_eq!(state.requested_agent.as_deref(), Some("fast"));
        assert!(state.active_agent.is_none());

        // Activate fast
        let mut patch = store.begin_mutation();
        patch.update::<ActiveAgentKey>(activate_handoff("fast"));
        store.commit(patch).unwrap();

        let state = store.read::<ActiveAgentKey>().unwrap();
        assert_eq!(state.active_agent.as_deref(), Some("fast"));
        assert!(state.requested_agent.is_none());

        // Request deep (while fast is active)
        let mut patch = store.begin_mutation();
        patch.update::<ActiveAgentKey>(request_handoff("deep"));
        store.commit(patch).unwrap();

        let state = store.read::<ActiveAgentKey>().unwrap();
        assert_eq!(state.active_agent.as_deref(), Some("fast"));
        assert_eq!(state.requested_agent.as_deref(), Some("deep"));

        // Effective should prefer requested
        assert_eq!(
            HandoffPlugin::effective_agent(&state).map(String::as_str),
            Some("deep")
        );

        // Clear
        let mut patch = store.begin_mutation();
        patch.update::<ActiveAgentKey>(clear_handoff());
        store.commit(patch).unwrap();

        let state = store.read::<ActiveAgentKey>().unwrap();
        assert!(HandoffPlugin::effective_agent(&state).is_none());
    }
}
