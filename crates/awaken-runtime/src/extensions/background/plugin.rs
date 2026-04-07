use std::sync::Arc;

use awaken_contract::StateError;
use awaken_contract::model::Phase;
use awaken_contract::registry_spec::AgentSpec;

use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use crate::state::{KeyScope, MutationBatch, StateKeyOptions, StateStore};

use super::hook::BackgroundTaskSyncHook;
use super::manager::BackgroundTaskManager;
use super::state::{BackgroundTaskStateKey, BackgroundTaskViewKey};
use super::types::BACKGROUND_TASKS_PLUGIN_ID;

/// Plugin that registers the background task view state key and
/// the persisted task metadata state key.
pub struct BackgroundTaskPlugin {
    manager: Arc<BackgroundTaskManager>,
}

impl BackgroundTaskPlugin {
    pub fn new(manager: Arc<BackgroundTaskManager>) -> Self {
        Self { manager }
    }

    /// Create the plugin and wire the store into the manager.
    pub fn with_store(manager: Arc<BackgroundTaskManager>, store: StateStore) -> Self {
        manager.set_store(store);
        Self { manager }
    }

    /// Return the manager for inbox wiring.
    pub fn manager(&self) -> &Arc<BackgroundTaskManager> {
        &self.manager
    }
}

impl Plugin for BackgroundTaskPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: BACKGROUND_TASKS_PLUGIN_ID,
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<BackgroundTaskViewKey>(StateKeyOptions::default())?;
        registrar.register_key::<BackgroundTaskStateKey>(StateKeyOptions {
            persistent: true,
            scope: KeyScope::Thread,
            ..StateKeyOptions::default()
        })?;

        // Sync task metadata into persisted state at run boundaries.
        registrar.register_phase_hook(
            BACKGROUND_TASKS_PLUGIN_ID,
            Phase::RunStart,
            BackgroundTaskSyncHook {
                manager: self.manager.clone(),
            },
        )?;
        registrar.register_phase_hook(
            BACKGROUND_TASKS_PLUGIN_ID,
            Phase::RunEnd,
            BackgroundTaskSyncHook {
                manager: self.manager.clone(),
            },
        )?;
        // Update PendingWorkKey at step boundaries so the orchestrator
        // can detect running tasks without knowing about this plugin.
        registrar.register_phase_hook(
            BACKGROUND_TASKS_PLUGIN_ID,
            Phase::StepEnd,
            BackgroundTaskSyncHook {
                manager: self.manager.clone(),
            },
        )?;

        Ok(())
    }

    fn on_activate(
        &self,
        _agent_spec: &AgentSpec,
        _patch: &mut MutationBatch,
    ) -> Result<(), StateError> {
        Ok(())
    }
}
