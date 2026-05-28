//! Internal runtime plugins — state keys for action queues.

use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use crate::state::StateKeyOptions;
use awaken_runtime_contract::StateError;
use awaken_runtime_contract::model::{FailedScheduledActions, PendingScheduledActions};

/// Internal plugin that registers runtime queue state keys.
pub(crate) struct RuntimeQueuePlugin;

impl Plugin for RuntimeQueuePlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "phase-runtime",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        let runtime_options = StateKeyOptions {
            persistent: true,
            retain_on_uninstall: false,
            scope: crate::state::KeyScope::Run,
        };
        registrar.register_key::<PendingScheduledActions>(runtime_options)?;
        registrar.register_key::<FailedScheduledActions>(runtime_options)?;
        Ok(())
    }
}
