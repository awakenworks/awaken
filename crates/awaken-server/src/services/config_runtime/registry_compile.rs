use std::collections::HashMap;
use std::sync::Arc;

use awaken_contract::{AgentSpec, ModelPoolSpec, ModelSpec, ProviderSpec};
use awaken_runtime::registry::memory::{
    MapAgentSpecRegistry, MapModelRegistry, MapProviderRegistry,
};
use awaken_runtime::registry::{AgentSpecRegistry, RegistrySet, ToolRegistry};

use super::{
    AgentSpecRegistryWithDiscovery, ConfigRuntimeError, ConfigRuntimeManager, ProviderExecutorCache,
};

impl ConfigRuntimeManager {
    pub(super) fn compile_registry_set(
        &self,
        providers: &[ProviderSpec],
        models: &[ModelSpec],
        pools: &[ModelPoolSpec],
        agents: &[AgentSpec],
        tool_specs: &[awaken_contract::ToolSpec],
        dynamic_tools: Option<Arc<dyn ToolRegistry>>,
    ) -> Result<(RegistrySet, ProviderExecutorCache), ConfigRuntimeError> {
        let mut provider_registry = MapProviderRegistry::new();
        let mut next_cache: ProviderExecutorCache = HashMap::with_capacity(providers.len());
        let prior_cache = self.provider_executor_cache.lock().clone();
        for provider in providers {
            let executor = match prior_cache.get(&provider.id) {
                Some((cached_spec, cached_executor)) if cached_spec == provider => {
                    Arc::clone(cached_executor)
                }
                _ => self.provider_factory.build(provider)?,
            };
            next_cache.insert(
                provider.id.clone(),
                (provider.clone(), Arc::clone(&executor)),
            );
            provider_registry
                .register_provider(provider.id.clone(), executor)
                .map_err(|error| ConfigRuntimeError::InvalidConfig(error.to_string()))?;
        }

        let mut model_registry = MapModelRegistry::new();
        for model in models {
            model_registry
                .register_model(model.clone())
                .map_err(|error| ConfigRuntimeError::InvalidConfig(error.to_string()))?;
        }
        for pool in pools {
            model_registry
                .register_model_pool(pool.clone())
                .map_err(|error| ConfigRuntimeError::InvalidConfig(error.to_string()))?;
        }

        let mut local_agents = MapAgentSpecRegistry::new();
        for agent in agents {
            local_agents
                .register_spec(agent.clone())
                .map_err(|error| ConfigRuntimeError::InvalidConfig(error.to_string()))?;
        }

        let local_agents: Arc<dyn AgentSpecRegistry> = Arc::new(local_agents);
        let agents = match &self.discovered_agents {
            Some(fallback) => Arc::new(AgentSpecRegistryWithDiscovery::new(
                local_agents,
                Arc::clone(fallback),
            )) as Arc<dyn AgentSpecRegistry>,
            None => local_agents,
        };

        let overrides: HashMap<String, String> = tool_specs
            .iter()
            .filter_map(|spec| {
                let live = self.tools.get_tool(&spec.id)?;
                if live.descriptor().description != spec.description {
                    Some((spec.id.clone(), spec.description.clone()))
                } else {
                    None
                }
            })
            .collect();
        let tools = self.compose_tool_registry(dynamic_tools, overrides)?;

        Ok((
            RegistrySet {
                agents,
                tools,
                models: Arc::new(model_registry),
                providers: Arc::new(provider_registry),
                plugins: Arc::clone(&self.plugins),
                backends: Arc::clone(&self.backends),
            },
            next_cache,
        ))
    }
}
