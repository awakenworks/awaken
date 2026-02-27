use super::agent_tools::{
    AGENT_RECOVERY_PLUGIN_ID, AGENT_TOOLS_PLUGIN_ID, SCOPE_CALLER_AGENT_ID_KEY,
};
use super::policy::{filter_tools_in_place, set_scope_filters_from_definition_if_absent};
use super::stop_policy_plugin::{StopConditionSpec, StopPolicyPlugin, STOP_POLICY_PLUGIN_ID};
use super::*;
use crate::extensions::skills::{
    InMemorySkillRegistry, Skill, SkillRegistry, SKILLS_BUNDLE_ID, SKILLS_DISCOVERY_PLUGIN_ID,
    SKILLS_PLUGIN_ID, SKILLS_RUNTIME_PLUGIN_ID,
};
use crate::runtime::loop_runner::GenaiLlmExecutor;

#[derive(Default)]
struct ResolvedBehaviors {
    global: Vec<Arc<dyn AgentBehavior>>,
    agent_default: Vec<Arc<dyn AgentBehavior>>,
}

impl ResolvedBehaviors {
    fn with_global(mut self, plugins: Vec<Arc<dyn AgentBehavior>>) -> Self {
        self.global.extend(plugins);
        self
    }

    fn with_agent_default(mut self, plugins: Vec<Arc<dyn AgentBehavior>>) -> Self {
        self.agent_default.extend(plugins);
        self
    }

    fn into_plugins(self) -> Result<Vec<Arc<dyn AgentBehavior>>, AgentOsWiringError> {
        let mut plugins = Vec::new();
        plugins.extend(self.global);
        plugins.extend(self.agent_default);
        AgentOs::ensure_unique_behavior_ids(&plugins)?;
        Ok(plugins)
    }
}

impl AgentOs {
    pub fn builder() -> AgentOsBuilder {
        AgentOsBuilder::new()
    }

    pub fn client(&self) -> Client {
        self.default_client.clone()
    }

    pub fn skill_list(&self) -> Option<Vec<Arc<dyn Skill>>> {
        self.skills_registry.as_ref().map(|registry| {
            let mut skills: Vec<Arc<dyn Skill>> = registry.snapshot().into_values().collect();
            skills.sort_by(|a, b| a.meta().id.cmp(&b.meta().id));
            skills
        })
    }

    pub(crate) fn agents_registry(&self) -> Arc<dyn AgentRegistry> {
        self.agents.clone()
    }

    pub fn agent(&self, agent_id: &str) -> Option<AgentDefinition> {
        self.agents.get(agent_id)
    }

    pub fn tools(&self) -> HashMap<String, Arc<dyn Tool>> {
        self.base_tools.snapshot()
    }

