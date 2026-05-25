use std::collections::HashMap;
use std::sync::Arc;

use awaken_contract::{AgentSpec, ModelPoolSpec, ModelSpec, ProviderSpec};
use awaken_runtime::registry::memory::{
    MapAgentSpecRegistry, MapModelRegistry, MapProviderRegistry,
};
use awaken_runtime::registry::{AgentSpecRegistry, RegistrySet, ToolRegistry};
use sha2::{Digest, Sha256};

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
                .register_provider_with_signature(
                    provider.id.clone(),
                    executor,
                    provider_definition_signature(provider),
                )
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

fn provider_definition_signature(provider: &ProviderSpec) -> String {
    let options =
        serde_json::to_string(&provider.adapter_options).unwrap_or_else(|_| "<options>".into());
    format!(
        "adapter={};base_url={:?};timeout={};credential={};options={}",
        provider.adapter,
        provider.base_url,
        provider.timeout_secs,
        provider_credential_signature(provider),
        options
    )
}

fn provider_credential_signature(provider: &ProviderSpec) -> String {
    let kind = provider
        .adapter_options
        .get("credentials_kind")
        .and_then(|value| value.as_str())
        .unwrap_or("bearer");
    let fingerprint = provider
        .api_key
        .as_ref()
        .filter(|key| !key.is_empty())
        .map(|key| {
            let digest = Sha256::digest(key.expose_secret().as_bytes());
            format!("sha256:{digest:x}")
        })
        .unwrap_or_else(|| "none".to_string());
    format!("kind={kind};material={fingerprint}")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use awaken_contract::ProviderSpec;

    use super::provider_definition_signature;

    fn provider(api_key: Option<&str>) -> ProviderSpec {
        ProviderSpec {
            id: "provider-a".into(),
            adapter: "openai".into(),
            api_key: api_key.map(Into::into),
            base_url: Some("https://example.invalid/v1".into()),
            timeout_secs: 30,
            adapter_options: BTreeMap::new(),
        }
    }

    #[test]
    fn provider_signature_changes_when_credential_material_changes() {
        let first = provider_definition_signature(&provider(Some("credential-one")));
        let second = provider_definition_signature(&provider(Some("credential-two")));

        assert_ne!(first, second);
        assert!(!first.contains("credential-one"));
        assert!(!second.contains("credential-two"));
    }
}
