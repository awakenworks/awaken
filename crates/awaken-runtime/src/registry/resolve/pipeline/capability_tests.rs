use std::collections::HashMap;
use std::sync::Arc;

use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::StreamResult;
use awaken_contract::registry_spec::{AgentSpec, ModelSpec};

use crate::registry::memory::{
    MapAgentSpecRegistry, MapModelRegistry, MapPluginSource, MapProviderRegistry, MapToolRegistry,
};
use crate::registry::{ModelCapabilityPatch, RegistrySet};

use super::{resolve_model_and_executor, resolve_registry_set};

struct StubExecutor;

#[async_trait::async_trait]
impl LlmExecutor for StubExecutor {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        unreachable!("capability resolution test does not execute inference")
    }

    fn name(&self) -> &str {
        "stub"
    }
}

#[test]
fn model_resolution_prefers_discovered_capabilities_over_static_defaults() {
    let mut agents = MapAgentSpecRegistry::new();
    let agent = AgentSpec {
        id: "agent".into(),
        model_id: "m".into(),
        ..AgentSpec::default()
    };
    agents.register_spec(agent.clone()).expect("agent");

    let mut models = MapModelRegistry::new();
    models
        .register_model(ModelSpec::new("m", "p", "gpt-4o"))
        .expect("model");

    let mut providers = MapProviderRegistry::new();
    providers
        .register_provider_with_signature_and_capability_source(
            "p",
            Arc::new(StubExecutor),
            "sig",
            "openai",
        )
        .expect("provider");
    providers.register_provider_model_capabilities(
        "p",
        HashMap::from([(
            "gpt-4o".into(),
            ModelCapabilityPatch {
                context_window: Some(256_000),
                max_output_tokens: Some(64_000),
                modalities: None,
                knowledge_cutoff: None,
            },
        )]),
    );

    let registries = RegistrySet {
        agents: Arc::new(agents),
        tools: Arc::new(MapToolRegistry::new()),
        models: Arc::new(models),
        providers: Arc::new(providers),
        plugins: Arc::new(MapPluginSource::new()),
        #[cfg(feature = "a2a")]
        backends: Arc::new(crate::registry::memory::MapBackendRegistry::new()),
    };

    let (_, _, resolved_model) =
        resolve_model_and_executor(&registries, &agent).expect("resolved model");

    assert_eq!(resolved_model.context_window, Some(256_000));
    assert_eq!(resolved_model.max_output_tokens, Some(64_000));
}

#[test]
fn resolver_installs_knowledge_cutoff_plugin_for_cutoff_models() {
    let mut agents = MapAgentSpecRegistry::new();
    agents
        .register_spec(AgentSpec {
            id: "agent".into(),
            model_id: "m".into(),
            ..AgentSpec::default()
        })
        .expect("agent");

    let mut models = MapModelRegistry::new();
    models
        .register_model(ModelSpec {
            knowledge_cutoff: Some("2025-01".into()),
            ..ModelSpec::new("m", "p", "custom")
        })
        .expect("model");

    let mut providers = MapProviderRegistry::new();
    providers
        .register_provider_with_signature_and_capability_source(
            "p",
            Arc::new(StubExecutor),
            "sig",
            "custom",
        )
        .expect("provider");

    let registries = RegistrySet {
        agents: Arc::new(agents),
        tools: Arc::new(MapToolRegistry::new()),
        models: Arc::new(models),
        providers: Arc::new(providers),
        plugins: Arc::new(MapPluginSource::new()),
        #[cfg(feature = "a2a")]
        backends: Arc::new(crate::registry::memory::MapBackendRegistry::new()),
    };

    let resolved = resolve_registry_set(&registries, "agent").expect("resolved");

    assert!(
        resolved
            .env
            .plugins
            .iter()
            .any(|plugin| plugin.descriptor().name == crate::context::KNOWLEDGE_CUTOFF_PLUGIN_ID)
    );
}
