//! Per-run execution environment — built from resolved plugins.
//!
//! Contains all hooks, action handlers, effect handlers, and tools
//! for one agent run. Not global — rebuilt per resolve().

use std::collections::HashMap;
use std::sync::Arc;

use crate::plugins::{Plugin, PluginRegistrar};
use awaken_contract::StateError;
use awaken_contract::contract::tool::Tool;
use awaken_contract::model::Phase;

use crate::plugins::{KeyRegistration, RequestTransformArc};

use super::{EffectHandlerArc, PhaseHookArc, ScheduledActionHandlerArc};

/// A phase hook with its owning plugin ID.
pub(crate) struct TaggedPhaseHook {
    pub(crate) plugin_id: String,
    pub(crate) hook: PhaseHookArc,
}

/// A request transform with its owning plugin ID.
pub(crate) struct TaggedRequestTransform {
    pub(crate) plugin_id: String,
    pub(crate) transform: RequestTransformArc,
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
    /// Request transforms applied after message assembly, before LLM call.
    /// Tagged with plugin_id for diagnostics and potential per-plugin filtering.
    pub(crate) request_transforms: Vec<TaggedRequestTransform>,
    /// State key registrations collected from all plugins.
    pub(crate) key_registrations: Vec<KeyRegistration>,
    /// Tools registered by plugins (per-spec scoped).
    pub(crate) tools: HashMap<String, Arc<dyn Tool>>,
}

impl ExecutionEnv {
    /// Build an execution environment from a set of plugins.
    ///
    /// Each plugin's `register()` is called to collect hooks, handlers, and
    /// state key registrations. The key registrations are stored so they can
    /// be installed into a `StateStore` at run start.
    pub fn from_plugins(plugins: &[Arc<dyn Plugin>]) -> Result<Self, StateError> {
        let mut all_hooks: HashMap<Phase, Vec<TaggedPhaseHook>> = HashMap::new();
        let mut all_action_handlers: HashMap<String, ScheduledActionHandlerArc> = HashMap::new();
        let mut all_effect_handlers: HashMap<String, EffectHandlerArc> = HashMap::new();
        let mut all_transforms: Vec<TaggedRequestTransform> = Vec::new();
        let mut all_key_registrations: Vec<KeyRegistration> = Vec::new();
        let mut all_tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

        for plugin in plugins {
            let plugin_name = plugin.descriptor().name.to_string();
            let mut registrar = PluginRegistrar::new();
            plugin.register(&mut registrar)?;

            // Collect tools (check cross-plugin duplicates)
            for entry in registrar.tools {
                if all_tools.contains_key(&entry.id) {
                    return Err(StateError::ToolAlreadyRegistered { tool_id: entry.id });
                }
                tracing::debug!(
                    plugin_id = %plugin_name,
                    tool_id = %entry.id,
                    "registered_plugin_tool"
                );
                all_tools.insert(entry.id, entry.tool);
            }

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

            // Collect request transforms (tagged with plugin_id)
            for entry in registrar.request_transforms {
                tracing::debug!(plugin_id = %entry.plugin_id, "registered_request_transform");
                all_transforms.push(TaggedRequestTransform {
                    plugin_id: entry.plugin_id,
                    transform: entry.transform,
                });
            }

            // Collect state key registrations
            all_key_registrations.extend(registrar.keys);
        }

        Ok(Self {
            phase_hooks: all_hooks,
            scheduled_action_handlers: all_action_handlers,
            effect_handlers: all_effect_handlers,
            request_transforms: all_transforms,
            key_registrations: all_key_registrations,
            tools: all_tools,
        })
    }

    /// Create an empty execution environment (no hooks, no handlers).
    pub fn empty() -> Self {
        Self {
            phase_hooks: HashMap::new(),
            scheduled_action_handlers: HashMap::new(),
            effect_handlers: HashMap::new(),
            request_transforms: Vec::new(),
            key_registrations: Vec::new(),
            tools: HashMap::new(),
        }
    }

    /// Get the request transform arcs (without plugin tags) for `apply_transforms`.
    pub(crate) fn transform_arcs(&self) -> Vec<RequestTransformArc> {
        self.request_transforms
            .iter()
            .map(|t| Arc::clone(&t.transform))
            .collect()
    }

    /// Get all tagged hooks for a phase.
    pub(crate) fn hooks_for_phase(&self, phase: Phase) -> &[TaggedPhaseHook] {
        self.phase_hooks
            .get(&phase)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }
}
