//! Tests for the fluent [`AgentRuntimeBuilder`] API.

use super::*;
use async_trait::async_trait;
use awaken_runtime_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
use awaken_runtime_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
#[cfg(feature = "a2a")]
use awaken_runtime_contract::contract::lifecycle::TerminationReason;
use awaken_runtime_contract::contract::tool::{
    ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
#[cfg(feature = "a2a")]
use awaken_runtime_contract::registry_spec::RemoteEndpoint;
use serde_json::Value;
#[cfg(feature = "a2a")]
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::registry::memory::{
    MapAgentSpecRegistry, MapModelRegistry, MapPluginSource, MapProviderRegistry, MapToolRegistry,
};

struct MockTool {
    id: String,
}

#[async_trait]
impl Tool for MockTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(&self.id, &self.id, "mock tool")
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolResult::success(&self.id, Value::Null).into())
    }
}

struct MockExecutor;

#[async_trait]
impl LlmExecutor for MockExecutor {
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
        "mock"
    }
}

#[cfg(feature = "a2a")]
struct NoopRemoteBackend;

#[cfg(feature = "a2a")]
#[async_trait]
impl crate::backend::ExecutionBackend for NoopRemoteBackend {
    async fn execute_root(
        &self,
        request: crate::backend::BackendRootRunRequest<'_>,
    ) -> Result<crate::backend::BackendRunResult, crate::backend::ExecutionBackendError> {
        Ok(crate::backend::BackendRunResult {
            agent_id: request.agent_id.to_string(),
            status: crate::backend::BackendRunStatus::Completed,
            termination: TerminationReason::NaturalEnd,
            status_reason: None,
            response: None,
            output: crate::backend::BackendRunOutput::default(),
            steps: 0,
            run_id: None,
            inbox: None,
            state: None,
        })
    }
}

#[cfg(feature = "a2a")]
struct CountingValidationBackendFactory {
    validate_count: Arc<AtomicUsize>,
    build_count: Arc<AtomicUsize>,
}

#[cfg(feature = "a2a")]
impl crate::backend::ExecutionBackendFactory for CountingValidationBackendFactory {
    fn backend(&self) -> &str {
        "counting-remote"
    }

    fn validate(
        &self,
        endpoint: &RemoteEndpoint,
    ) -> Result<(), crate::backend::ExecutionBackendFactoryError> {
        self.validate_count.fetch_add(1, Ordering::SeqCst);
        if endpoint.base_url.trim().is_empty() {
            return Err(crate::backend::ExecutionBackendFactoryError::InvalidConfig(
                "empty base_url".into(),
            ));
        }
        Ok(())
    }

    fn build(
        &self,
        endpoint: &RemoteEndpoint,
    ) -> Result<
        Arc<dyn crate::backend::ExecutionBackend>,
        crate::backend::ExecutionBackendFactoryError,
    > {
        self.build_count.fetch_add(1, Ordering::SeqCst);
        if endpoint.backend != self.backend() {
            return Err(crate::backend::ExecutionBackendFactoryError::InvalidConfig(
                format!("unexpected backend '{}'", endpoint.backend),
            ));
        }
        Ok(Arc::new(NoopRemoteBackend))
    }
}

fn make_registry_set(agent_id: &str, model_id: &str, upstream_model: &str) -> RegistrySet {
    let mut agents = MapAgentSpecRegistry::new();
    agents
        .register_spec(AgentSpec {
            id: agent_id.into(),
            model_id: model_id.into(),
            system_prompt: format!("system-{agent_id}"),
            ..Default::default()
        })
        .expect("register test agent");

    let mut models = MapModelRegistry::new();
    models
        .register_model(ModelSpec::new(model_id, "mock", upstream_model))
        .expect("register test model");

    let mut providers = MapProviderRegistry::new();
    providers
        .register_provider("mock", Arc::new(MockExecutor))
        .expect("register test provider");

    RegistrySet {
        agents: Arc::new(agents),
        tools: Arc::new(MapToolRegistry::new()),
        models: Arc::new(models),
        providers: Arc::new(providers),
        plugins: Arc::new(MapPluginSource::new()),
        backends: Arc::new(MapBackendRegistry::new()),
    }
}

