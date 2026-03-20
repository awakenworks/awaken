use std::any::TypeId;
use std::collections::HashMap;

use crate::error::StateError;
use crate::model::{
    EffectLog, FailedScheduledActions, PendingScheduledActions, Phase, ScheduledActionLog,
};
use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use crate::state::SlotOptions;

use super::{EffectHandlerArc, PhaseHookArc, ScheduledActionHandlerArc};

#[derive(Default)]
pub(crate) struct InstalledRuntimePlugin {
    pub(crate) scheduled_action_keys: Vec<String>,
    pub(crate) effect_keys: Vec<String>,
    pub(crate) phase_hook_ids: Vec<(Phase, u64)>,
}

pub(crate) struct RuntimeRegistry {
    pub(crate) scheduled_action_handlers: HashMap<String, ScheduledActionHandlerArc>,
    pub(crate) effect_handlers: HashMap<String, EffectHandlerArc>,
    pub(crate) phase_hooks: HashMap<Phase, Vec<(u64, String, PhaseHookArc)>>,
    pub(crate) installed_plugins: HashMap<TypeId, InstalledRuntimePlugin>,
    pub(crate) next_hook_id: u64,
}

impl Default for RuntimeRegistry {
    fn default() -> Self {
        Self {
            scheduled_action_handlers: HashMap::new(),
            effect_handlers: HashMap::new(),
            phase_hooks: HashMap::new(),
            installed_plugins: HashMap::new(),
            next_hook_id: 1,
        }
    }
}

pub(crate) struct RuntimeQueuePlugin;

impl Plugin for RuntimeQueuePlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "phase-runtime",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        let runtime_options = SlotOptions {
            persistent: true,
            retain_on_uninstall: false,
        };
        registrar.register_slot::<PendingScheduledActions>(runtime_options)?;
        registrar.register_slot::<FailedScheduledActions>(runtime_options)?;
        registrar.register_slot::<ScheduledActionLog>(runtime_options)?;
        registrar.register_slot::<EffectLog>(runtime_options)?;
        Ok(())
    }
}
