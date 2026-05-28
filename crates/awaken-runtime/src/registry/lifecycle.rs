use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use awaken_runtime_contract::contract::executor::LlmExecutor;
use awaken_runtime_contract::registry_spec::{AgentSpec, ModelSpec};
use awaken_runtime_contract::validate_unique_model_ids;
use serde::Serialize;

#[cfg(feature = "a2a")]
use super::MapBackendRegistry;
use super::diagnostics::{RegistryValidationError, validate_registry_set};
use super::memory::{
    MapAgentSpecRegistry, MapModelRegistry, MapPluginSource, MapProviderRegistry, MapToolRegistry,
};
use super::snapshot::RegistryHandle;
#[cfg(feature = "a2a")]
use super::traits::BackendRegistry;
use super::traits::RegistrySet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderRemovalPolicy {
    BlockIfReferenced,
    CascadeUnusedModels,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderRemovalPreview {
    pub provider_id: String,
    pub model_ids: Vec<String>,
    pub agent_ids: Vec<String>,
    pub block_if_referenced_allowed: bool,
    pub cascade_unused_models_allowed: bool,
}

impl ProviderRemovalPreview {
    pub fn new(
        provider_id: impl Into<String>,
        mut model_ids: Vec<String>,
        mut agent_ids: Vec<String>,
    ) -> Self {
        model_ids.sort();
        model_ids.dedup();
        agent_ids.sort();
        agent_ids.dedup();
        Self {
            provider_id: provider_id.into(),
            block_if_referenced_allowed: model_ids.is_empty(),
            cascade_unused_models_allowed: agent_ids.is_empty(),
            model_ids,
            agent_ids,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderRemovalImpact {
    pub provider_id: String,
    pub removed_model_ids: Vec<String>,
    pub affected_agent_ids: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryUpdateError {
    #[error("provider already registered: {0}")]
    ProviderAlreadyExists(String),
    #[error("provider not found: {0}")]
    ProviderNotFound(String),
    #[error(
        "provider '{provider_id}' is still referenced by models {model_ids:?} and agents {agent_ids:?}"
    )]
    ProviderInUse {
        provider_id: String,
        model_ids: Vec<String>,
        agent_ids: Vec<String>,
    },
    #[error("registry build failed: {0}")]
    Build(String),
    #[error("{0}")]
    Validation(#[from] RegistryValidationError),
    #[error("config validation failed: {0}")]
    ConfigValidation(#[from] awaken_runtime_contract::ConfigValidationError),
}

pub struct RuntimeRegistryUpdate {
    pub providers: HashMap<String, Arc<dyn LlmExecutor>>,
    pub models: Vec<ModelSpec>,
    pub agents: Vec<AgentSpec>,
}

impl RegistryHandle {
    pub fn preview_remove_provider(
        &self,
        id: &str,
    ) -> Result<ProviderRemovalPreview, RegistryUpdateError> {
        let snapshot = self.snapshot();
        preview_provider_removal(snapshot.registries(), id)
    }

    pub fn register_provider(
        &self,
        id: impl Into<String>,
        executor: Arc<dyn LlmExecutor>,
    ) -> Result<u64, RegistryUpdateError> {
        let id = id.into();
        self.update(|registries| {
            let mut draft = RegistrySetDraft::from_set(registries)?;
            if draft.providers.contains_key(&id) {
                return Err(RegistryUpdateError::ProviderAlreadyExists(id));
            }
            draft
                .providers
                .register_provider(id, executor)
                .map_err(|error| RegistryUpdateError::Build(error.to_string()))?;
            draft.into_validated_set()
        })
    }

    pub fn replace_provider(
        &self,
        id: impl Into<String>,
        executor: Arc<dyn LlmExecutor>,
    ) -> Result<u64, RegistryUpdateError> {
        let id = id.into();
        self.update(|registries| {
            let mut draft = RegistrySetDraft::from_set(registries)?;
            if !draft.providers.contains_key(&id) {
                return Err(RegistryUpdateError::ProviderNotFound(id));
            }
            draft.providers.replace_provider(id, executor);
            draft.into_validated_set()
        })
    }

    pub fn remove_provider(
        &self,
        id: &str,
        policy: ProviderRemovalPolicy,
    ) -> Result<ProviderRemovalImpact, RegistryUpdateError> {
        let mut impact = None;
        self.update(|registries| {
            let mut draft = RegistrySetDraft::from_set(registries)?;
            if !draft.providers.contains_key(id) {
                return Err(RegistryUpdateError::ProviderNotFound(id.to_string()));
            }

            let preview = preview_provider_removal_from_draft(&draft, id)?;

            match policy {
                ProviderRemovalPolicy::BlockIfReferenced if !preview.model_ids.is_empty() => {
                    return Err(RegistryUpdateError::ProviderInUse {
                        provider_id: id.to_string(),
                        model_ids: preview.model_ids,
                        agent_ids: preview.agent_ids,
                    });
                }
                ProviderRemovalPolicy::CascadeUnusedModels if !preview.agent_ids.is_empty() => {
                    return Err(RegistryUpdateError::ProviderInUse {
                        provider_id: id.to_string(),
                        model_ids: preview.model_ids,
                        agent_ids: preview.agent_ids,
                    });
                }
                _ => {}
            }

            for model_id in &preview.model_ids {
                draft.models.remove(model_id);
            }
            draft.providers.remove_provider(id);

            impact = Some(ProviderRemovalImpact {
                provider_id: preview.provider_id,
                removed_model_ids: preview.model_ids,
                affected_agent_ids: preview.agent_ids,
            });
            draft.into_validated_set()
        })?;
        impact.ok_or_else(|| RegistryUpdateError::Build("provider removal did not run".into()))
    }
}

pub fn preview_provider_removal(
    registries: &RegistrySet,
    id: &str,
) -> Result<ProviderRemovalPreview, RegistryUpdateError> {
    if registries.providers.get_provider(id).is_none() {
        return Err(RegistryUpdateError::ProviderNotFound(id.to_string()));
    }
    let model_ids = provider_model_ids_from_set(registries, id);
    let agent_ids = agents_using_models_from_set(registries, &model_ids);
    Ok(ProviderRemovalPreview::new(id, model_ids, agent_ids))
}

pub fn rebuild_agent_model_provider_registries(
    base: &RegistrySet,
    update: RuntimeRegistryUpdate,
) -> Result<RegistrySet, RegistryUpdateError> {
    let mut draft = RegistrySetDraft::from_set(base)?;

    draft.providers = MapProviderRegistry::new();
    for (id, executor) in update.providers {
        draft
            .providers
            .register_provider(id, executor)
            .map_err(|error| RegistryUpdateError::Build(error.to_string()))?;
    }

    // Reject duplicate model ids before populating so callers see a clean
    // `DuplicateModelId` error instead of the registry's generic conflict.
    validate_unique_model_ids(&update.models)?;

    draft.models = MapModelRegistry::new();
    for model in update.models {
        draft
            .models
            .register_model(model)
            .map_err(|error| RegistryUpdateError::Build(error.to_string()))?;
    }

    draft.agents = MapAgentSpecRegistry::new();
    for agent in update.agents {
        draft
            .agents
            .register_spec(agent)
            .map_err(|error| RegistryUpdateError::Build(error.to_string()))?;
    }

    let registries = draft.into_set();
    validate_registry_set(&registries)?;
    Ok(registries)
}

struct RegistrySetDraft {
    agents: MapAgentSpecRegistry,
    tools: MapToolRegistry,
    models: MapModelRegistry,
    providers: MapProviderRegistry,
    plugins: MapPluginSource,
    #[cfg(feature = "a2a")]
    backends: MapBackendRegistry,
}

impl RegistrySetDraft {
    fn from_set(set: &RegistrySet) -> Result<Self, RegistryUpdateError> {
        let mut agents = MapAgentSpecRegistry::new();
        for id in set.agents.agent_ids() {
            if let Some(agent) = set.agents.get_agent(&id) {
                agents
                    .register(id, agent, |msg| {
                        crate::builder::BuildError::AgentRegistryConflict(format!("agent {msg}"))
                    })
                    .map_err(|error| RegistryUpdateError::Build(error.to_string()))?;
            }
        }

        let mut tools = MapToolRegistry::new();
        for id in set.tools.tool_ids() {
            if let Some(tool) = set.tools.get_tool(&id) {
                tools
                    .register_tool(id, tool)
                    .map_err(|error| RegistryUpdateError::Build(error.to_string()))?;
            }
        }

        let mut models = MapModelRegistry::new();
        for id in set.models.model_ids() {
            if let Some(model) = set.models.get_model(&id) {
                models
                    .register_model(model)
                    .map_err(|error| RegistryUpdateError::Build(error.to_string()))?;
            }
        }

        let mut providers = MapProviderRegistry::new();
        for id in set.providers.provider_ids() {
            if let Some(provider) = set.providers.get_provider(&id) {
                providers
                    .register_provider(id, provider)
                    .map_err(|error| RegistryUpdateError::Build(error.to_string()))?;
            }
        }

        let mut plugins = MapPluginSource::new();
        for id in set.plugins.plugin_ids() {
            if let Some(plugin) = set.plugins.get_plugin(&id) {
                plugins
                    .register_plugin(id, plugin)
                    .map_err(|error| RegistryUpdateError::Build(error.to_string()))?;
            }
        }

        #[cfg(feature = "a2a")]
        let mut backends = MapBackendRegistry::new();
        #[cfg(feature = "a2a")]
        for id in set.backends.backend_ids() {
            if let Some(factory) = set.backends.get_backend_factory(&id) {
                backends
                    .register_backend_factory(factory)
                    .map_err(|error| RegistryUpdateError::Build(error.to_string()))?;
            }
        }

        Ok(Self {
            agents,
            tools,
            models,
            providers,
            plugins,
            #[cfg(feature = "a2a")]
            backends,
        })
    }

    fn into_set(self) -> RegistrySet {
        RegistrySet {
            agents: Arc::new(self.agents),
            tools: Arc::new(self.tools),
            models: Arc::new(self.models),
            providers: Arc::new(self.providers),
            plugins: Arc::new(self.plugins),
            #[cfg(feature = "a2a")]
            backends: Arc::new(self.backends) as Arc<dyn BackendRegistry>,
        }
    }

    fn into_validated_set(self) -> Result<RegistrySet, RegistryUpdateError> {
        let registries = self.into_set();
        validate_registry_set(&registries)?;
        Ok(registries)
    }
}

fn provider_model_ids(models: &MapModelRegistry, provider_id: &str) -> Vec<String> {
    models
        .ids()
        .into_iter()
        .filter(|model_id| {
            models
                .get(model_id)
                .is_some_and(|model| model.provider_id == provider_id)
        })
        .collect()
}

fn preview_provider_removal_from_draft(
    draft: &RegistrySetDraft,
    provider_id: &str,
) -> Result<ProviderRemovalPreview, RegistryUpdateError> {
    if !draft.providers.contains_key(provider_id) {
        return Err(RegistryUpdateError::ProviderNotFound(
            provider_id.to_string(),
        ));
    }
    let model_ids = provider_model_ids(&draft.models, provider_id);
    let agent_ids = agents_using_models(&draft.agents, &model_ids);
    Ok(ProviderRemovalPreview::new(
        provider_id,
        model_ids,
        agent_ids,
    ))
}

fn provider_model_ids_from_set(registries: &RegistrySet, provider_id: &str) -> Vec<String> {
    registries
        .models
        .model_ids()
        .into_iter()
        .filter(|model_id| {
            registries
                .models
                .get_model(model_id)
                .is_some_and(|model| model.provider_id == provider_id)
        })
        .collect()
}

fn agents_using_models(agents: &MapAgentSpecRegistry, model_ids: &[String]) -> Vec<String> {
    let model_ids: HashSet<_> = model_ids.iter().map(String::as_str).collect();
    agents
        .ids()
        .into_iter()
        .filter(|agent_id| {
            agents.get(agent_id).is_some_and(|agent| {
                agent.endpoint.is_none() && model_ids.contains(agent.model_id.as_str())
            })
        })
        .collect()
}

fn agents_using_models_from_set(registries: &RegistrySet, model_ids: &[String]) -> Vec<String> {
    let model_ids: HashSet<_> = model_ids.iter().map(String::as_str).collect();
    registries
        .agents
        .agent_ids()
        .into_iter()
        .filter(|agent_id| {
            registries.agents.get_agent(agent_id).is_some_and(|agent| {
                agent.endpoint.is_none() && model_ids.contains(agent.model_id.as_str())
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_runtime_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
    use awaken_runtime_contract::contract::inference::{StopReason, StreamResult, TokenUsage};

    struct StubExecutor;

    #[async_trait]
    impl LlmExecutor for StubExecutor {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            Ok(StreamResult {
                content: vec![],
                tool_calls: vec![],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "stub"
        }
    }

    fn executor() -> Arc<dyn LlmExecutor> {
        Arc::new(StubExecutor)
    }

    fn registry_set() -> RegistrySet {
        let mut agents = MapAgentSpecRegistry::new();
        agents
            .register_spec(AgentSpec {
                id: "a".into(),
                model_id: "m".into(),
                system_prompt: "s".into(),
                ..Default::default()
            })
            .unwrap();

        let mut models = MapModelRegistry::new();
        models
            .register_model(ModelSpec::new("m", "p", "upstream"))
            .unwrap();

        let mut providers = MapProviderRegistry::new();
        providers.register_provider("p", executor()).unwrap();

        RegistrySet {
            agents: Arc::new(agents),
            tools: Arc::new(MapToolRegistry::new()),
            models: Arc::new(models),
            providers: Arc::new(providers),
            plugins: Arc::new(MapPluginSource::new()),
            #[cfg(feature = "a2a")]
            backends: Arc::new(MapBackendRegistry::new()),
        }
    }

    #[test]
    fn remove_provider_blocks_when_model_and_agent_depend_on_it() {
        let handle = RegistryHandle::new(registry_set());
        let preview = handle
            .preview_remove_provider("p")
            .expect("provider exists");
        assert_eq!(
            preview,
            ProviderRemovalPreview {
                provider_id: "p".into(),
                model_ids: vec!["m".into()],
                agent_ids: vec!["a".into()],
                block_if_referenced_allowed: false,
                cascade_unused_models_allowed: false,
            }
        );

        let err = handle
            .remove_provider("p", ProviderRemovalPolicy::CascadeUnusedModels)
            .expect_err("agent dependency must block removal");
        assert!(err.to_string().contains("agents [\"a\"]"));
    }

    #[test]
    fn remove_provider_cascades_unused_models() {
        let mut update = RuntimeRegistryUpdate {
            providers: HashMap::new(),
            models: vec![ModelSpec::new("m", "p", "upstream")],
            agents: Vec::new(),
        };
        update.providers.insert("p".into(), executor());
        let base = registry_set();
        let registries = rebuild_agent_model_provider_registries(&base, update).unwrap();
        let handle = RegistryHandle::new(registries);

        let impact = handle
            .remove_provider("p", ProviderRemovalPolicy::CascadeUnusedModels)
            .expect("unused model can be removed with provider");

        assert_eq!(impact.removed_model_ids, vec!["m"]);
        let snapshot = handle.snapshot();
        assert!(snapshot.registries().providers.get_provider("p").is_none());
        assert!(snapshot.registries().models.get_model("m").is_none());
    }

    #[test]
    fn replace_provider_keeps_model_and_agent() {
        let handle = RegistryHandle::new(registry_set());
        let version = handle
            .replace_provider("p", executor())
            .expect("provider exists");
        assert_eq!(version, 2);
        let snapshot = handle.snapshot();
        assert!(snapshot.registries().providers.get_provider("p").is_some());
        assert!(snapshot.registries().models.get_model("m").is_some());
        assert!(snapshot.registries().agents.get_agent("a").is_some());
    }

    #[test]
    fn concurrent_provider_registration_preserves_all_updates() {
        let handle = Arc::new(RegistryHandle::new(registry_set()));
        let mut threads = Vec::new();

        for index in 0..16 {
            let handle = Arc::clone(&handle);
            threads.push(std::thread::spawn(move || {
                handle
                    .register_provider(format!("p-{index}"), executor())
                    .expect("provider registration must succeed");
            }));
        }

        for thread in threads {
            thread.join().expect("thread must not panic");
        }

        let snapshot = handle.snapshot();
        for index in 0..16 {
            let provider_id = format!("p-{index}");
            assert!(
                snapshot
                    .registries()
                    .providers
                    .get_provider(&provider_id)
                    .is_some(),
                "provider {provider_id} must survive concurrent updates"
            );
        }
    }
}
