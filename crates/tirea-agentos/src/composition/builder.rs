use super::*;
use crate::contracts::runtime::tool_call::Tool;
use crate::contracts::runtime::AgentBehavior;
use crate::contracts::storage::ThreadStore;
#[cfg(feature = "skills")]
use crate::extensions::skills::{
    CompositeSkillRegistry, InMemorySkillRegistry, Skill, SkillRegistry, SkillRegistryManagerError,
};
use crate::runtime::resolve::SkillsSystemWiring;
use crate::runtime::StopPolicy;
use crate::runtime::{AgentOs, RuntimeServices};
use genai::Client;
use std::collections::HashMap;
use std::sync::Arc;
#[cfg(feature = "skills")]
use std::time::Duration;

pub struct AgentOsBuilder {
    pub(crate) client: Option<Client>,
    pub(crate) bundles: Vec<Arc<dyn RegistryBundle>>,
    pub(crate) agents: HashMap<String, AgentDefinition>,
    pub(crate) agent_registries: Vec<Arc<dyn AgentRegistry>>,
    pub(crate) base_tools: HashMap<String, Arc<dyn Tool>>,
    pub(crate) base_tool_registries: Vec<Arc<dyn ToolRegistry>>,
    pub(crate) behaviors: HashMap<String, Arc<dyn AgentBehavior>>,
    pub(crate) behavior_registries: Vec<Arc<dyn BehaviorRegistry>>,
    pub(crate) stop_policies: HashMap<String, Arc<dyn StopPolicy>>,
    pub(crate) stop_policy_registries: Vec<Arc<dyn StopPolicyRegistry>>,
    pub(crate) providers: HashMap<String, Client>,
    pub(crate) provider_registries: Vec<Arc<dyn ProviderRegistry>>,
    pub(crate) models: HashMap<String, ModelDefinition>,
    pub(crate) model_registries: Vec<Arc<dyn ModelRegistry>>,
    #[cfg(feature = "skills")]
    pub(crate) skills: Vec<Arc<dyn Skill>>,
    #[cfg(feature = "skills")]
    pub(crate) skill_registries: Vec<Arc<dyn SkillRegistry>>,
    #[cfg(feature = "skills")]
    pub(crate) skills_refresh_interval: Option<Duration>,
    #[cfg(feature = "skills")]
    pub(crate) skills_config: SkillsConfig,
    pub(crate) system_wirings: Vec<Arc<dyn SystemWiring>>,
    pub(crate) agent_tools: AgentToolsConfig,
    pub(crate) agent_state_store: Option<Arc<dyn ThreadStore>>,
}

fn merge_registry<R: ?Sized, M>(
    memory: M,
    mut external: Vec<Arc<R>>,
    is_memory_empty: impl Fn(&M) -> bool,
    into_memory_registry: impl FnOnce(M) -> Arc<R>,
    compose: impl FnOnce(Vec<Arc<R>>) -> Result<Arc<R>, AgentOsBuildError>,
) -> Result<Arc<R>, AgentOsBuildError> {
    if external.is_empty() {
        return Ok(into_memory_registry(memory));
    }

    let mut registries: Vec<Arc<R>> = Vec::new();
    if !is_memory_empty(&memory) {
        registries.push(into_memory_registry(memory));
    }
    registries.append(&mut external);

    if registries.len() == 1 {
        Ok(registries.pop().expect("single registry must exist"))
    } else {
        compose(registries)
    }
}

impl std::fmt::Debug for AgentOs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("AgentOs");
        s.field("default_client", &"[genai::Client]")
            .field("agents", &self.agents.len())
            .field("base_tools", &self.base_tools.len())
            .field("behaviors", &self.behaviors.len())
            .field("stop_policies", &self.stop_policies.len())
            .field("providers", &self.providers.len())
            .field("models", &self.models.len());
        #[cfg(feature = "skills")]
        s.field(
            "skills",
            &self.skills_registry.as_ref().map(|registry| registry.len()),
        );
        s.field("system_wirings", &self.system_wirings.len())
            .field("active_runs", &"[internal]")
            .field("agent_tools", &self.agent_tools)
            .field("agent_state_store", &self.agent_state_store.is_some())
            .finish()
    }
}

