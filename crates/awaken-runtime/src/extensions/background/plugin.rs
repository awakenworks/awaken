use awaken_runtime_contract::StateError;
use awaken_runtime_contract::model::Phase;
use awaken_runtime_contract::registry_spec::AgentSpec;
use std::sync::Arc;

use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use crate::state::{KeyScope, MutationBatch, StateKeyOptions, StateStore};

use super::cancel_task_tool::{CANCEL_TASK_TOOL_ID, CancelTaskTool};
use super::hook::BackgroundTaskSyncHook;
use super::manager::BackgroundTaskManager;
use super::recover_messages_tool::{RECOVER_FAILED_MESSAGES_TOOL_ID, RecoverFailedMessagesTool};
use super::send_message_tool::{
    DurableMessageSink, FailedDurableMessageKey, MessageDispatchHook, MessageOutboxKey,
    SEND_MESSAGE_TOOL_ID, SendMessageTool,
};
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
    /// Host-provided durable transport, handed to [`MessageDispatchHook`]. When
    /// absent, `send_message` still works for the live `child` route; durable
    /// routes are dead-lettered by the dispatcher.
    durable_sink: Option<Arc<dyn DurableMessageSink>>,
}

impl BackgroundTaskPlugin {
    pub fn new(manager: Arc<BackgroundTaskManager>) -> Self {
        Self {
            manager,
            durable_sink: None,
        }
    }

    /// Create the plugin and wire the store into the manager.
    pub fn with_store(manager: Arc<BackgroundTaskManager>, store: StateStore) -> Self {
        manager.set_store(store);
        Self {
            manager,
            durable_sink: None,
        }
    }

    /// Create the plugin with a host durable sink, enabling durable
    /// (cross-thread) `send_message` delivery. Without it, only the live
    /// `child` route is delivered.
    pub fn with_messaging(
        manager: Arc<BackgroundTaskManager>,
        durable_sink: Arc<dyn DurableMessageSink>,
    ) -> Self {
        Self {
            manager,
            durable_sink: Some(durable_sink),
        }
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

        // `send_message` only commits an outbox entry — it holds no transport,
        // so it is always available (the live `child` route needs no durable
        // sink). The dispatcher is the sole holder of the transports and drains
        // the committed outbox at StepEnd; with no durable sink, durable routes
        // are dead-lettered rather than dropped.
        let outbox_options = StateKeyOptions {
            persistent: true,
            scope: KeyScope::Thread,
            ..StateKeyOptions::default()
        };
        registrar.register_key::<MessageOutboxKey>(outbox_options)?;
        registrar.register_key::<FailedDurableMessageKey>(outbox_options)?;
        registrar.register_tool(SEND_MESSAGE_TOOL_ID, Arc::new(SendMessageTool::new()))?;
        // Recovery surface for the durable-message dead-letter list.
        registrar.register_tool(
            RECOVER_FAILED_MESSAGES_TOOL_ID,
            Arc::new(RecoverFailedMessagesTool::new()),
        )?;
        registrar.register_phase_hook(
            BACKGROUND_TASKS_PLUGIN_ID,
            Phase::StepEnd,
            MessageDispatchHook::new(self.manager.clone(), self.durable_sink.clone()),
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
