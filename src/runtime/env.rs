//! Per-run execution environment — built from resolved plugins.
//!
//! Contains all hooks, action handlers, effect handlers, and permission checkers
//! for one agent run. Not global — rebuilt per resolve().

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::StateError;
use crate::model::Phase;
use crate::plugins::{Plugin, PluginRegistrar};

use super::handlers::{
    EffectHandlerArc, PhaseHookArc, ScheduledActionHandlerArc, ToolPermissionCheckerArc,
};

/// A phase hook with its owning plugin ID.
pub(crate) struct TaggedPhaseHook {
    pub(crate) plugin_id: String,
    pub(crate) hook: PhaseHookArc,
}

/// Per-run execution environment.
///
/// Built from a set of plugins via `ExecutionEnv::from_plugins()`.
/// Passed to `PhaseRuntime` methods for each phase execution.
/// Not shared across agent runs — each resolve produces a new one.
pub struct ExecutionEnv {
    pub(crate) phase_hooks: HashMap<Phase, Vec<TaggedPhaseHook>>,
    pub(crate) scheduled_action_handlers: HashMap<String, ScheduledActionHandlerArc>,
    pub(crate) effect_handlers: HashMap<String, EffectHandlerArc>,
    pub(crate) tool_permission_checkers: Vec<ToolPermissionCheckerArc>,
}

impl ExecutionEnv {
    /// Build an execution environment from a set of plugins.
    ///
    /// Each plugin's `register()` is called to collect hooks and handlers.
    /// State keys registered by plugins are ignored here — they are handled
    /// separately by `StateStore::install_plugin_with_keys()`.
    pub fn from_plugins(plugins: &[Arc<dyn Plugin>]) -> Result<Self, StateError> {
        let mut all_hooks: HashMap<Phase, Vec<TaggedPhaseHook>> = HashMap::new();
        let mut all_action_handlers: HashMap<String, ScheduledActionHandlerArc> = HashMap::new();
        let mut all_effect_handlers: HashMap<String, EffectHandlerArc> = HashMap::new();
        let mut all_permission_checkers: Vec<ToolPermissionCheckerArc> = Vec::new();

        for plugin in plugins {
            let mut registrar = PluginRegistrar::new();
            plugin.register(&mut registrar)?;

            // Collect hooks (with plugin_id for profile filtering)
            for entry in registrar.phase_hooks {
                all_hooks
                    .entry(entry.phase)
                    .or_default()
                    .push(TaggedPhaseHook {
                        plugin_id: entry.plugin_id,
                        hook: entry.hook,
                    });
            }

            // Collect action handlers (check duplicates)
            for entry in registrar.scheduled_actions {
                if all_action_handlers.contains_key(&entry.key) {
                    return Err(StateError::HandlerAlreadyRegistered { key: entry.key });
                }
                all_action_handlers.insert(entry.key, entry.handler);
            }

            // Collect effect handlers (check duplicates)
            for entry in registrar.effects {
                if all_effect_handlers.contains_key(&entry.key) {
                    return Err(StateError::EffectHandlerAlreadyRegistered { key: entry.key });
                }
                all_effect_handlers.insert(entry.key, entry.handler);
            }

            // Collect permission checkers
            for entry in registrar.tool_permissions {
                all_permission_checkers.push(entry.checker);
            }
        }

        Ok(Self {
            phase_hooks: all_hooks,
            scheduled_action_handlers: all_action_handlers,
            effect_handlers: all_effect_handlers,
            tool_permission_checkers: all_permission_checkers,
        })
    }

    /// Create an empty execution environment (no hooks, no handlers).
    pub fn empty() -> Self {
        Self {
            phase_hooks: HashMap::new(),
            scheduled_action_handlers: HashMap::new(),
            effect_handlers: HashMap::new(),
            tool_permission_checkers: Vec::new(),
        }
    }

    /// Get all tagged hooks for a phase.
    pub(crate) fn hooks_for_phase(&self, phase: Phase) -> &[TaggedPhaseHook] {
        self.phase_hooks
            .get(&phase)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }
}
