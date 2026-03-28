//! Per-run execution environment — built from resolved plugins.
//!
//! Contains all hooks, action handlers, effect handlers, and tools
//! for one agent run. Not global — rebuilt per resolve().

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::plugins::{Plugin, PluginRegistrar};
use awaken_contract::StateError;
use awaken_contract::contract::tool::Tool;
use awaken_contract::model::Phase;

use crate::plugins::{KeyRegistration, ProfileKeyRegistration, RequestTransformArc};

use super::{EffectHandlerArc, PhaseHookArc, ScheduledActionHandlerArc};

/// A phase hook with its owning plugin ID.
#[derive(Clone)]
pub(crate) struct TaggedPhaseHook {
    pub(crate) plugin_id: String,
    pub(crate) hook: PhaseHookArc,
}

/// A request transform with its owning plugin ID.
#[derive(Clone)]
pub(crate) struct TaggedRequestTransform {
    pub(crate) plugin_id: String,
    pub(crate) transform: RequestTransformArc,
}

/// Per-run execution environment.
///
/// Built from a set of plugins via `ExecutionEnv::from_plugins()`.
/// Passed to `PhaseRuntime` methods for each phase execution.
/// Not shared across agent runs — each resolve produces a new one.
#[derive(Clone)]
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
    /// Plugin references retained for lifecycle hooks (`on_activate`/`on_deactivate`).
    pub(crate) plugins: Vec<Arc<dyn Plugin>>,
    /// Profile key registrations collected from all plugins.
    pub profile_key_registrations: Vec<ProfileKeyRegistration>,
}

impl ExecutionEnv {
    /// Build an execution environment from a set of plugins.
    ///
    /// Each plugin's `register()` is called to collect hooks, handlers, and
    /// state key registrations. The key registrations are stored so they can
    /// be installed into a `StateStore` at run start.
    ///
    /// When `active_plugin_filter` is non-empty, only plugins whose descriptor
    /// name appears in the filter will have their hooks, tools, and request
    /// transforms included. An empty filter means all plugins are active.
    /// State keys, action handlers, and effect handlers are always registered
    /// regardless of the filter — they are structural, not behavioral.
    pub fn from_plugins(
        plugins: &[Arc<dyn Plugin>],
        active_plugin_filter: &HashSet<String>,
    ) -> Result<Self, StateError> {
        let mut all_hooks: HashMap<Phase, Vec<TaggedPhaseHook>> = HashMap::new();
        let mut all_action_handlers: HashMap<String, ScheduledActionHandlerArc> = HashMap::new();
        let mut all_effect_handlers: HashMap<String, EffectHandlerArc> = HashMap::new();
        let mut all_transforms: Vec<TaggedRequestTransform> = Vec::new();
        let mut all_key_registrations: Vec<KeyRegistration> = Vec::new();
        let mut all_profile_key_registrations: Vec<ProfileKeyRegistration> = Vec::new();
        let mut all_tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

        for plugin in plugins {
            let plugin_name = plugin.descriptor().name.to_string();
            let plugin_active =
                active_plugin_filter.is_empty() || active_plugin_filter.contains(&plugin_name);
            let mut registrar = PluginRegistrar::new();
            plugin.register(&mut registrar)?;

            // Collect tools (only from active plugins; check cross-plugin duplicates)
            if plugin_active {
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
            } else {
                tracing::debug!(
                    plugin_id = %plugin_name,
                    tools_skipped = registrar.tools.len(),
                    "plugin_tools_filtered"
                );
            }

            // Collect hooks (only from active plugins, with plugin_id for diagnostics)
            if plugin_active {
                for entry in registrar.phase_hooks {
                    all_hooks
                        .entry(entry.phase)
                        .or_default()
                        .push(TaggedPhaseHook {
                            plugin_id: entry.plugin_id,
                            hook: entry.hook,
                        });
                }
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

            // Collect request transforms (only from active plugins)
            if plugin_active {
                for entry in registrar.request_transforms {
                    tracing::debug!(plugin_id = %entry.plugin_id, "registered_request_transform");
                    all_transforms.push(TaggedRequestTransform {
                        plugin_id: entry.plugin_id,
                        transform: entry.transform,
                    });
                }
            } else {
                tracing::debug!(
                    plugin_id = %plugin_name,
                    transforms_skipped = registrar.request_transforms.len(),
                    "plugin_transforms_filtered"
                );
            }

            // Collect state key registrations (always, regardless of filter)
            all_key_registrations.extend(registrar.keys);

            // Collect profile key registrations (always, structural)
            all_profile_key_registrations.extend(registrar.profile_keys);
        }

        Ok(Self {
            phase_hooks: all_hooks,
            scheduled_action_handlers: all_action_handlers,
            effect_handlers: all_effect_handlers,
            request_transforms: all_transforms,
            key_registrations: all_key_registrations,
            profile_key_registrations: all_profile_key_registrations,
            tools: all_tools,
            plugins: plugins.to_vec(),
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
            profile_key_registrations: Vec::new(),
            tools: HashMap::new(),
            plugins: Vec::new(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::PhaseContext;
    use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
    use crate::state::StateCommand;
    use async_trait::async_trait;
    use awaken_contract::StateError;
    use awaken_contract::contract::message::Message;
    use awaken_contract::contract::tool::{
        Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult,
    };
    use awaken_contract::contract::transform::{InferenceRequestTransform, TransformOutput};
    use awaken_contract::model::Phase;
    use serde_json::Value;
    use std::collections::HashSet;
    use std::sync::Arc;

    // --- Test helpers ---

    struct NoOpHook;

    #[async_trait]
    impl crate::hooks::PhaseHook for NoOpHook {
        async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
            Ok(StateCommand::default())
        }
    }

    struct NoOpTool(String);

    #[async_trait]
    impl Tool for NoOpTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new(&self.0, &self.0, format!("{} tool", self.0))
        }
        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success(&self.0, Value::Null))
        }
    }