impl std::fmt::Debug for AgentOsBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("AgentOsBuilder");
        s.field("client", &self.client.is_some())
            .field("bundles", &self.bundles.len())
            .field("agents", &self.agents.len())
            .field("base_tools", &self.base_tools.len())
            .field("behaviors", &self.behaviors.len())
            .field("stop_policies", &self.stop_policies.len())
            .field("stop_policy_registries", &self.stop_policy_registries.len())
            .field("providers", &self.providers.len())
            .field("models", &self.models.len());
        #[cfg(feature = "skills")]
        {
            s.field("skills", &self.skills.len())
                .field("skill_registries", &self.skill_registries.len())
                .field("skills_refresh_interval", &self.skills_refresh_interval)
                .field("skills_config", &self.skills_config);
        }
        s.field("system_wirings", &self.system_wirings.len())
            .field("agent_tools", &self.agent_tools)
            .field("agent_state_store", &self.agent_state_store.is_some())
            .finish()
    }
}

impl AgentOsBuilder {
    pub fn new() -> Self {
        Self {
            client: None,
            bundles: Vec::new(),
            agents: HashMap::new(),
            agent_registries: Vec::new(),
            base_tools: HashMap::new(),
            base_tool_registries: Vec::new(),
            behaviors: HashMap::new(),
            behavior_registries: Vec::new(),
            stop_policies: HashMap::new(),
            stop_policy_registries: Vec::new(),
            providers: HashMap::new(),
            provider_registries: Vec::new(),
            models: HashMap::new(),
            model_registries: Vec::new(),
            #[cfg(feature = "skills")]
            skills: Vec::new(),
            #[cfg(feature = "skills")]
            skill_registries: Vec::new(),
            #[cfg(feature = "skills")]
            skills_refresh_interval: None,
            #[cfg(feature = "skills")]
            skills_config: SkillsConfig::default(),
            system_wirings: Vec::new(),
            agent_tools: AgentToolsConfig::default(),
            agent_state_store: None,
        }
    }

    pub fn with_client(mut self, client: Client) -> Self {
        self.client = Some(client);
        self
    }

    pub fn with_bundle(mut self, bundle: Arc<dyn RegistryBundle>) -> Self {
        self.bundles.push(bundle);
        self
    }

    pub fn with_agent(mut self, agent_id: impl Into<String>, def: AgentDefinition) -> Self {
        let agent_id = agent_id.into();
        let mut def = def;
        // The registry key is the canonical id to avoid mismatches.
        def.id = agent_id.clone();
        self.agents.insert(agent_id, def);
        self
    }

    pub fn with_agent_registry(mut self, registry: Arc<dyn AgentRegistry>) -> Self {
        self.agent_registries.push(registry);
        self
    }

    pub fn with_tools(mut self, tools: HashMap<String, Arc<dyn Tool>>) -> Self {
        self.base_tools = tools;
        self
    }

    pub fn with_tool_registry(mut self, registry: Arc<dyn ToolRegistry>) -> Self {
        self.base_tool_registries.push(registry);
        self
    }

    pub fn with_registered_behavior(
        mut self,
        behavior_id: impl Into<String>,
        behavior: Arc<dyn AgentBehavior>,
    ) -> Self {
        self.behaviors.insert(behavior_id.into(), behavior);
        self
    }

    pub fn with_behavior_registry(mut self, registry: Arc<dyn BehaviorRegistry>) -> Self {
        self.behavior_registries.push(registry);
        self
    }

    pub fn with_stop_policy(mut self, id: impl Into<String>, policy: Arc<dyn StopPolicy>) -> Self {
        self.stop_policies.insert(id.into(), policy);
        self
    }

    pub fn with_stop_policy_registry(mut self, registry: Arc<dyn StopPolicyRegistry>) -> Self {
        self.stop_policy_registries.push(registry);
        self
    }

    pub fn with_provider(mut self, provider_id: impl Into<String>, client: Client) -> Self {
        self.providers.insert(provider_id.into(), client);
        self
    }

    pub fn with_provider_registry(mut self, registry: Arc<dyn ProviderRegistry>) -> Self {
        self.provider_registries.push(registry);
        self
    }

    pub fn with_model(mut self, model_id: impl Into<String>, def: ModelDefinition) -> Self {
        self.models.insert(model_id.into(), def);
        self
    }

    pub fn with_models(mut self, defs: HashMap<String, ModelDefinition>) -> Self {
        self.models = defs;
        self
    }

    pub fn with_model_registry(mut self, registry: Arc<dyn ModelRegistry>) -> Self {
        self.model_registries.push(registry);
        self
    }

    #[cfg(feature = "skills")]
    pub fn with_skills(mut self, skills: Vec<Arc<dyn Skill>>) -> Self {
        self.skills = skills;
        self
    }

    #[cfg(feature = "skills")]
    pub fn with_skill_registry(mut self, registry: Arc<dyn SkillRegistry>) -> Self {
        self.skill_registries.push(registry);
        self
    }

    #[cfg(feature = "skills")]
    pub fn with_skill_registry_refresh_interval(mut self, interval: Duration) -> Self {
        self.skills_refresh_interval = Some(interval);
        self
    }

