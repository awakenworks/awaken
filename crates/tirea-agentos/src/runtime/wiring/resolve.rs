use crate::composition::{
    AgentCatalog, AgentDefinition, AgentOsBuilder, AgentOsWiringError, AgentRegistry,
    InMemoryAgentCatalog, InMemoryAgentRegistry, RegistryBundle, StopConditionSpec, SystemWiring,
    ToolBehaviorBundle, ToolExecutionMode, WiringContext,
};
use crate::contracts::runtime::behavior::{AgentBehavior, NoOpBehavior};
use crate::contracts::runtime::tool_call::Tool;
use crate::contracts::runtime::ToolExecutor;
use crate::contracts::RunPolicy;
#[cfg(feature = "handoff")]
use crate::runtime::agent_tools::AgentHandoffTool;
use crate::runtime::agent_tools::{
    AgentOutputTool, AgentRecoveryPlugin, AgentRunTool, AgentStopTool, AgentToolsPlugin,
    AGENT_RECOVERY_PLUGIN_ID, AGENT_TOOLS_PLUGIN_ID,
};
use crate::runtime::background_tasks::{
    BackgroundTasksPlugin, TaskCancelTool, TaskOutputTool, TaskStatusTool,
    BACKGROUND_TASKS_PLUGIN_ID,
};
use crate::runtime::context::{policy_for_model, ContextPlugin, CONTEXT_PLUGIN_ID};
use crate::runtime::loop_runner::{
    BaseAgent, GenaiLlmExecutor, LlmExecutor, ParallelToolExecutor, ResolvedRun,
    SequentialToolExecutor,
};
use crate::runtime::policy::{filter_tools_in_place, set_runtime_policy_from_definition_if_absent};
use crate::runtime::stop_policy::{StopPolicyPlugin, STOP_POLICY_PLUGIN_ID};
use crate::runtime::{AgentOs, AgentOsResolveError, StopPolicy};
use genai::{chat::ChatOptions, Client};
use std::collections::HashMap;
use std::sync::Arc;
use tirea_contract::runtime::state::{StateActionDeserializerRegistry, StateScopeRegistry};
#[cfg(feature = "skills")]
use tirea_extension_skills::{InMemorySkillRegistry, Skill, SkillRegistry};
use tirea_state::LatticeRegistry;

use super::bundle_merge::ResolvedBehaviors;
use super::{ensure_unique_behavior_ids, merge_wiring_bundles, CompositeBehavior};

#[derive(Clone)]
struct ResolvedModelRuntime {
    model: String,
    chat_options: Option<ChatOptions>,
    llm_executor: Arc<dyn LlmExecutor>,
}

// ---------------------------------------------------------------------------
// AgentOs wiring implementation
// ---------------------------------------------------------------------------

impl AgentOs {
    pub fn builder() -> AgentOsBuilder {
        AgentOsBuilder::new()
    }

    pub fn client(&self) -> Client {
        self.default_client.clone()
    }

    #[cfg(feature = "skills")]
    pub fn skill_list(&self) -> Option<Vec<Arc<dyn Skill>>> {
        self.skills_registry.as_ref().map(|registry| {
            let mut skills: Vec<Arc<dyn Skill>> = registry.snapshot().into_values().collect();
            skills.sort_by(|a, b| a.meta().id.cmp(&b.meta().id));
            skills
        })
    }

    pub(crate) fn agent_catalog(&self) -> Arc<dyn AgentCatalog> {
        self.agent_catalog.clone()
    }

    pub fn agent(&self, agent_id: &str) -> Option<AgentDefinition> {
        self.agents.get(agent_id)
    }

    /// Return all registered agent ids in stable order.
    pub fn agent_ids(&self) -> Vec<String> {
        let mut ids = self.agents.ids();
        ids.sort();
        ids
    }

    pub fn tools(&self) -> HashMap<String, Arc<dyn Tool>> {
        self.base_tools.snapshot()
    }