#[test]
fn builder_creates_runtime() {
    let spec = AgentSpec {
        id: "test-agent".into(),
        model_id: "test-model".into(),
        system_prompt: "You are helpful.".into(),
        ..Default::default()
    };

    let runtime = AgentRuntimeBuilder::new()
        .with_agent_spec(spec)
        .with_tool("echo", Arc::new(MockTool { id: "echo".into() }))
        .with_model(ModelSpec::new("test-model", "mock", "mock-model"))
        .with_provider("mock", Arc::new(MockExecutor))
        .build();

    assert!(runtime.is_ok());
}

#[test]
fn builder_with_mock_provider_profile_registers_provider_and_model() {
    let runtime = AgentRuntimeBuilder::new()
        .with_agent_spec(AgentSpec {
            id: "mock-agent".into(),
            model_id: "mock-model".into(),
            system_prompt: "sys".into(),
            ..Default::default()
        })
        .with_mock_provider_profile(
            crate::engine::MockProviderProfile::new("mock-provider", "mock-model")
                .with_responses(vec!["ok".into()]),
        )
        .build()
        .unwrap();

    let resolved = runtime.resolver().resolve("mock-agent").unwrap();
    assert_eq!(resolved.upstream_model, "mock-model");
    assert_eq!(resolved.llm_executor.name(), "mock");
}

#[test]
fn builder_default_creates_empty() {
    let builder = AgentRuntimeBuilder::default();
    // Cannot resolve any agent but should build
    let runtime = builder.build();
    assert!(runtime.is_ok());
}

#[test]
fn builder_with_multiple_agents() {
    let spec1 = AgentSpec {
        id: "agent-1".into(),
        model_id: "m".into(),
        system_prompt: "sys".into(),
        ..Default::default()
    };
    let spec2 = AgentSpec {
        id: "agent-2".into(),
        model_id: "m".into(),
        system_prompt: "sys".into(),
        ..Default::default()
    };

    let runtime = AgentRuntimeBuilder::new()
        .with_agent_specs(vec![spec1, spec2])
        .with_model(ModelSpec::new("m", "p", "n"))
        .with_provider("p", Arc::new(MockExecutor))
        .build()
        .unwrap();

    // Both agents should be resolvable
    assert!(runtime.resolver().resolve("agent-1").is_ok());
    assert!(runtime.resolver().resolve("agent-2").is_ok());
}

#[test]
fn builder_resolver_returns_correct_config() {
    let spec = AgentSpec {
        id: "my-agent".into(),
        model_id: "test-model".into(),
        system_prompt: "Be helpful.".into(),
        max_rounds: 10,
        ..Default::default()
    };

    let runtime = AgentRuntimeBuilder::new()
        .with_agent_spec(spec)
        .with_tool(
            "search",
            Arc::new(MockTool {
                id: "search".into(),
            }),
        )
        .with_model(ModelSpec::new("test-model", "mock", "claude-test"))
        .with_provider("mock", Arc::new(MockExecutor))
        .build()
        .unwrap();

    let resolved = runtime.resolver().resolve("my-agent").unwrap();
    assert_eq!(resolved.id(), "my-agent");
    assert_eq!(resolved.upstream_model, "claude-test");
    assert_eq!(resolved.system_prompt(), "Be helpful.");
    assert_eq!(resolved.max_rounds(), 10);
    assert!(resolved.tools.contains_key("search"));
}

#[test]
fn builder_missing_agent_errors() {
    let runtime = AgentRuntimeBuilder::new()
        .with_model(ModelSpec::new("m", "p", "n"))
        .with_provider("p", Arc::new(MockExecutor))
        .build()
        .unwrap();

    let err = runtime.resolver().resolve("nonexistent");
    assert!(err.is_err());
}

// Migrated from uncarve: additional builder tests

#[test]
fn builder_with_plugin() {
    use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};

    struct TestPlugin;
    impl Plugin for TestPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "test-builder-plugin",
            }
        }
        fn register(
            &self,
            _registrar: &mut PluginRegistrar,
        ) -> Result<(), awaken_runtime_contract::StateError> {
            Ok(())
        }
    }

    let runtime = AgentRuntimeBuilder::new()
        .with_plugin("test-builder-plugin", Arc::new(TestPlugin))
        .build()
        .unwrap();
    let _ = runtime;
}

