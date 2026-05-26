use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::StreamResult;
use awaken_contract::contract::message::Message;
use awaken_contract::registry_spec::{AgentSpec, Modalities, Modality, ModelPoolSpec, ModelSpec};

use crate::registry::memory::{
    MapAgentSpecRegistry, MapModelRegistry, MapPluginSource, MapProviderRegistry, MapToolRegistry,
};
use crate::registry::{ModelCapabilityPatch, RegistrySet};

use super::{resolve_model_and_executor, resolve_registry_set};

#[derive(Default)]
struct StubExecutor {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for StubExecutor {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(StreamResult {
            content: Vec::new(),
            tool_calls: Vec::new(),
            usage: None,
            stop_reason: None,
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        "stub"
    }
}

fn image_request() -> InferenceRequest {
    InferenceRequest {
        upstream_model: "gpt-4o".into(),
        routing_key: None,
        messages: vec![Message::user_with_content(vec![ContentBlock::image_url(
            "https://example.com/image.png",
        )])],
        tools: vec![],
        system: vec![],
        overrides: None,
        enable_prompt_cache: false,
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
            Arc::new(StubExecutor::default()),
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

    let (_, _, resolved_model, _) =
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
            Arc::new(StubExecutor::default()),
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

#[test]
fn resolver_respects_disabled_knowledge_cutoff_config() {
    let mut agent = AgentSpec {
        id: "agent".into(),
        model_id: "m".into(),
        ..AgentSpec::default()
    };
    agent
        .set_config::<crate::context::KnowledgeCutoffConfigKey>(
            crate::context::KnowledgeCutoffConfig { enabled: false },
        )
        .expect("set cutoff config");
    let mut agents = MapAgentSpecRegistry::new();
    agents.register_spec(agent).expect("agent");

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
            Arc::new(StubExecutor::default()),
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
        !resolved
            .env
            .plugins
            .iter()
            .any(|plugin| plugin.descriptor().name == crate::context::KNOWLEDGE_CUTOFF_PLUGIN_ID)
    );
}

#[test]
fn resolver_does_not_install_knowledge_cutoff_plugin_from_static_defaults() {
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
        .register_model(ModelSpec::new("m", "p", "gpt-4.1"))
        .expect("model");

    let mut providers = MapProviderRegistry::new();
    providers
        .register_provider_with_signature_and_capability_source(
            "p",
            Arc::new(StubExecutor::default()),
            "sig",
            "openai",
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
        !resolved
            .env
            .plugins
            .iter()
            .any(|plugin| plugin.descriptor().name == crate::context::KNOWLEDGE_CUTOFF_PLUGIN_ID),
        "static heuristic cutoff must not inject prompt context"
    );
}

#[test]
fn resolver_installs_knowledge_cutoff_plugin_for_pool_with_common_trusted_cutoff() {
    let mut agents = MapAgentSpecRegistry::new();
    agents
        .register_spec(AgentSpec {
            id: "agent".into(),
            model_id: "pool".into(),
            ..AgentSpec::default()
        })
        .expect("agent");

    let mut models = MapModelRegistry::new();
    for id in ["m0", "m1"] {
        models
            .register_model(ModelSpec {
                knowledge_cutoff: Some("2025-01".into()),
                ..ModelSpec::new(id, "p", format!("{id}-upstream"))
            })
            .expect("model");
    }
    models
        .register_model_pool(ModelPoolSpec::new("pool", ["m0", "m1"]))
        .expect("pool");

    let mut providers = MapProviderRegistry::new();
    providers
        .register_provider_with_signature_and_capability_source(
            "p",
            Arc::new(StubExecutor::default()),
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

#[tokio::test]
async fn resolver_does_not_enforce_modalities_from_static_defaults() {
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

    let inner = Arc::new(StubExecutor::default());
    let mut providers = MapProviderRegistry::new();
    providers
        .register_provider_with_signature_and_capability_source("p", inner.clone(), "sig", "openai")
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

    let (executor, _, resolved_model, sources) =
        resolve_model_and_executor(&registries, &agent).expect("resolved model");

    assert_eq!(resolved_model.modalities.input.len(), 2);
    assert_eq!(
        sources.input_modalities,
        Some(crate::registry::model_capabilities::CapabilitySource::StaticHeuristic)
    );
    executor
        .execute(image_request())
        .await
        .expect("static heuristic modalities are metadata, not runtime enforcement");
    assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn resolver_enforces_modalities_from_provider_discovery() {
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

    let inner = Arc::new(StubExecutor::default());
    let mut providers = MapProviderRegistry::new();
    providers
        .register_provider_with_signature_and_capability_source("p", inner.clone(), "sig", "openai")
        .expect("provider");
    providers.register_provider_model_capabilities(
        "p",
        HashMap::from([(
            "gpt-4o".into(),
            ModelCapabilityPatch {
                context_window: None,
                max_output_tokens: None,
                modalities: Some(Modalities {
                    input: vec![Modality::Text],
                    output: vec![Modality::Text],
                }),
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

    let (executor, _, _, sources) =
        resolve_model_and_executor(&registries, &agent).expect("resolved model");

    assert_eq!(
        sources.input_modalities,
        Some(crate::registry::model_capabilities::CapabilitySource::ProviderDiscovery)
    );
    let err = executor
        .execute(image_request())
        .await
        .expect_err("discovered modalities should be enforced");
    assert!(matches!(err, InferenceExecutionError::InvalidRequest(_)));
    assert_eq!(inner.calls.load(Ordering::SeqCst), 0);
}