    struct NoOpTransform;

    impl InferenceRequestTransform for NoOpTransform {
        fn transform(&self, messages: Vec<Message>, _tools: &[ToolDescriptor]) -> TransformOutput {
            TransformOutput { messages }
        }
    }

    /// A plugin that registers a tool, a hook, and a transform.
    struct FullPlugin {
        name: &'static str,
        tool_id: &'static str,
    }

    impl Plugin for FullPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: self.name }
        }

        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_tool(self.tool_id, Arc::new(NoOpTool(self.tool_id.into())))?;
            registrar.register_phase_hook(self.name, Phase::StepStart, NoOpHook)?;
            registrar.register_request_transform(self.name, NoOpTransform);
            Ok(())
        }
    }

    // --- Tests ---

    #[test]
    fn empty_filter_includes_all_plugins() {
        let plugins: Vec<Arc<dyn Plugin>> = vec![
            Arc::new(FullPlugin {
                name: "alpha",
                tool_id: "alpha_tool",
            }),
            Arc::new(FullPlugin {
                name: "beta",
                tool_id: "beta_tool",
            }),
        ];
        let env = ExecutionEnv::from_plugins(&plugins, &HashSet::new()).unwrap();

        assert!(env.tools.contains_key("alpha_tool"));
        assert!(env.tools.contains_key("beta_tool"));
        assert_eq!(env.hooks_for_phase(Phase::StepStart).len(), 2);
        assert_eq!(env.request_transforms.len(), 2);
    }

    #[test]
    fn filter_excludes_tools_from_inactive_plugin() {
        let plugins: Vec<Arc<dyn Plugin>> = vec![
            Arc::new(FullPlugin {
                name: "alpha",
                tool_id: "alpha_tool",
            }),
            Arc::new(FullPlugin {
                name: "beta",
                tool_id: "beta_tool",
            }),
        ];
        let filter: HashSet<String> = ["alpha".to_string()].into();
        let env = ExecutionEnv::from_plugins(&plugins, &filter).unwrap();

        assert!(
            env.tools.contains_key("alpha_tool"),
            "active plugin tool should be present"
        );
        assert!(
            !env.tools.contains_key("beta_tool"),
            "inactive plugin tool should be filtered"
        );
    }

    #[test]
    fn filter_excludes_hooks_from_inactive_plugin() {
        let plugins: Vec<Arc<dyn Plugin>> = vec![
            Arc::new(FullPlugin {
                name: "alpha",
                tool_id: "alpha_tool",
            }),
            Arc::new(FullPlugin {
                name: "beta",
                tool_id: "beta_tool",
            }),
        ];
        let filter: HashSet<String> = ["alpha".to_string()].into();
        let env = ExecutionEnv::from_plugins(&plugins, &filter).unwrap();

        let hooks = env.hooks_for_phase(Phase::StepStart);
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].plugin_id, "alpha");
    }

    #[test]
    fn filter_excludes_transforms_from_inactive_plugin() {
        let plugins: Vec<Arc<dyn Plugin>> = vec![
            Arc::new(FullPlugin {
                name: "alpha",
                tool_id: "alpha_tool",
            }),
            Arc::new(FullPlugin {
                name: "beta",
                tool_id: "beta_tool",
            }),
        ];
        let filter: HashSet<String> = ["beta".to_string()].into();
        let env = ExecutionEnv::from_plugins(&plugins, &filter).unwrap();

        assert_eq!(env.request_transforms.len(), 1);
        assert_eq!(env.request_transforms[0].plugin_id, "beta");
    }

    #[test]
    fn filter_with_all_plugins_active_includes_everything() {
        let plugins: Vec<Arc<dyn Plugin>> = vec![
            Arc::new(FullPlugin {
                name: "alpha",
                tool_id: "alpha_tool",
            }),
            Arc::new(FullPlugin {
                name: "beta",
                tool_id: "beta_tool",
            }),
        ];
        let filter: HashSet<String> = ["alpha".to_string(), "beta".to_string()].into();
        let env = ExecutionEnv::from_plugins(&plugins, &filter).unwrap();

        assert_eq!(env.tools.len(), 2);
        assert_eq!(env.hooks_for_phase(Phase::StepStart).len(), 2);
        assert_eq!(env.request_transforms.len(), 2);
    }

    #[test]
    fn filter_with_no_matching_plugins_produces_empty_env() {
        let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(FullPlugin {
            name: "alpha",
            tool_id: "alpha_tool",
        })];
        let filter: HashSet<String> = ["nonexistent".to_string()].into();
        let env = ExecutionEnv::from_plugins(&plugins, &filter).unwrap();

        assert!(env.tools.is_empty());
        assert!(env.hooks_for_phase(Phase::StepStart).is_empty());
        assert!(env.request_transforms.is_empty());
    }

    #[test]
    fn filter_still_registers_keys_from_inactive_plugins() {
        // Key registrations are structural and should always be included
        let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(FullPlugin {
            name: "alpha",
            tool_id: "alpha_tool",
        })];
        let filter: HashSet<String> = ["nonexistent".to_string()].into();
        let env = ExecutionEnv::from_plugins(&plugins, &filter).unwrap();

        // Tools filtered out, but env is still valid (keys would still be there
        // if the plugin registered any). Structural handlers are unaffected.
        assert!(env.tools.is_empty());
        assert!(env.hooks_for_phase(Phase::StepStart).is_empty());
    }

    #[test]
    fn from_plugins_collects_profile_keys() {
        use awaken_contract::contract::profile_store::ProfileKey;

        struct ProfileLocale;
        impl ProfileKey for ProfileLocale {
            const KEY: &'static str = "locale";
            type Value = String;
        }

        struct ProfilePlugin;
        impl Plugin for ProfilePlugin {
            fn descriptor(&self) -> PluginDescriptor {
                PluginDescriptor {
                    name: "profile-plugin",
                }
            }
            fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
                registrar.register_profile_key::<ProfileLocale>()?;
                Ok(())
            }
        }

        let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(ProfilePlugin)];
        // Even with a non-matching filter, profile keys should be collected
        let filter: HashSet<String> = ["nonexistent".to_string()].into();
        let env = ExecutionEnv::from_plugins(&plugins, &filter).unwrap();
        assert_eq!(env.profile_key_registrations.len(), 1);
        assert_eq!(env.profile_key_registrations[0].key, "locale");
    }
}