#[test]
fn builder_chained_tools_all_registered() {
    let spec = AgentSpec {
        id: "agent".into(),
        model_id: "m".into(),
        system_prompt: "sys".into(),
        ..Default::default()
    };

    let runtime = AgentRuntimeBuilder::new()
        .with_agent_spec(spec)
        .with_tool("t1", Arc::new(MockTool { id: "t1".into() }))
        .with_tool("t2", Arc::new(MockTool { id: "t2".into() }))
        .with_tool("t3", Arc::new(MockTool { id: "t3".into() }))
        .with_model(ModelSpec::new("m", "p", "n"))
        .with_provider("p", Arc::new(MockExecutor))
        .build()
        .unwrap();

    let resolved = runtime.resolver().resolve("agent").unwrap();
    assert!(resolved.tools.contains_key("t1"));
    assert!(resolved.tools.contains_key("t2"));
    assert!(resolved.tools.contains_key("t3"));
}

#[test]
fn build_catches_missing_model() {
    let spec = AgentSpec {
        id: "bad-agent".into(),
        model_id: "nonexistent-model".into(),
        system_prompt: "sys".into(),
        ..Default::default()
    };

    let result = AgentRuntimeBuilder::new().with_agent_spec(spec).build();

    let err = match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected build to fail for missing model"),
    };
    assert!(
        err.contains("bad-agent"),
        "error should mention the agent ID: {err}"
    );
}

#[test]
fn build_succeeds_with_valid_config() {
    let spec = AgentSpec {
        id: "good-agent".into(),
        model_id: "m".into(),
        system_prompt: "sys".into(),
        ..Default::default()
    };

    let result = AgentRuntimeBuilder::new()
        .with_agent_spec(spec)
        .with_model(ModelSpec::new("m", "p", "n"))
        .with_provider("p", Arc::new(MockExecutor))
        .build();

    assert!(result.is_ok());
}

#[test]
fn builder_runtime_starts_with_registry_version_one() {
    let runtime = AgentRuntimeBuilder::new()
        .with_agent_spec(AgentSpec {
            id: "versioned-agent".into(),
            model_id: "m".into(),
            system_prompt: "sys".into(),
            ..Default::default()
        })
        .with_model(ModelSpec::new("m", "mock", "model-v1"))
        .with_provider("mock", Arc::new(MockExecutor))
        .build()
        .unwrap();

    assert_eq!(runtime.registry_version(), Some(1));
    assert!(runtime.registry_handle().is_some());
}

#[test]
fn replacing_registry_set_updates_dynamic_resolver() {
    let runtime = AgentRuntimeBuilder::new()
        .with_agent_spec(AgentSpec {
            id: "agent-v1".into(),
            model_id: "m".into(),
            system_prompt: "sys".into(),
            ..Default::default()
        })
        .with_model(ModelSpec::new("m", "mock", "model-v1"))
        .with_provider("mock", Arc::new(MockExecutor))
        .build()
        .unwrap();

    assert!(runtime.resolver().resolve("agent-v1").is_ok());
    assert!(runtime.resolver().resolve("agent-v2").is_err());

    let version = runtime
        .replace_registry_set(make_registry_set("agent-v2", "m2", "model-v2"))
        .expect("builder runtimes should expose a registry handle");

    assert_eq!(version, 2);
    assert_eq!(runtime.registry_version(), Some(2));
    assert!(runtime.resolver().resolve("agent-v1").is_err());

    let resolved = runtime.resolver().resolve("agent-v2").unwrap();
    assert_eq!(resolved.id(), "agent-v2");
    assert_eq!(resolved.upstream_model, "model-v2");
}

#[test]
fn builder_model_spec_provider_name() {
    let spec = AgentSpec {
        id: "agent".into(),
        model_id: "gpt-4".into(),
        system_prompt: "sys".into(),
        ..Default::default()
    };

    let runtime = AgentRuntimeBuilder::new()
        .with_agent_spec(spec)
        .with_model(ModelSpec::new("gpt-4", "openai", "gpt-4-turbo"))
        .with_provider("openai", Arc::new(MockExecutor))
        .build()
        .unwrap();

    let resolved = runtime.resolver().resolve("agent").unwrap();
    // The model ID should resolve to the upstream model name
    assert_eq!(resolved.upstream_model, "gpt-4-turbo");
}