    /// Collect reserved behavior IDs from all system wirings + internal IDs.
    pub(crate) fn reserved_behavior_ids(
        system_wirings: &[Arc<dyn SystemWiring>],
    ) -> Vec<&'static str> {
        let mut ids = vec![
            AGENT_TOOLS_PLUGIN_ID,
            AGENT_RECOVERY_PLUGIN_ID,
            BACKGROUND_TASKS_PLUGIN_ID,
            CONTEXT_PLUGIN_ID,
            STOP_POLICY_PLUGIN_ID,
        ];
        for wiring in system_wirings {
            ids.extend_from_slice(wiring.reserved_behavior_ids());
        }
        ids
    }

    fn resolve_behavior_id_list(
        &self,
        behavior_ids: &[String],
    ) -> Result<Vec<Arc<dyn AgentBehavior>>, AgentOsWiringError> {
        let reserved = Self::reserved_behavior_ids(&self.system_wirings);
        let mut out: Vec<Arc<dyn AgentBehavior>> = Vec::new();
        for id in behavior_ids {
            let id = id.trim();
            if reserved.contains(&id) {
                return Err(AgentOsWiringError::ReservedBehaviorId(id.to_string()));
            }
            let p = self
                .behaviors
                .get(id)
                .ok_or_else(|| AgentOsWiringError::BehaviorNotFound(id.to_string()))?;
            out.push(p);
        }
        Ok(out)
    }

    fn resolve_stop_condition_id_list(
        &self,
        stop_condition_ids: &[String],
    ) -> Result<Vec<Arc<dyn StopPolicy>>, AgentOsWiringError> {
        let mut out = Vec::new();
        for id in stop_condition_ids {
            let id = id.trim();
            let p = self
                .stop_policies
                .get(id)
                .ok_or_else(|| AgentOsWiringError::StopConditionNotFound(id.to_string()))?;
            out.push(p);
        }
        Ok(out)
    }

    fn ensure_agent_tools_plugin_not_installed(
        plugins: &[Arc<dyn AgentBehavior>],
    ) -> Result<(), AgentOsWiringError> {
        for existing in plugins.iter().map(|p| p.id()) {
            if existing == AGENT_TOOLS_PLUGIN_ID {
                return Err(AgentOsWiringError::AgentToolsBehaviorAlreadyInstalled(
                    existing.to_string(),
                ));
            }
            if existing == AGENT_RECOVERY_PLUGIN_ID {
                return Err(AgentOsWiringError::AgentRecoveryBehaviorAlreadyInstalled(
                    existing.to_string(),
                ));
            }
        }
        Ok(())
    }

    fn freeze_agent_registry(&self) -> Arc<dyn AgentRegistry> {
        let mut frozen = InMemoryAgentRegistry::new();
        frozen.extend_upsert(self.agents.snapshot());
        Arc::new(frozen)
    }

    fn freeze_agent_catalog(&self) -> Arc<dyn AgentCatalog> {
        let mut frozen = InMemoryAgentCatalog::new();
        frozen.extend_upsert(self.agent_catalog.snapshot());
        Arc::new(frozen)
    }

    #[cfg(feature = "skills")]
    fn freeze_skill_registry(&self) -> Option<Arc<dyn SkillRegistry>> {
        self.skills_registry.as_ref().map(|registry| {
            let mut frozen = InMemorySkillRegistry::new();
            frozen.extend_upsert(registry.snapshot().into_values().collect());
            Arc::new(frozen) as Arc<dyn SkillRegistry>
        })
    }

    fn background_task_store(&self) -> Option<Arc<crate::runtime::background_tasks::TaskStore>> {
        self.agent_state_store.as_ref().map(|store| {
            Arc::new(crate::runtime::background_tasks::TaskStore::new(
                store.clone(),
            ))
        })
    }

    fn with_registry_overrides(
        &self,
        agents: Arc<dyn AgentRegistry>,
        agent_catalog: Arc<dyn AgentCatalog>,
        #[cfg(feature = "skills")] skills_registry: Option<Arc<dyn SkillRegistry>>,
    ) -> Self {
        let mut cloned = self.clone();
        cloned.agents = agents;
        cloned.agent_catalog = agent_catalog;
        #[cfg(feature = "skills")]
        {
            cloned.skills_registry = skills_registry;
        }
        cloned
    }

    fn build_agent_tool_wiring_bundles(
        &self,
        resolved_plugins: &[Arc<dyn AgentBehavior>],
        agents_registry: Arc<dyn AgentRegistry>,
        _current_definition: &AgentDefinition,
    ) -> Result<Vec<Arc<dyn RegistryBundle>>, AgentOsWiringError> {
        Self::ensure_agent_tools_plugin_not_installed(resolved_plugins)?;

        #[cfg(feature = "skills")]
        let pinned_os = {
            let frozen_skills = self.freeze_skill_registry();
            let frozen_agent_catalog = self.freeze_agent_catalog();
            self.with_registry_overrides(
                agents_registry.clone(),
                frozen_agent_catalog,
                frozen_skills,
            )
        };
        #[cfg(not(feature = "skills"))]
        let pinned_os = {
            let frozen_agent_catalog = self.freeze_agent_catalog();
            self.with_registry_overrides(agents_registry.clone(), frozen_agent_catalog)
        };

        let run_tool: Arc<dyn Tool> = Arc::new(AgentRunTool::new(
            pinned_os.clone(),
            self.sub_agent_handles.clone(),
        ));
        let stop_tool: Arc<dyn Tool> = Arc::new(AgentStopTool::with_os(
            pinned_os.clone(),
            self.sub_agent_handles.clone(),
        ));
        let output_tool: Arc<dyn Tool> = Arc::new(AgentOutputTool::new(pinned_os));
        let task_store = self.background_task_store();
        let task_status_tool: Arc<dyn Tool> = Arc::new(
            TaskStatusTool::new(self.background_tasks.clone()).with_task_store(task_store.clone()),
        );
        let task_cancel_tool: Arc<dyn Tool> = Arc::new(
            TaskCancelTool::new(self.background_tasks.clone()).with_task_store(task_store.clone()),
        );
        let task_output_tool: Arc<dyn Tool> = Arc::new(
            TaskOutputTool::new(
                self.background_tasks.clone(),
                self.agent_state_store.clone(),
            )
            .with_task_store(task_store.clone()),
        );

        let tools_plugin =
            AgentToolsPlugin::new(self.freeze_agent_catalog(), self.sub_agent_handles.clone())
                .with_limits(
                    self.agent_tools.discovery_max_entries,
                    self.agent_tools.discovery_max_chars,
                );
        let recovery_plugin = AgentRecoveryPlugin::new(self.sub_agent_handles.clone());
        let background_tasks_plugin =
            BackgroundTasksPlugin::new(self.background_tasks.clone()).with_task_store(task_store);

        let mut agent_tools_bundle = ToolBehaviorBundle::new(AGENT_TOOLS_PLUGIN_ID)
            .with_tool(run_tool)
            .with_tool(stop_tool)
            .with_tool(output_tool);
        #[cfg(feature = "handoff")]
        {
            agent_tools_bundle =
                agent_tools_bundle.with_tool(Arc::new(AgentHandoffTool) as Arc<dyn Tool>);
        }
        let tools_bundle: Arc<dyn RegistryBundle> =
            Arc::new(agent_tools_bundle.with_behavior(Arc::new(tools_plugin)));
        let recovery_bundle: Arc<dyn RegistryBundle> = Arc::new(
            ToolBehaviorBundle::new(AGENT_RECOVERY_PLUGIN_ID)
                .with_behavior(Arc::new(recovery_plugin)),
        );
        let background_tasks_bundle: Arc<dyn RegistryBundle> = Arc::new(
            ToolBehaviorBundle::new(BACKGROUND_TASKS_PLUGIN_ID)
                .with_tool(task_status_tool)
                .with_tool(task_cancel_tool)
                .with_tool(task_output_tool)
                .with_behavior(Arc::new(background_tasks_plugin)),
        );

        let mut bundles = vec![tools_bundle, recovery_bundle, background_tasks_bundle];

        // Handoff plugin: auto-compute overlays from all agents in the registry.
        #[cfg(feature = "handoff")]
        {
            use tirea_extension_handoff::{AgentOverlay, HandoffPlugin, HANDOFF_PLUGIN_ID};

            let all_agents = agents_registry.snapshot();
            if all_agents.len() > 1 {
                let mut overlays = std::collections::HashMap::new();
                for (id, def) in &all_agents {
                    if id == &_current_definition.id {
                        continue;
                    }
                    overlays.insert(id.clone(), agent_overlay_from_definition(def));
                }
                let handoff_bundle: Arc<dyn RegistryBundle> = Arc::new(
                    ToolBehaviorBundle::new(HANDOFF_PLUGIN_ID)
                        .with_behavior(Arc::new(HandoffPlugin::new(overlays))),
                );
                bundles.push(handoff_bundle);
            }
        }

        Ok(bundles)
    }

    #[cfg(test)]
    pub(crate) fn wire_behaviors_into(
        &self,
        definition: AgentDefinition,
    ) -> Result<Vec<Arc<dyn AgentBehavior>>, AgentOsWiringError> {
        if definition.behavior_ids.is_empty() {
            return Ok(Vec::new());
        }

        let resolved_plugins = self.resolve_behavior_id_list(&definition.behavior_ids)?;
        ResolvedBehaviors::default()
            .with_agent_default(resolved_plugins)
            .into_plugins()
    }

    fn wire_into(
        &self,
        definition: AgentDefinition,
        tools: &mut HashMap<String, Arc<dyn Tool>>,
        model_runtime: &ResolvedModelRuntime,
    ) -> Result<BaseAgent, AgentOsWiringError> {
        let resolved_plugins = self.resolve_behavior_id_list(&definition.behavior_ids)?;
        let frozen_agents = self.freeze_agent_registry();

        // Run all system wirings generically.
        let wiring_ctx = WiringContext {
            resolved_behaviors: &resolved_plugins,
            existing_tools: tools,
            agent_definition: &definition,
        };
        let mut system_bundles = Vec::new();
        for wiring in &self.system_wirings {
            let bundles = wiring.wire(&wiring_ctx)?;
            system_bundles.extend(bundles);
        }

        // Agent tools stay hardcoded (internal, needs &self/AgentOs access).
        system_bundles.extend(self.build_agent_tool_wiring_bundles(
            &resolved_plugins,
            frozen_agents,
            &definition,
        )?);

        let system_plugins = merge_wiring_bundles(&system_bundles, tools)?;
        let mut all_plugins = ResolvedBehaviors::default()
            .with_global(system_plugins)
            .with_agent_default(resolved_plugins)
            .into_plugins()?;

        // Context plugin: logical compression (compaction) + hard truncation.
        let context_policy = policy_for_model(&model_runtime.model);
        all_plugins.push(Arc::new(
            ContextPlugin::new(context_policy).with_llm_summarizer(
                model_runtime.model.clone(),
                model_runtime.llm_executor.clone(),
                model_runtime.chat_options.clone(),
            ),
        ));

        // Resolve stop conditions from stop_condition_ids
        let stop_conditions =
            self.resolve_stop_condition_id_list(&definition.stop_condition_ids)?;
        let specs = synthesize_stop_specs(&definition);
        let stop_plugin = StopPolicyPlugin::new(stop_conditions, specs);
        if !stop_plugin.is_empty() {
            all_plugins.push(Arc::new(stop_plugin));
            ensure_unique_behavior_ids(&all_plugins)?;
        }

        Ok(build_base_agent_from_definition(definition, all_plugins))
    }

    fn resolve_model_runtime(
        &self,
        definition: &AgentDefinition,
    ) -> Result<ResolvedModelRuntime, AgentOsResolveError> {
        if self.models.is_empty() {
            return Ok(ResolvedModelRuntime {
                model: definition.model.clone(),
                chat_options: definition.chat_options.clone(),
                llm_executor: Arc::new(GenaiLlmExecutor::new(self.default_client.clone())),
            });
        }

        let Some(def) = self.models.get(&definition.model) else {
            return Err(AgentOsResolveError::ModelNotFound(definition.model.clone()));
        };

        let Some(client) = self.providers.get(&def.provider) else {
            return Err(AgentOsResolveError::ProviderNotFound {
                provider_id: def.provider.clone(),
                model_id: definition.model.clone(),
            });
        };

        Ok(ResolvedModelRuntime {
            model: def.model.clone(),
            chat_options: def
                .chat_options
                .clone()
                .or_else(|| definition.chat_options.clone()),
            llm_executor: Arc::new(GenaiLlmExecutor::new(client)),
        })
    }

    #[cfg(all(test, feature = "skills"))]
    pub(crate) fn wire_skills_into(
        &self,
        definition: AgentDefinition,
        tools: &mut HashMap<String, Arc<dyn Tool>>,
    ) -> Result<BaseAgent, AgentOsWiringError> {
        let resolved_plugins = self.resolve_behavior_id_list(&definition.behavior_ids)?;

        // Build skills wiring via SystemWiring iteration (only skills wirings apply).
        let wiring_ctx = WiringContext {
            resolved_behaviors: &resolved_plugins,
            existing_tools: tools,
            agent_definition: &definition,
        };
        let mut skills_bundles = Vec::new();
        for wiring in &self.system_wirings {
            let bundles = wiring.wire(&wiring_ctx)?;
            skills_bundles.extend(bundles);
        }

        let skills_plugins = merge_wiring_bundles(&skills_bundles, tools)?;
        let mut all_plugins = ResolvedBehaviors::default()
            .with_global(skills_plugins)
            .with_agent_default(resolved_plugins)
            .into_plugins()?;

        let stop_conditions =
            self.resolve_stop_condition_id_list(&definition.stop_condition_ids)?;
        let specs = synthesize_stop_specs(&definition);
        let stop_plugin = StopPolicyPlugin::new(stop_conditions, specs);
        if !stop_plugin.is_empty() {
            all_plugins.push(Arc::new(stop_plugin));
            ensure_unique_behavior_ids(&all_plugins)?;
        }

        Ok(build_base_agent_from_definition(definition, all_plugins))
    }

    /// Check whether an agent with the given ID is registered.
    pub fn validate_agent(&self, agent_id: &str) -> Result<(), AgentOsResolveError> {
        if self.agents.get(agent_id).is_some() {
            Ok(())
        } else {
            Err(AgentOsResolveError::AgentNotFound(agent_id.to_string()))
        }
    }

    /// Resolve an agent's static wiring: config, tools, and run policy.
    pub fn resolve(&self, agent_id: &str) -> Result<ResolvedRun, AgentOsResolveError> {
        let definition = self
            .agents
            .get(agent_id)
            .ok_or_else(|| AgentOsResolveError::AgentNotFound(agent_id.to_string()))?;
        self.resolve_definition(definition)
    }

    fn resolve_definition(
        &self,
        definition: AgentDefinition,
    ) -> Result<ResolvedRun, AgentOsResolveError> {
        let mut run_policy = RunPolicy::new();
        set_runtime_policy_from_definition_if_absent(&mut run_policy, &definition);

        let model_runtime = self.resolve_model_runtime(&definition)?;
        let allowed_tools = definition.allowed_tools.clone();
        let excluded_tools = definition.excluded_tools.clone();
        let mut tools = self.base_tools.snapshot();
        let mut cfg = self.wire_into(definition, &mut tools, &model_runtime)?;
        filter_tools_in_place(
            &mut tools,
            allowed_tools.as_deref(),
            excluded_tools.as_deref(),
        );
        cfg.model = model_runtime.model;
        cfg.chat_options = model_runtime.chat_options;
        cfg.llm_executor = Some(model_runtime.llm_executor);
        Ok(ResolvedRun {
            agent: cfg,
            tools,
            run_policy,
            parent_tool_call_id: None,
        })
    }
}

