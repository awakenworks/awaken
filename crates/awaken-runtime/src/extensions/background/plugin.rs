use awaken_contract::StateError;
use awaken_contract::model::Phase;
use awaken_contract::registry_spec::AgentSpec;
use std::sync::Arc;

use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use crate::state::{KeyScope, MutationBatch, StateKeyOptions, StateStore};

use super::cancel_task_tool::{CANCEL_TASK_TOOL_ID, CancelTaskTool};
use super::hook::BackgroundTaskSyncHook;
use super::manager::BackgroundTaskManager;
use super::state::{BackgroundTaskStateKey, BackgroundTaskViewKey};
use super::types::BACKGROUND_TASKS_PLUGIN_ID;

/// Plugin that registers the background task view state key and
/// the persisted task metadata state key.
///
/// # Single-manager invariant
///
/// Each `StateStore` MUST have exactly one `BackgroundTaskPlugin`, and
/// therefore one `BackgroundTaskManager`. The plugin install path
/// ([`StateStore::install_plugin`]) enforces this by rejecting a second
/// install of the same plugin TypeId, and `BackgroundTaskManager::set_store`
/// uses a `OnceLock` so each manager binds to at most one store.
///
/// This invariant is what makes `bg_{n}` task ids unique within a store —
/// downstream code (`BackgroundTaskStateSnapshot::tasks`,
/// `OtelMetricsSink::task_context_key`) keys by `TaskId` alone and depends
/// on it. Allowing multiple managers per store would require composite keys
/// throughout that path.
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

    fn bind_runtime_context(
        &self,
        store: &StateStore,
        owner_inbox: Option<&crate::inbox::InboxSender>,
    ) {
        self.manager.set_store(store.clone());
        if let Some(inbox) = owner_inbox {
            self.manager.set_owner_inbox(inbox.clone());
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<BackgroundTaskViewKey>(StateKeyOptions::default())?;
        registrar.register_key::<BackgroundTaskStateKey>(StateKeyOptions {
            persistent: true,
            scope: KeyScope::Thread,
            ..StateKeyOptions::default()
        })?;
        registrar.register_tool(
            CANCEL_TASK_TOOL_ID,
            Arc::new(CancelTaskTool::new(self.manager.clone())),
        )?;

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