    pub(super) fn reserved_behavior_ids() -> &'static [&'static str] {
        &[
            SKILLS_PLUGIN_ID,
            SKILLS_DISCOVERY_PLUGIN_ID,
            SKILLS_RUNTIME_PLUGIN_ID,
            AGENT_TOOLS_PLUGIN_ID,
            AGENT_RECOVERY_PLUGIN_ID,
            STOP_POLICY_PLUGIN_ID,
        ]
    }

    fn reserved_skills_behavior_ids() -> &'static [&'static str] {
        &[
            SKILLS_PLUGIN_ID,
            SKILLS_DISCOVERY_PLUGIN_ID,
            SKILLS_RUNTIME_PLUGIN_ID,
        ]
    }

    fn resolve_behavior_id_list(
        &self,
        behavior_ids: &[String],
    ) -> Result<Vec<Arc<dyn AgentBehavior>>, AgentOsWiringError> {
        let reserved = Self::reserved_behavior_ids();
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

    pub(super) fn ensure_unique_behavior_ids(
        plugins: &[Arc<dyn AgentBehavior>],
    ) -> Result<(), AgentOsWiringError> {
        // Fail-fast: plugins are keyed by `AgentBehavior::id()`. Duplicates are almost always a bug.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for p in plugins {
            let id = p.id().to_string();
            if !seen.insert(id.clone()) {
                return Err(AgentOsWiringError::BehaviorAlreadyInstalled(id));
            }
        }
        Ok(())
    }

    fn ensure_skills_plugin_not_installed(
        plugins: &[Arc<dyn AgentBehavior>],
    ) -> Result<(), AgentOsWiringError> {
        let reserved = Self::reserved_skills_behavior_ids();
        if let Some(existing) = plugins
            .iter()
            .map(|p| p.id())
            .find(|id| reserved.contains(id))
        {
            return Err(AgentOsWiringError::SkillsBehaviorAlreadyInstalled(
                existing.to_string(),
            ));
        }
        Ok(())
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

    fn build_skills_plugins(&self, registry: Arc<dyn SkillRegistry>) -> Vec<Arc<dyn AgentBehavior>> {
        match self.skills_config.mode {
            SkillsMode::Disabled => Vec::new(),
            SkillsMode::DiscoveryAndRuntime => {
                let discovery = SkillDiscoveryPlugin::new(registry).with_limits(
                    self.skills_config.discovery_max_entries,
                    self.skills_config.discovery_max_chars,
                );
                vec![SkillPlugin::new(discovery).boxed()]
            }
            SkillsMode::DiscoveryOnly => {
                let discovery = SkillDiscoveryPlugin::new(registry).with_limits(
                    self.skills_config.discovery_max_entries,
                    self.skills_config.discovery_max_chars,
                );
                vec![Arc::new(discovery)]
            }
            SkillsMode::RuntimeOnly => vec![Arc::new(SkillRuntimePlugin::new())],
        }
    }

    fn freeze_agent_registry(&self) -> Arc<dyn AgentRegistry> {
        let mut frozen = InMemoryAgentRegistry::new();
        frozen.extend_upsert(self.agents.snapshot());
        Arc::new(frozen)
    }

    fn freeze_skill_registry(&self) -> Option<Arc<dyn SkillRegistry>> {
        self.skills_registry.as_ref().map(|registry| {
            let mut frozen = InMemorySkillRegistry::new();
            frozen.extend_upsert(registry.snapshot().into_values().collect());
            Arc::new(frozen) as Arc<dyn SkillRegistry>
        })
    }

    fn with_registry_overrides(
        &self,
        agents: Arc<dyn AgentRegistry>,
        skills_registry: Option<Arc<dyn SkillRegistry>>,
    ) -> Self {
        let mut cloned = self.clone();
        cloned.agents = agents;
        cloned.skills_registry = skills_registry;
        cloned
    }

    fn build_skills_wiring_bundles(
        &self,
        resolved_plugins: &[Arc<dyn AgentBehavior>],
        skills_registry: Option<Arc<dyn SkillRegistry>>,
    ) -> Result<Vec<Arc<dyn RegistryBundle>>, AgentOsWiringError> {
        if self.skills_config.mode == SkillsMode::Disabled {
            return Ok(Vec::new());
        }

        Self::ensure_skills_plugin_not_installed(resolved_plugins)?;
        let registry = skills_registry.ok_or(AgentOsWiringError::SkillsNotConfigured)?;

        let subsystem = SkillSubsystem::new(registry.clone());
        let mut tool_defs = HashMap::new();
        subsystem
            .extend_tools(&mut tool_defs)
            .map_err(|e| match e {
                SkillSubsystemError::ToolIdConflict(id) => {
                    AgentOsWiringError::SkillsToolIdConflict(id)
                }
            })?;

        let mut bundle = ToolBehaviorBundle::new(SKILLS_BUNDLE_ID).with_tools(tool_defs);
        for plugin in self.build_skills_plugins(registry) {
            bundle = bundle.with_behavior(plugin);
        }
        Ok(vec![Arc::new(bundle)])
    }

    fn build_agent_tool_wiring_bundles(
        &self,
        resolved_plugins: &[Arc<dyn AgentBehavior>],
        agents_registry: Arc<dyn AgentRegistry>,
        skills_registry: Option<Arc<dyn SkillRegistry>>,
    ) -> Result<Vec<Arc<dyn RegistryBundle>>, AgentOsWiringError> {
        Self::ensure_agent_tools_plugin_not_installed(resolved_plugins)?;
        let pinned_os = self.with_registry_overrides(agents_registry.clone(), skills_registry);

        let run_tool: Arc<dyn Tool> =
            Arc::new(AgentRunTool::new(pinned_os, self.agent_runs.clone()));
        let stop_tool: Arc<dyn Tool> = Arc::new(AgentStopTool::new(self.agent_runs.clone()));

        let tools_plugin = AgentToolsPlugin::new(agents_registry, self.agent_runs.clone())
            .with_limits(
                self.agent_tools.discovery_max_entries,
                self.agent_tools.discovery_max_chars,
            );
        let recovery_plugin = AgentRecoveryPlugin::new(self.agent_runs.clone());

        let tools_bundle: Arc<dyn RegistryBundle> = Arc::new(
            ToolBehaviorBundle::new(AGENT_TOOLS_PLUGIN_ID)
                .with_tool(run_tool)
                .with_tool(stop_tool)
                .with_behavior(Arc::new(tools_plugin)),
        );
        let recovery_bundle: Arc<dyn RegistryBundle> = Arc::new(
            ToolBehaviorBundle::new(AGENT_RECOVERY_PLUGIN_ID).with_behavior(Arc::new(recovery_plugin)),
        );

        Ok(vec![tools_bundle, recovery_bundle])
    }

    fn merge_wiring_bundles(
        &self,
        bundles: &[Arc<dyn RegistryBundle>],
        tools: &mut HashMap<String, Arc<dyn Tool>>,
    ) -> Result<Vec<Arc<dyn AgentBehavior>>, AgentOsWiringError> {
        let mut plugins = Vec::new();
        for bundle in bundles {
            Self::validate_wiring_bundle(bundle.as_ref())?;
            Self::merge_wiring_bundle_tools(bundle.as_ref(), tools)?;
            let mut bundle_plugins = Self::collect_wiring_bundle_behaviors(bundle.as_ref())?;
            plugins.append(&mut bundle_plugins);
        }
        Self::ensure_unique_behavior_ids(&plugins)?;
        Ok(plugins)
    }

    fn validate_wiring_bundle(bundle: &dyn RegistryBundle) -> Result<(), AgentOsWiringError> {
        let unsupported = [
            (
                !bundle.agent_definitions().is_empty(),
                "agent_definitions".to_string(),
            ),
            (
                !bundle.agent_registries().is_empty(),
                "agent_registries".to_string(),
            ),
            (
                !bundle.provider_definitions().is_empty(),
                "provider_definitions".to_string(),
            ),
            (
                !bundle.provider_registries().is_empty(),
                "provider_registries".to_string(),
            ),
            (
                !bundle.model_definitions().is_empty(),
                "model_definitions".to_string(),
            ),
            (
                !bundle.model_registries().is_empty(),
                "model_registries".to_string(),
            ),
        ];
        if let Some((_, kind)) = unsupported.into_iter().find(|(has, _)| *has) {
            return Err(AgentOsWiringError::BundleUnsupportedContribution {
                bundle_id: bundle.id().to_string(),
                kind,
            });
        }
        Ok(())
    }

    fn merge_wiring_bundle_tools(
        bundle: &dyn RegistryBundle,
        tools: &mut HashMap<String, Arc<dyn Tool>>,
    ) -> Result<(), AgentOsWiringError> {
        let mut defs: Vec<(String, Arc<dyn Tool>)> =
            bundle.tool_definitions().into_iter().collect();
        defs.sort_by(|a, b| a.0.cmp(&b.0));
        for (id, tool) in defs {
            if tools.contains_key(&id) {
                return Err(Self::wiring_tool_conflict(bundle.id(), id));
            }
            tools.insert(id, tool);
        }

        for reg in bundle.tool_registries() {
            let mut ids = reg.ids();
            ids.sort();
            for id in ids {
                let Some(tool) = reg.get(&id) else {
                    continue;
                };
                if tools.contains_key(&id) {
                    return Err(Self::wiring_tool_conflict(bundle.id(), id));
                }
                tools.insert(id, tool);
            }
        }
        Ok(())
    }

    fn collect_wiring_bundle_behaviors(
        bundle: &dyn RegistryBundle,
    ) -> Result<Vec<Arc<dyn AgentBehavior>>, AgentOsWiringError> {
        let mut out = Vec::new();

        let mut defs: Vec<(String, Arc<dyn AgentBehavior>)> =
            bundle.behavior_definitions().into_iter().collect();
        defs.sort_by(|a, b| a.0.cmp(&b.0));
        for (key, behavior) in defs {
            let behavior_id = behavior.id().to_string();
            if key != behavior_id {
                return Err(AgentOsWiringError::BundleBehaviorIdMismatch {
                    bundle_id: bundle.id().to_string(),
                    key,
                    behavior_id,
                });
            }
            out.push(behavior);
        }

        for reg in bundle.behavior_registries() {
            let mut ids = reg.ids();
            ids.sort();
            for id in ids {
                let Some(behavior) = reg.get(&id) else {
                    continue;
                };
                if id != behavior.id() {
                    return Err(AgentOsWiringError::BundleBehaviorIdMismatch {
                        bundle_id: bundle.id().to_string(),
                        key: id,
                        behavior_id: behavior.id().to_string(),
                    });
                }
                out.push(behavior);
            }
        }

        Ok(out)
    }

    fn wiring_tool_conflict(bundle_id: &str, id: String) -> AgentOsWiringError {
        if bundle_id == SKILLS_BUNDLE_ID {
            return AgentOsWiringError::SkillsToolIdConflict(id);
        }
        if bundle_id == AGENT_TOOLS_PLUGIN_ID || bundle_id == AGENT_RECOVERY_PLUGIN_ID {
            return AgentOsWiringError::AgentToolIdConflict(id);
        }
        AgentOsWiringError::BundleToolIdConflict {
            bundle_id: bundle_id.to_string(),
            id,
        }
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

    pub fn wire_into(
        &self,
        definition: AgentDefinition,
        tools: &mut HashMap<String, Arc<dyn Tool>>,
    ) -> Result<BaseAgent, AgentOsWiringError> {
        // Resolve plugins: system bundles (skills, agent_tools, agent_recovery) -> behavior_ids
        let resolved_plugins = self.resolve_behavior_id_list(&definition.behavior_ids)?;
        let frozen_agents = self.freeze_agent_registry();
        let frozen_skills = self.freeze_skill_registry();

        let mut system_bundles =
            self.build_skills_wiring_bundles(&resolved_plugins, frozen_skills.clone())?;
        system_bundles.extend(self.build_agent_tool_wiring_bundles(
            &resolved_plugins,
            frozen_agents,
            frozen_skills,
        )?);
        let system_plugins = self.merge_wiring_bundles(&system_bundles, tools)?;
        let mut all_plugins = ResolvedBehaviors::default()
            .with_global(system_plugins)
            .with_agent_default(resolved_plugins)
            .into_plugins()?;

        // Resolve stop conditions from stop_condition_ids
        let stop_conditions =
            self.resolve_stop_condition_id_list(&definition.stop_condition_ids)?;
        let specs = synthesize_stop_specs(&definition);
        let stop_plugin = StopPolicyPlugin::new(stop_conditions, specs);
        if !stop_plugin.is_empty() {
            all_plugins.push(Arc::new(stop_plugin));
            AgentOs::ensure_unique_behavior_ids(&all_plugins)?;
        }

        Ok(definition.into_loop_config(all_plugins))
    }

    fn resolve_model(&self, cfg: &mut BaseAgent) -> Result<(), AgentOsResolveError> {
        if self.models.is_empty() {
            cfg.llm_executor = Some(Arc::new(GenaiLlmExecutor::new(self.default_client.clone())));
            return Ok(());
        }

        let Some(def) = self.models.get(&cfg.model) else {
            return Err(AgentOsResolveError::ModelNotFound(cfg.model.clone()));
        };

        let Some(client) = self.providers.get(&def.provider) else {
            return Err(AgentOsResolveError::ProviderNotFound {
                provider_id: def.provider.clone(),
                model_id: cfg.model.clone(),
            });
        };

        cfg.model = def.model;
        if let Some(opts) = def.chat_options {
            cfg.chat_options = Some(opts);
        }
        cfg.llm_executor = Some(Arc::new(GenaiLlmExecutor::new(client)));
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn wire_skills_into(
        &self,
        definition: AgentDefinition,
        tools: &mut HashMap<String, Arc<dyn Tool>>,
    ) -> Result<BaseAgent, AgentOsWiringError> {
        let resolved_plugins = self.resolve_behavior_id_list(&definition.behavior_ids)?;
        let skills_bundles =
            self.build_skills_wiring_bundles(&resolved_plugins, self.freeze_skill_registry())?;
        let skills_plugins = self.merge_wiring_bundles(&skills_bundles, tools)?;
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
            AgentOs::ensure_unique_behavior_ids(&all_plugins)?;
        }

        Ok(definition.into_loop_config(all_plugins))
    }

    /// Check whether an agent with the given ID is registered.
    pub fn validate_agent(&self, agent_id: &str) -> Result<(), AgentOsResolveError> {
        if self.agents.get(agent_id).is_some() {
            Ok(())
        } else {
            Err(AgentOsResolveError::AgentNotFound(agent_id.to_string()))
        }
    }

    /// Resolve an agent's static wiring: config, tools, and run config.
    ///
    /// This performs all one-time resolution (tool filtering, model lookup,
    /// plugin wiring) and returns a [`ResolvedRun`] that can be inspected
    /// or mutated before execution.
    pub fn resolve(&self, agent_id: &str) -> Result<ResolvedRun, AgentOsResolveError> {
        let definition = self
            .agents
            .get(agent_id)
            .ok_or_else(|| AgentOsResolveError::AgentNotFound(agent_id.to_string()))?;

        let mut run_config = crate::contracts::RunConfig::new();
        let _ = run_config.set(SCOPE_CALLER_AGENT_ID_KEY, agent_id.to_string());
        let _ = set_scope_filters_from_definition_if_absent(&mut run_config, &definition);

        let allowed_tools = definition.allowed_tools.clone();
        let excluded_tools = definition.excluded_tools.clone();
        let mut tools = self.base_tools.snapshot();
        let mut cfg = self.wire_into(definition, &mut tools)?;
        filter_tools_in_place(
            &mut tools,
            allowed_tools.as_deref(),
            excluded_tools.as_deref(),
        );
        self.resolve_model(&mut cfg)?;
        Ok(ResolvedRun {
            agent: cfg,
            tools,
            run_config,
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