#[test]
fn builder_with_profile_store() {
    use awaken_runtime_contract::contract::profile_store::{
        ProfileEntry, ProfileOwner as POwner, ProfileStore,
    };
    use awaken_runtime_contract::contract::storage::StorageError;

    struct NoOpProfileStore;

    #[async_trait]
    impl ProfileStore for NoOpProfileStore {
        async fn get(
            &self,
            _owner: &POwner,
            _key: &str,
        ) -> Result<Option<ProfileEntry>, StorageError> {
            Ok(None)
        }
        async fn set(
            &self,
            _owner: &POwner,
            _key: &str,
            _value: Value,
        ) -> Result<(), StorageError> {
            Ok(())
        }
        async fn delete(&self, _owner: &POwner, _key: &str) -> Result<(), StorageError> {
            Ok(())
        }
        async fn list(&self, _owner: &POwner) -> Result<Vec<ProfileEntry>, StorageError> {
            Ok(vec![])
        }
        async fn clear_owner(&self, _owner: &POwner) -> Result<(), StorageError> {
            Ok(())
        }
    }

    let runtime = AgentRuntimeBuilder::new()
        .with_profile_store(Arc::new(NoOpProfileStore))
        .build()
        .unwrap();
    assert!(runtime.profile_store.is_some());
}

#[cfg(feature = "a2a")]
#[test]
fn build_allows_endpoint_agents_when_backend_factory_exists() {
    let validate_count = Arc::new(AtomicUsize::new(0));
    let build_count = Arc::new(AtomicUsize::new(0));
    let runtime = AgentRuntimeBuilder::new()
        .with_agent_spec(
            AgentSpec::new("remote-agent")
                .with_model_id("remote-model")
                .with_system_prompt("remote")
                .with_endpoint(RemoteEndpoint {
                    backend: "counting-remote".into(),
                    base_url: "https://remote.example.com".into(),
                    ..Default::default()
                }),
        )
        .with_agent_backend_factory(Arc::new(CountingValidationBackendFactory {
            validate_count: validate_count.clone(),
            build_count: build_count.clone(),
        }))
        .build()
        .expect("endpoint agents should validate through backend factory");

    let spec = runtime
        .registry_set()
        .and_then(|set| set.agents.get_agent("remote-agent"))
        .expect("remote agent should remain registered");
    assert!(spec.endpoint.is_some());
    assert_eq!(validate_count.load(Ordering::SeqCst), 1);
    assert_eq!(build_count.load(Ordering::SeqCst), 0);
}

#[test]
fn duplicate_agent_spec_errors_at_build() {
    let spec = AgentSpec {
        id: "dup-agent".into(),
        model_id: "m".into(),
        system_prompt: "sys".into(),
        ..Default::default()
    };

    let result = AgentRuntimeBuilder::new()
        .with_agent_spec(spec.clone())
        .with_agent_spec(spec)
        .with_model(ModelSpec::new("m", "p", "n"))
        .with_provider("p", Arc::new(MockExecutor))
        .build();

    match result {
        Err(e) => {
            let err = e.to_string();
            assert!(
                err.contains("dup-agent"),
                "error should mention the duplicate agent ID: {err}"
            );
        }
        Ok(_) => panic!("expected build to fail for duplicate agent"),
    }
}

#[test]
fn duplicate_tool_errors_at_build() {
    let result = AgentRuntimeBuilder::new()
        .with_tool(
            "dup-tool",
            Arc::new(MockTool {
                id: "dup-tool".into(),
            }),
        )
        .with_tool(
            "dup-tool",
            Arc::new(MockTool {
                id: "dup-tool".into(),
            }),
        )
        .build();

    match result {
        Err(e) => {
            let err = e.to_string();
            assert!(
                err.contains("dup-tool"),
                "error should mention the duplicate tool ID: {err}"
            );
        }
        Ok(_) => panic!("expected build to fail for duplicate tool"),
    }
}

#[test]
fn duplicate_model_errors_at_build() {
    let result = AgentRuntimeBuilder::new()
        .with_model(ModelSpec::new("dup-model", "p", "n1"))
        .with_model(ModelSpec::new("dup-model", "p", "n2"))
        .build();

    match result {
        Err(e) => {
            assert!(
                matches!(
                    e,
                    BuildError::ConfigValidation(
                        awaken_runtime_contract::ConfigValidationError::DuplicateModelId { ref id }
                    ) if id == "dup-model"
                ),
                "expected DuplicateModelId at builder surface, got: {e:?}"
            );
        }
        Ok(_) => panic!("expected build to fail for duplicate model"),
    }
}

