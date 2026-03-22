use std::collections::HashMap;

use awaken_contract::StateError;
use awaken_contract::contract::profile::ActiveAgentIdKey;
use awaken_contract::model::Phase;
use awaken_contract::registry_spec::AgentSpec;

use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use crate::state::{MutationBatch, StateKeyOptions};

use super::action::HandoffAction;
use super::hook::HandoffSyncHook;
use super::state::{ActiveAgentKey, HandoffState};
use super::types::AgentOverlay;

/// Stable plugin ID for handoff.
pub const HANDOFF_PLUGIN_ID: &str = "agent_handoff";

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
        registrar.register_key::<ActiveAgentIdKey>(StateKeyOptions::default())?;
        registrar.register_phase_hook(HANDOFF_PLUGIN_ID, Phase::RunStart, HandoffSyncHook)?;
        registrar.register_phase_hook(HANDOFF_PLUGIN_ID, Phase::StepEnd, HandoffSyncHook)?;
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
        patch.update::<ActiveAgentIdKey>(None);
        Ok(())
    }
}