    #[cfg(feature = "skills")]
    pub fn with_skills_config(mut self, cfg: SkillsConfig) -> Self {
        self.skills_config = cfg;
        self
    }

    /// Register a [`SystemWiring`] implementation for generic extension wiring.
    pub fn with_system_wiring(mut self, wiring: Arc<dyn SystemWiring>) -> Self {
        self.system_wirings.push(wiring);
        self
    }

    pub fn with_agent_tools_config(mut self, cfg: AgentToolsConfig) -> Self {
        self.agent_tools = cfg;
        self
    }

    pub fn with_agent_state_store(mut self, agent_state_store: Arc<dyn ThreadStore>) -> Self {
        self.agent_state_store = Some(agent_state_store);
        self
    }

    pub fn build(self) -> Result<AgentOs, AgentOsBuildError> {
        let AgentOsBuilder {
            client,
            bundles,
            agents: mut agents_defs,
            mut agent_registries,
            base_tools: mut base_tools_defs,
            mut base_tool_registries,
            behaviors: mut behavior_defs,
            mut behavior_registries,
            stop_policies: stop_policy_defs,
            stop_policy_registries,
            providers: mut provider_defs,
            mut provider_registries,
            models: mut model_defs,
            mut model_registries,
            #[cfg(feature = "skills")]
            skills,
            #[cfg(feature = "skills")]
            mut skill_registries,
            #[cfg(feature = "skills")]
            skills_refresh_interval,
            #[cfg(feature = "skills")]
            skills_config,
            system_wirings,
            agent_tools,
            agent_state_store,
        } = self;

        BundleComposer::apply(
            &bundles,
            BundleRegistryAccumulator {
                agent_definitions: &mut agents_defs,
                agent_registries: &mut agent_registries,
                tool_definitions: &mut base_tools_defs,
                tool_registries: &mut base_tool_registries,
                behavior_definitions: &mut behavior_defs,
                behavior_registries: &mut behavior_registries,
                provider_definitions: &mut provider_defs,
                provider_registries: &mut provider_registries,
                model_definitions: &mut model_defs,
                model_registries: &mut model_registries,
            },
        )?;

        // --- Skills registry setup (feature-gated) ---
        #[allow(unused_mut)]
        let mut system_wirings = system_wirings;
        #[cfg(feature = "skills")]
        let skills_registry = {
            if skills_config.enabled && skills.is_empty() && skill_registries.is_empty() {
                return Err(AgentOsBuildError::SkillsNotConfigured);
            }

            let mut in_memory_skills = InMemorySkillRegistry::new();
            in_memory_skills.extend_upsert(skills);

            let registry = if in_memory_skills.is_empty() && skill_registries.is_empty() {
                None
            } else {
                Some(merge_registry(
                    in_memory_skills,
                    std::mem::take(&mut skill_registries),
                    |reg: &InMemorySkillRegistry| reg.is_empty(),
                    |reg| Arc::new(reg),
                    |regs| Ok(Arc::new(CompositeSkillRegistry::try_new(regs)?)),
                )?)
            };

            if let (Some(r), Some(interval)) = (&registry, skills_refresh_interval) {
                match r.start_periodic_refresh(interval) {
                    Ok(()) | Err(SkillRegistryManagerError::PeriodicRefreshAlreadyRunning) => {}
                    Err(err) => return Err(err.into()),
                }
            }

            // If skills are configured+enabled, push the built-in SkillsSystemWiring.
            if skills_config.enabled {
                if let Some(ref reg) = registry {
                    system_wirings.push(Arc::new(SkillsSystemWiring::new(
                        reg.clone(),
                        skills_config.clone(),
                    )));
                }
            }

            registry
        };

        let mut base_tools = InMemoryToolRegistry::new();
        base_tools.extend_named(base_tools_defs)?;

        let base_tools: Arc<dyn ToolRegistry> = merge_registry(
            base_tools,
            base_tool_registries,
            |reg: &InMemoryToolRegistry| reg.is_empty(),
            |reg| Arc::new(reg),
            |regs| Ok(Arc::new(CompositeToolRegistry::try_new(regs)?)),
        )?;

        let mut behaviors = InMemoryBehaviorRegistry::new();
        behaviors.extend_named(behavior_defs)?;

        let behaviors: Arc<dyn BehaviorRegistry> = merge_registry(
            behaviors,
            behavior_registries,
            |reg: &InMemoryBehaviorRegistry| reg.is_empty(),
            |reg| Arc::new(reg),
            |regs| Ok(Arc::new(CompositeBehaviorRegistry::try_new(regs)?)),
        )?;

        let mut stop_policies_mem = InMemoryStopPolicyRegistry::new();
        stop_policies_mem.extend_named(stop_policy_defs)?;

        let stop_policies: Arc<dyn StopPolicyRegistry> = merge_registry(
            stop_policies_mem,
            stop_policy_registries,
            |reg: &InMemoryStopPolicyRegistry| reg.is_empty(),
            |reg| Arc::new(reg),
            |regs| Ok(Arc::new(CompositeStopPolicyRegistry::try_new(regs)?)),
        )?;

        // Fail-fast for builder-provided agents (external registries may be dynamic).
        {
            let reserved = AgentOs::reserved_behavior_ids(&system_wirings);
            for (agent_id, def) in &agents_defs {
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                for id in &def.behavior_ids {
                    let id = id.trim();
                    if id.is_empty() {
                        return Err(AgentOsBuildError::AgentEmptyBehaviorRef {
                            agent_id: agent_id.clone(),
                        });
                    }
                    if reserved.contains(&id) {
                        return Err(AgentOsBuildError::AgentReservedBehaviorId {
                            agent_id: agent_id.clone(),
                            behavior_id: id.to_string(),
                        });
                    }
                    if !seen.insert(id.to_string()) {
                        return Err(AgentOsBuildError::AgentDuplicateBehaviorRef {
                            agent_id: agent_id.clone(),
                            behavior_id: id.to_string(),
                        });
                    }
                    if behaviors.get(id).is_none() {
                        return Err(AgentOsBuildError::AgentBehaviorNotFound {
                            agent_id: agent_id.clone(),
                            behavior_id: id.to_string(),
                        });
                    }
                }

                // Validate stop_condition_ids
                let mut sc_seen: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                for sc_id in &def.stop_condition_ids {
                    let sc_id = sc_id.trim();
                    if sc_id.is_empty() {
                        return Err(AgentOsBuildError::AgentEmptyStopConditionRef {
                            agent_id: agent_id.clone(),
                        });
                    }
                    if !sc_seen.insert(sc_id.to_string()) {
                        return Err(AgentOsBuildError::AgentDuplicateStopConditionRef {
                            agent_id: agent_id.clone(),
                            stop_condition_id: sc_id.to_string(),
                        });
                    }
                    if stop_policies.get(sc_id).is_none() {
                        return Err(AgentOsBuildError::AgentStopConditionNotFound {
                            agent_id: agent_id.clone(),
                            stop_condition_id: sc_id.to_string(),
                        });
                    }
                }
            }
        }

        let mut providers = InMemoryProviderRegistry::new();
        providers.extend(provider_defs)?;

        let providers: Arc<dyn ProviderRegistry> = merge_registry(
            providers,
            provider_registries,
            |reg: &InMemoryProviderRegistry| reg.is_empty(),
            |reg| Arc::new(reg),
            |regs| Ok(Arc::new(CompositeProviderRegistry::try_new(regs)?)),
        )?;

        let mut models = InMemoryModelRegistry::new();
        models.extend(model_defs.clone())?;

        let models: Arc<dyn ModelRegistry> = merge_registry(
            models,
            model_registries,
            |reg: &InMemoryModelRegistry| reg.is_empty(),
            |reg| Arc::new(reg),
            |regs| Ok(Arc::new(CompositeModelRegistry::try_new(regs)?)),
        )?;

        if !models.is_empty() && providers.is_empty() {
            return Err(AgentOsBuildError::ProvidersNotConfigured);
        }

        for (model_id, def) in models.snapshot() {
            if providers.get(&def.provider).is_none() {
                return Err(AgentOsBuildError::ProviderNotFound {
                    provider_id: def.provider,
                    model_id,
                });
            }
        }

        let mut agents = InMemoryAgentRegistry::new();
        agents.extend_upsert(agents_defs);

        let agents: Arc<dyn AgentRegistry> = merge_registry(
            agents,
            agent_registries,
            |reg: &InMemoryAgentRegistry| reg.is_empty(),
            |reg| Arc::new(reg),
            |regs| Ok(Arc::new(CompositeAgentRegistry::try_new(regs)?)),
        )?;

        let registries = RegistrySet::new(
            agents,
            base_tools,
            behaviors,
            providers,
            models,
            stop_policies,
            #[cfg(feature = "skills")]
            skills_registry,
        );
        let services = RuntimeServices {
            default_client: client.unwrap_or_default(),
            system_wirings,
            agent_tools,
            agent_state_store,
        };

        Ok(AgentOs::from_registry_set(registries, services))
    }
}

impl Default for AgentOsBuilder {
    fn default() -> Self {
        Self::new()
    }
}