/// Merge explicit `stop_condition_specs` with implicit `max_rounds` from the
/// definition. If the user already declared a `MaxRounds` spec, `max_rounds`
/// is NOT added a second time.
fn synthesize_stop_specs(definition: &AgentDefinition) -> Vec<StopConditionSpec> {
    let mut specs = definition.stop_condition_specs.clone();
    let has_explicit_max_rounds = specs
        .iter()
        .any(|s| matches!(s, StopConditionSpec::MaxRounds { .. }));
    if !has_explicit_max_rounds && definition.max_rounds > 0 {
        specs.push(StopConditionSpec::MaxRounds {
            rounds: definition.max_rounds,
        });
    }
    specs
}

fn build_base_agent_from_definition(
    definition: AgentDefinition,
    behaviors: Vec<Arc<dyn AgentBehavior>>,
) -> BaseAgent {
    let definition = normalize_definition_models(definition);
    let tool_executor: Arc<dyn ToolExecutor> = match definition.tool_execution_mode {
        ToolExecutionMode::Sequential => Arc::new(SequentialToolExecutor),
        ToolExecutionMode::ParallelBatchApproval => {
            Arc::new(ParallelToolExecutor::batch_approval())
        }
        ToolExecutionMode::ParallelStreaming => Arc::new(ParallelToolExecutor::streaming()),
    };

    let mut lattice_registry = LatticeRegistry::new();
    for behavior in &behaviors {
        behavior.register_lattice_paths(&mut lattice_registry);
    }

    let mut state_scope_registry = StateScopeRegistry::new();
    for behavior in &behaviors {
        behavior.register_state_scopes(&mut state_scope_registry);
    }

    let mut state_action_deserializer_registry = StateActionDeserializerRegistry::new();
    for behavior in &behaviors {
        behavior.register_state_action_deserializers(&mut state_action_deserializer_registry);
    }

    let behavior: Arc<dyn AgentBehavior> = if behaviors.is_empty() {
        Arc::new(NoOpBehavior)
    } else {
        Arc::new(CompositeBehavior::new(definition.id.clone(), behaviors))
    };

    BaseAgent {
        id: definition.id,
        model: definition.model,
        system_prompt: definition.system_prompt,
        max_rounds: definition.max_rounds,
        tool_executor,
        chat_options: definition.chat_options,
        fallback_models: definition.fallback_models,
        llm_retry_policy: definition.llm_retry_policy,
        behavior,
        lattice_registry: Arc::new(lattice_registry),
        state_scope_registry: Arc::new(state_scope_registry),
        step_tool_provider: None,
        llm_executor: None,
        state_action_deserializer_registry: Arc::new(state_action_deserializer_registry),
    }
}