/// The builder/registry path must reject the same invalid `ModelSpec` values
/// the JSON config surface rejects — capability bounds, cross-field invariant,
/// pricing, calendar-valid cutoff, modality dedup — so the `ModelSpec` contract
/// is not split across entry points.
#[test]
fn builder_rejects_invalid_model_specs() {
    let cases: Vec<(&str, ModelSpec)> = vec![
        (
            "zero context_window",
            ModelSpec {
                context_window: Some(0),
                ..ModelSpec::new("m", "p", "u")
            },
        ),
        (
            "output exceeds context",
            ModelSpec {
                context_window: Some(1_000),
                max_output_tokens: Some(2_000),
                ..ModelSpec::new("m", "p", "u")
            },
        ),
        (
            "negative price",
            ModelSpec {
                input_token_price_per_million_usd: Some(-1.0),
                ..ModelSpec::new("m", "p", "u")
            },
        ),
        (
            "non-finite price",
            ModelSpec {
                output_token_price_per_million_usd: Some(f64::NAN),
                ..ModelSpec::new("m", "p", "u")
            },
        ),
        (
            "malformed knowledge_cutoff",
            ModelSpec {
                knowledge_cutoff: Some("yesterday".into()),
                ..ModelSpec::new("m", "p", "u")
            },
        ),
        (
            "calendar-invalid knowledge_cutoff",
            ModelSpec {
                knowledge_cutoff: Some("2026-02-31".into()),
                ..ModelSpec::new("m", "p", "u")
            },
        ),
        (
            "duplicate input modalities",
            ModelSpec {
                modalities: awaken_runtime_contract::registry_spec::Modalities {
                    input: vec![
                        awaken_runtime_contract::registry_spec::Modality::Text,
                        awaken_runtime_contract::registry_spec::Modality::Text,
                    ],
                    output: vec![],
                },
                ..ModelSpec::new("m", "p", "u")
            },
        ),
    ];

    for (label, spec) in cases {
        match AgentRuntimeBuilder::new().with_model(spec).build() {
            Err(BuildError::ConfigValidation(_)) => {}
            Err(other) => {
                panic!("{label}: expected ConfigValidation error from builder, got: {other:?}")
            }
            Ok(_) => panic!("{label}: expected build to fail validation, but it succeeded"),
        }
    }
}

#[test]
fn duplicate_provider_errors_at_build() {
    let result = AgentRuntimeBuilder::new()
        .with_provider("dup-prov", Arc::new(MockExecutor))
        .with_provider("dup-prov", Arc::new(MockExecutor))
        .build();

    match result {
        Err(e) => {
            let err = e.to_string();
            assert!(
                err.contains("dup-prov"),
                "error should mention the duplicate provider ID: {err}"
            );
        }
        Ok(_) => panic!("expected build to fail for duplicate provider"),
    }
}

#[cfg(feature = "a2a")]
#[test]
fn duplicate_backend_factory_errors_at_build() {
    let result = AgentRuntimeBuilder::new()
        .with_agent_backend_factory(Arc::new(crate::extensions::a2a::A2aBackendFactory))
        .build();

    match result {
        Err(BuildError::BackendRegistryConflict(err)) => {
            assert!(
                err.contains("a2a"),
                "error should mention the duplicate backend kind: {err}"
            );
        }
        Err(other) => panic!("expected backend registry conflict, got {other}"),
        Ok(_) => panic!("expected build to fail for duplicate backend factory"),
    }
}

#[tokio::test]
async fn builder_runtime_resolves_persistent_activation_as_replayable() {
    let runtime = AgentRuntimeBuilder::new()
        .with_agent_spec(AgentSpec {
            id: "test-agent".into(),
            model_id: "test-model".into(),
            system_prompt: "You are helpful.".into(),
            ..Default::default()
        })
        .with_model(ModelSpec::new("test-model", "mock", "mock-model"))
        .with_provider("mock", Arc::new(MockExecutor))
        .build()
        .expect("runtime");

    let activation = crate::RunActivation::new(
        "thread",
        vec![awaken_runtime_contract::contract::message::Message::user(
            "hi",
        )],
    )
    .with_agent_id("test-agent");
    let plan = runtime
        .resolve_activation(
            &activation,
            crate::resolution::ResolutionPolicy::PersistentServer,
        )
        .await
        .expect("persistent resolution");
    let crate::ResolvedRunPlan::Replayable(plan) = plan else {
        panic!("persistent builder runtime must resolve replayable plans");
    };
    assert_eq!(
        plan.artifact.registry_manifest.registry_snapshot_version,
        runtime.registry_version()
    );
    assert!(plan.artifact.registry_manifest.entries.iter().any(|entry| {
        entry.kind == awaken_runtime_contract::contract::pinned_registry::REGISTRY_KIND_AGENT
            && entry.id == "test-agent"
    }));
}
