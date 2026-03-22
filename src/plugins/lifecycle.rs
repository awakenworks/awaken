use crate::error::StateError;
use crate::registry::spec::AgentSpec;
use crate::state::MutationBatch;

use super::{PluginDescriptor, PluginRegistrar};

pub trait Plugin: Send + Sync + 'static {
    fn descriptor(&self) -> PluginDescriptor;

    /// Declare capabilities: state keys, hooks, action handlers, effect handlers, permission checkers.
    /// Called once per resolve to build the ExecutionEnv.
    fn register(&self, _registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        Ok(())
    }

    /// Agent activated: read spec config, write initial state.
    /// Called when this plugin becomes active for a specific agent.
    fn on_activate(
        &self,
        _agent_spec: &AgentSpec,
        _patch: &mut MutationBatch,
    ) -> Result<(), StateError> {
        Ok(())
    }

    /// Agent deactivated: clean up agent-scoped state.
    /// Called when switching away from an agent that uses this plugin.
    fn on_deactivate(&self, _patch: &mut MutationBatch) -> Result<(), StateError> {
        Ok(())
    }
}