fn normalize_definition_models(mut definition: AgentDefinition) -> AgentDefinition {
    definition.model = definition.model.trim().to_string();
    definition.fallback_models = definition
        .fallback_models
        .into_iter()
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty())
        .collect();
    definition
}

/// Build an [`AgentOverlay`] from an [`AgentDefinition`], extracting all
/// overridable configuration dimensions including inference parameters
/// from `chat_options`.
#[cfg(feature = "handoff")]
fn agent_overlay_from_definition(
    def: &AgentDefinition,
) -> tirea_contract::runtime::overlay::AgentOverlay {
    use tirea_contract::runtime::inference::{InferenceOverride, ReasoningEffort};

    let (temperature, max_tokens, top_p, reasoning_effort) =
        def.chat_options
            .as_ref()
            .map_or((None, None, None, None), |opts| {
                (
                    opts.temperature,
                    opts.max_tokens,
                    opts.top_p,
                    opts.reasoning_effort.as_ref().map(|r| {
                        use genai::chat::ReasoningEffort as G;
                        match r {
                            G::None => ReasoningEffort::None,
                            G::Low | G::Minimal => ReasoningEffort::Low,
                            G::Medium => ReasoningEffort::Medium,
                            G::High => ReasoningEffort::High,
                            G::Max => ReasoningEffort::Max,
                            G::Budget(n) => ReasoningEffort::Budget(*n),
                        }
                    }),
                )
            });

    let model = if def.model.is_empty() {
        None
    } else {
        Some(def.model.clone())
    };
    let fallback_models = if def.fallback_models.is_empty() {
        None
    } else {
        Some(def.fallback_models.clone())
    };

    let has_inference = model.is_some()
        || fallback_models.is_some()
        || temperature.is_some()
        || max_tokens.is_some()
        || top_p.is_some()
        || reasoning_effort.is_some();

    tirea_contract::runtime::overlay::AgentOverlay {
        inference: if has_inference {
            Some(InferenceOverride {
                model,
                fallback_models,
                temperature,
                max_tokens,
                top_p,
                reasoning_effort,
            })
        } else {
            None
        },
        system_prompt: if def.system_prompt.is_empty() {
            None
        } else {
            Some(def.system_prompt.clone())
        },
        allowed_tools: def.allowed_tools.clone(),
        excluded_tools: def.excluded_tools.clone(),
        allowed_skills: def.allowed_skills.clone(),
        excluded_skills: def.excluded_skills.clone(),
        allowed_agents: def.allowed_agents.clone(),
        excluded_agents: def.excluded_agents.clone(),
        ..Default::default()
    }
}

#[cfg(test)]
pub(crate) fn normalize_definition_models_for_test(definition: AgentDefinition) -> AgentDefinition {
    normalize_definition_models(definition)
}
