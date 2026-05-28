use super::*;
#[cfg(feature = "a2a")]
use crate::extensions::a2a::{
    AgentBackend, AgentBackendError, AgentBackendFactory, AgentBackendFactoryError,
    DelegateRunResult, DelegateRunStatus,
};
use crate::plugins::{ConfigSchema, PluginDescriptor, PluginRegistrar};
#[cfg(feature = "a2a")]
use crate::registry::BackendRegistry;
#[cfg(feature = "a2a")]
use crate::registry::memory::MapBackendRegistry;
use crate::registry::memory::{
    MapAgentSpecRegistry, MapModelRegistry, MapPluginSource, MapProviderRegistry, MapToolRegistry,
};
use crate::registry::traits::ModelRegistry;
use crate::resolution::{PersistenceRequirement, RunFeatureSet};
use async_trait::async_trait;
use awaken_runtime_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
use awaken_runtime_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_runtime_contract::contract::lifecycle::TerminationReason;
use awaken_runtime_contract::contract::run::RunResolutionScope;
use awaken_runtime_contract::contract::tool::{
    ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
#[cfg(feature = "a2a")]
use awaken_runtime_contract::registry_spec::RemoteEndpoint;
use awaken_runtime_contract::registry_spec::{ModelPoolSpec, ModelSpec};
use awaken_runtime_contract::{REGISTRY_KIND_MODEL, REGISTRY_KIND_PROVIDER};
use serde_json::Value;
#[cfg(feature = "a2a")]
use std::sync::atomic::{AtomicUsize, Ordering};

// -- Mock Tool --

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

// -- Mock LlmExecutor --

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

struct AmbiguousModelRegistry {
    shared_id: String,
    model: ModelSpec,
    pool: ModelPoolSpec,
}

impl ModelRegistry for AmbiguousModelRegistry {
    fn get_model(&self, id: &str) -> Option<ModelSpec> {
        (id == self.shared_id).then(|| self.model.clone())
    }

    fn model_ids(&self) -> Vec<String> {
        vec![self.shared_id.clone()]
    }

    fn get_pool(&self, id: &str) -> Option<ModelPoolSpec> {
        (id == self.shared_id).then(|| self.pool.clone())
    }

    fn pool_ids(&self) -> Vec<String> {
        vec![self.shared_id.clone()]
    }
}

#[cfg(feature = "a2a")]
struct StaticBackend {
    result: DelegateRunResult,
}

#[cfg(feature = "a2a")]
#[async_trait]
impl AgentBackend for StaticBackend {
    async fn execute_root(
        &self,
        _request: crate::backend::BackendRootRunRequest<'_>,
    ) -> Result<DelegateRunResult, AgentBackendError> {
        Ok(self.result.clone())
    }

    async fn execute_delegate(
        &self,
        _request: crate::backend::BackendDelegateRunRequest<'_>,
    ) -> Result<DelegateRunResult, AgentBackendError> {
        Ok(self.result.clone())
    }
}

#[cfg(feature = "a2a")]
struct StaticBackendFactory {
    backend: &'static str,
    result: DelegateRunResult,
    validate_count: Arc<AtomicUsize>,
    build_count: Arc<AtomicUsize>,
}

#[cfg(feature = "a2a")]
impl AgentBackendFactory for StaticBackendFactory {
    fn backend(&self) -> &str {
        self.backend
    }

    fn validate(&self, endpoint: &RemoteEndpoint) -> Result<(), AgentBackendFactoryError> {
        self.validate_count.fetch_add(1, Ordering::SeqCst);
        if endpoint.backend != self.backend {
            return Err(AgentBackendFactoryError::InvalidConfig(format!(
                "unexpected backend {}",
                endpoint.backend
            )));
        }
        Ok(())
    }

    fn build(
        &self,
        endpoint: &RemoteEndpoint,
    ) -> Result<Arc<dyn AgentBackend>, AgentBackendFactoryError> {
        self.build_count.fetch_add(1, Ordering::SeqCst);
        if endpoint.backend != self.backend {
            return Err(AgentBackendFactoryError::InvalidConfig(format!(
                "unexpected backend {}",
                endpoint.backend
            )));
        }

        Ok(Arc::new(StaticBackend {
            result: self.result.clone(),
        }))
    }
}

// -- Mock Plugin --

struct MockPlugin {
    name: &'static str,
}

impl Plugin for MockPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: self.name }
    }
}

// -- Test helper: build a fully populated RegistrySet --

fn build_registries(
    tools: Vec<(&str, Arc<dyn Tool>)>,
    model_id: &str,
    model_spec: ModelSpec,
    provider_id: &str,
    executor: Arc<dyn LlmExecutor>,
    plugins: Vec<(&str, Arc<dyn Plugin>)>,
    spec: AgentSpec,
) -> RegistrySet {
    let mut tool_reg = MapToolRegistry::new();
    for (id, tool) in tools {
        tool_reg
            .register_tool(id, tool)
            .expect("duplicate tool in test");
    }

    debug_assert_eq!(
        model_spec.id, model_id,
        "model_spec.id must match model_id arg"
    );
    let mut model_reg = MapModelRegistry::new();
    model_reg
        .register_model(model_spec)
        .expect("duplicate model in test");

    let mut provider_reg = MapProviderRegistry::new();
    provider_reg
        .register_provider(provider_id, executor)
        .expect("duplicate provider in test");

    let mut plugin_reg = MapPluginSource::new();
    for (id, plugin) in plugins {
        plugin_reg
            .register_plugin(id, plugin)
            .expect("duplicate plugin in test");
    }

    let mut agent_reg = MapAgentSpecRegistry::new();
    agent_reg
        .register_spec(spec)
        .expect("duplicate agent in test");

    RegistrySet {
        agents: Arc::new(agent_reg),
        tools: Arc::new(tool_reg),
        models: Arc::new(model_reg),
        providers: Arc::new(provider_reg),
        plugins: Arc::new(plugin_reg),
        #[cfg(feature = "a2a")]
        backends: Arc::new(MapBackendRegistry::with_default_remote_backends())
            as Arc<dyn BackendRegistry>,
    }
}

fn build_registries_with_provider_source(
    model_spec: ModelSpec,
    provider_id: &str,
    provider_source: &str,
    spec: AgentSpec,
) -> RegistrySet {
    let mut model_reg = MapModelRegistry::new();
    model_reg
        .register_model(model_spec)
        .expect("duplicate model in test");

    let mut provider_reg = MapProviderRegistry::new();
    provider_reg
        .register_provider_with_signature_and_capability_source(
            provider_id,
            Arc::new(MockExecutor),
            "mock",
            provider_source,
        )
        .expect("duplicate provider in test");

    let mut agent_reg = MapAgentSpecRegistry::new();
    agent_reg
        .register_spec(spec)
        .expect("duplicate agent in test");

    RegistrySet {
        agents: Arc::new(agent_reg),
        tools: Arc::new(MapToolRegistry::new()),
        models: Arc::new(model_reg),
        providers: Arc::new(provider_reg),
        plugins: Arc::new(MapPluginSource::new()),
        #[cfg(feature = "a2a")]
        backends: Arc::new(MapBackendRegistry::with_default_remote_backends())
            as Arc<dyn BackendRegistry>,
    }
}

fn make_spec(id: &str) -> AgentSpec {
    AgentSpec {
        id: id.into(),
        model_id: "test-model".into(),
        system_prompt: "You are helpful.".into(),
        ..Default::default()
    }
}

/// Build a `RegistrySet` with the given member models, a pool over them, one
/// provider, and one agent whose `model_id` is the pool id.
fn build_pool_registries(
    models: Vec<ModelSpec>,
    pool: ModelPoolSpec,
    provider_id: &str,
    executor: Arc<dyn LlmExecutor>,
    spec: AgentSpec,
) -> RegistrySet {
    let mut model_reg = MapModelRegistry::new();
    for model in models {
        model_reg
            .register_model(model)
            .expect("duplicate model in test");
    }
    model_reg
        .register_model_pool(pool)
        .expect("duplicate pool in test");

    let mut provider_reg = MapProviderRegistry::new();
    provider_reg
        .register_provider(provider_id, executor)
        .expect("duplicate provider in test");

    let mut agent_reg = MapAgentSpecRegistry::new();
    agent_reg
        .register_spec(spec)
        .expect("duplicate agent in test");

    RegistrySet {
        agents: Arc::new(agent_reg),
        tools: Arc::new(MapToolRegistry::new()),
        models: Arc::new(model_reg),
        providers: Arc::new(provider_reg),
        plugins: Arc::new(MapPluginSource::new()),
        #[cfg(feature = "a2a")]
        backends: Arc::new(MapBackendRegistry::with_default_remote_backends())
            as Arc<dyn BackendRegistry>,
    }
}

#[test]
fn resolve_pool_succeeds_with_placeholder_upstream() {
    let spec = AgentSpec {
        model_id: "my-pool".into(),
        ..make_spec("agent-1")
    };
    let regs = build_pool_registries(
        vec![
            ModelSpec::new("m0", "p", "m0-upstream"),
            ModelSpec::new("m1", "p", "m1-upstream"),
        ],
        ModelPoolSpec::new("my-pool", ["m0", "m1"]),
        "p",
        Arc::new(MockExecutor),
        spec,
    );

    let run = resolve(&regs, "agent-1").expect("pool model_id resolves");
    assert_eq!(run.id(), "agent-1");
    // The pool overrides the per-request upstream model per member, so the
    // resolved stand-in is the pool id rather than any single member.
    assert_eq!(run.upstream_model, "my-pool");
}

#[tokio::test]
async fn replayable_manifest_includes_model_pool_and_member_models() {
    let spec = AgentSpec {
        model_id: "my-pool".into(),
        ..make_spec("agent-1")
    };
    let regs = build_pool_registries(
        vec![
            ModelSpec::new("m0", "p", "m0-upstream"),
            ModelSpec::new("m1", "p", "m1-upstream"),
        ],
        ModelPoolSpec::new("my-pool", ["m0", "m1"]),
        "p",
        Arc::new(MockExecutor),
        spec,
    );

    let resolver = RegistrySetResolver::new(regs);
    let plan = Resolver::resolve(
        &resolver,
        ResolutionRequest {
            target: ResolutionTarget::Root {
                agent_id: "agent-1".into(),
                thread_id: "thread-1".into(),
            },
            resolution_scope: RunResolutionScope::Live,
            overrides: None,
            frontend_tools: vec![],
            features: RunFeatureSet {
                requested_persistence: PersistenceRequirement::CheckpointRequired,
                ..Default::default()
            },
        },
    )
    .await
    .expect("persistent pool-backed agent should resolve");
    let manifest = plan
        .replayable_manifest()
        .expect("persistent request should pin a manifest");

    assert!(
        manifest
            .entries
            .iter()
            .any(|entry| { entry.kind == REGISTRY_KIND_MODEL_POOL && entry.id == "my-pool" })
    );
    assert!(
        manifest
            .entries
            .iter()
            .any(|entry| entry.kind == REGISTRY_KIND_MODEL && entry.id == "m0")
    );
    assert!(
        manifest
            .entries
            .iter()
            .any(|entry| entry.kind == REGISTRY_KIND_MODEL && entry.id == "m1")
    );
    assert!(
        manifest
            .entries
            .iter()
            .any(|entry| entry.kind == REGISTRY_KIND_PROVIDER && entry.id == "p")
    );
}

#[test]
fn resolve_pool_missing_member_model_errors() {
    let spec = AgentSpec {
        model_id: "my-pool".into(),
        ..make_spec("agent-1")
    };
    let regs = build_pool_registries(
        vec![ModelSpec::new("m0", "p", "m0-upstream")],
        ModelPoolSpec::new("my-pool", ["m0", "m9"]),
        "p",
        Arc::new(MockExecutor),
        spec,
    );

    let err = resolve(&regs, "agent-1").unwrap_err();
    assert!(matches!(err, ResolveError::ModelNotFound(ref id) if id == "m9"));
}

fn build_ambiguous_model_reference_registries() -> RegistrySet {
    let shared_id = "shared-model-id";
    let mut provider_reg = MapProviderRegistry::new();
    provider_reg
        .register_provider("p", Arc::new(MockExecutor))
        .expect("provider registers");

    let mut agent_reg = MapAgentSpecRegistry::new();
    agent_reg
        .register_spec(AgentSpec {
            model_id: shared_id.into(),
            ..make_spec("agent-1")
        })
        .expect("agent registers");

    RegistrySet {
        agents: Arc::new(agent_reg),
        tools: Arc::new(MapToolRegistry::new()),
        models: Arc::new(AmbiguousModelRegistry {
            shared_id: shared_id.into(),
            model: ModelSpec::new(shared_id, "p", "single-upstream"),
            pool: ModelPoolSpec::new(shared_id, ["member"]),
        }),
        providers: Arc::new(provider_reg),
        plugins: Arc::new(MapPluginSource::new()),
        #[cfg(feature = "a2a")]
        backends: Arc::new(MapBackendRegistry::with_default_remote_backends())
            as Arc<dyn BackendRegistry>,
    }
}

#[test]
fn resolve_rejects_model_pool_id_collision() {
    let regs = build_ambiguous_model_reference_registries();

    let err = resolve(&regs, "agent-1").expect_err("shared model/pool id must be ambiguous");

    assert!(matches!(
        err,
        ResolveError::AmbiguousModelReference(ref id) if id == "shared-model-id"
    ));
}

#[tokio::test]
async fn replayable_manifest_rejects_model_pool_id_collision() {
    let regs = build_ambiguous_model_reference_registries();
    let resolver = RegistrySetResolver::new(regs);

    let err = match Resolver::resolve(
        &resolver,
        ResolutionRequest {
            target: ResolutionTarget::Root {
                agent_id: "agent-1".into(),
                thread_id: "thread-1".into(),
            },
            resolution_scope: RunResolutionScope::Live,
            overrides: None,
            frontend_tools: vec![],
            features: RunFeatureSet {
                requested_persistence: PersistenceRequirement::CheckpointRequired,
                ..Default::default()
            },
        },
    )
    .await
    {
        Ok(_) => panic!("manifest pinning must reject ambiguous model references"),
        Err(err) => err,
    };

    assert!(
        err.to_string().contains("model id resolves to both"),
        "got: {err}"
    );
}

// -- Tests --

#[test]
fn resolve_happy_path() {
    let spec = AgentSpec {
        plugin_ids: vec!["log".into()],
        ..make_spec("agent-1")
    };

    let regs = build_registries(
        vec![
            ("read", Arc::new(MockTool { id: "read".into() })),
            ("write", Arc::new(MockTool { id: "write".into() })),
        ],
        "test-model",
        ModelSpec::new("test-model", "anthropic", "claude-opus-4-20250514"),
        "anthropic",
        Arc::new(MockExecutor),
        vec![("log", Arc::new(MockPlugin { name: "log" }))],
        spec,
    );

    let run = resolve(&regs, "agent-1").unwrap();
    assert_eq!(run.id(), "agent-1");
    assert_eq!(run.upstream_model, "claude-opus-4-20250514");
    assert_eq!(run.tools.len(), 2);
    assert!(run.tools.contains_key("read"));
    assert!(run.tools.contains_key("write"));
    assert_eq!(run.env.plugins.len(), 3); // user plugin + LoopActionHandlersPlugin + MaxRoundsPlugin
}

#[test]
fn resolve_normalizes_compaction_summary_model_registry_id_on_same_provider() {
    let mut spec = AgentSpec {
        context_policy: Some(
            awaken_runtime_contract::contract::inference::ContextWindowPolicy {
                autocompact_threshold: Some(4096),
                ..Default::default()
            },
        ),
        ..make_spec("agent-1")
    };
    spec.set_config::<crate::context::CompactionConfigKey>(crate::context::CompactionConfig {
        summary_model: Some("summary-model".into()),
        ..Default::default()
    })
    .unwrap();

    let mut model_reg = MapModelRegistry::new();
    model_reg
        .register_model(ModelSpec::new("test-model", "anthropic", "claude-opus"))
        .unwrap();
    model_reg
        .register_model(ModelSpec::new(
            "summary-model",
            "anthropic",
            "claude-haiku-summary",
        ))
        .unwrap();
    let mut provider_reg = MapProviderRegistry::new();
    provider_reg
        .register_provider("anthropic", Arc::new(MockExecutor))
        .unwrap();
    let mut agent_reg = MapAgentSpecRegistry::new();
    agent_reg.register_spec(spec).unwrap();
    let regs = RegistrySet {
        agents: Arc::new(agent_reg),
        tools: Arc::new(MapToolRegistry::new()),
        models: Arc::new(model_reg),
        providers: Arc::new(provider_reg),
        plugins: Arc::new(MapPluginSource::new()),
        #[cfg(feature = "a2a")]
        backends: Arc::new(MapBackendRegistry::with_default_remote_backends())
            as Arc<dyn BackendRegistry>,
    };

    let run = resolve(&regs, "agent-1").unwrap();
    let config = run
        .spec
        .config::<crate::context::CompactionConfigKey>()
        .unwrap();
    assert_eq!(
        config.summary_model.as_deref(),
        Some("claude-haiku-summary")
    );
}

#[test]
fn resolve_rejects_compaction_summary_model_registry_id_on_different_provider() {
    let mut spec = AgentSpec {
        context_policy: Some(
            awaken_runtime_contract::contract::inference::ContextWindowPolicy {
                autocompact_threshold: Some(4096),
                ..Default::default()
            },
        ),
        ..make_spec("agent-1")
    };
    spec.set_config::<crate::context::CompactionConfigKey>(crate::context::CompactionConfig {
        summary_model: Some("summary-model".into()),
        ..Default::default()
    })
    .unwrap();

    let mut model_reg = MapModelRegistry::new();
    model_reg
        .register_model(ModelSpec::new("test-model", "anthropic", "claude-opus"))
        .unwrap();
    model_reg
        .register_model(ModelSpec::new("summary-model", "openai", "gpt-summary"))
        .unwrap();
    let mut provider_reg = MapProviderRegistry::new();
    provider_reg
        .register_provider("anthropic", Arc::new(MockExecutor))
        .unwrap();
    provider_reg
        .register_provider("openai", Arc::new(MockExecutor))
        .unwrap();
    let mut agent_reg = MapAgentSpecRegistry::new();
    agent_reg.register_spec(spec).unwrap();
    let regs = RegistrySet {
        agents: Arc::new(agent_reg),
        tools: Arc::new(MapToolRegistry::new()),
        models: Arc::new(model_reg),
        providers: Arc::new(provider_reg),
        plugins: Arc::new(MapPluginSource::new()),
        #[cfg(feature = "a2a")]
        backends: Arc::new(MapBackendRegistry::with_default_remote_backends())
            as Arc<dyn BackendRegistry>,
    };

    let err = resolve(&regs, "agent-1").unwrap_err();
    match err {
        ResolveError::InvalidPluginConfig { key, message, .. } => {
            assert_eq!(key, "compaction");
            assert!(message.contains("summary_model `summary-model`"));
            assert!(message.contains("provider `openai`"));
            assert!(message.contains("provider `anthropic`"));
        }
        other => panic!("expected InvalidPluginConfig, got {other:?}"),
    }
}

#[test]
fn resolve_agent_not_found() {
    let regs = build_registries(
        vec![],
        "m",
        ModelSpec::new("m", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![],
        make_spec("existing"),
    );

    let err = resolve(&regs, "missing").unwrap_err();
    assert!(matches!(err, ResolveError::AgentNotFound(ref id) if id == "missing"));
    assert!(err.to_string().contains("missing"));
}

#[test]
fn resolve_remote_agent_returns_error() {
    use awaken_runtime_contract::registry_spec::RemoteEndpoint;

    let spec = AgentSpec {
        endpoint: Some(RemoteEndpoint {
            backend: "a2a".into(),
            base_url: "https://remote.example.com".into(),
            ..Default::default()
        }),
        ..make_spec("remote-agent")
    };

    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![],
        spec,
    );

    let err = resolve(&regs, "remote-agent").unwrap_err();
    assert!(
        matches!(err, ResolveError::RemoteAgentNotDirectlyRunnable(ref id) if id == "remote-agent")
    );
    assert!(err.to_string().contains("remote-agent"));
    assert!(err.to_string().contains("cannot be resolved locally"));
}

#[cfg(feature = "a2a")]
#[test]
fn resolve_delegate_rejects_unknown_remote_backend() {
    use awaken_runtime_contract::registry_spec::RemoteEndpoint;

    let root = AgentSpec {
        delegates: vec!["remote-worker".into()],
        ..make_spec("root")
    };
    let remote = AgentSpec {
        id: "remote-worker".into(),
        endpoint: Some(RemoteEndpoint {
            backend: "acp".into(),
            base_url: "https://remote.example.com".into(),
            ..Default::default()
        }),
        ..make_spec("remote-worker")
    };

    let mut model_reg = MapModelRegistry::new();
    model_reg
        .register_model(ModelSpec::new("test-model", "p", "n"))
        .unwrap();

    let mut provider_reg = MapProviderRegistry::new();
    provider_reg
        .register_provider("p", Arc::new(MockExecutor))
        .unwrap();

    let mut agent_reg = MapAgentSpecRegistry::new();
    agent_reg.register_spec(root).unwrap();
    agent_reg.register_spec(remote).unwrap();

    let regs = RegistrySet {
        agents: Arc::new(agent_reg),
        tools: Arc::new(MapToolRegistry::new()),
        models: Arc::new(model_reg),
        providers: Arc::new(provider_reg),
        plugins: Arc::new(MapPluginSource::new()),
        backends: Arc::new(MapBackendRegistry::with_default_remote_backends())
            as Arc<dyn BackendRegistry>,
    };

    let err = resolve(&regs, "root").unwrap_err();
    assert!(matches!(
        err,
        ResolveError::UnsupportedRemoteBackend {
            ref agent_id,
            ref backend,
        } if agent_id == "remote-worker" && backend == "acp"
    ));
}

#[cfg(feature = "a2a")]
#[tokio::test]
async fn resolve_delegate_uses_registered_backend_factory() {
    let validate_count = Arc::new(AtomicUsize::new(0));
    let build_count = Arc::new(AtomicUsize::new(0));
    let root = AgentSpec {
        delegates: vec!["remote-worker".into()],
        ..make_spec("root")
    };
    let remote = AgentSpec {
        id: "remote-worker".into(),
        endpoint: Some(RemoteEndpoint {
            backend: "test-backend".into(),
            base_url: "https://remote.example.com".into(),
            ..Default::default()
        }),
        ..make_spec("remote-worker")
    };

    let mut model_reg = MapModelRegistry::new();
    model_reg
        .register_model(ModelSpec::new("test-model", "p", "n"))
        .unwrap();

    let mut provider_reg = MapProviderRegistry::new();
    provider_reg
        .register_provider("p", Arc::new(MockExecutor))
        .unwrap();

    let mut agent_reg = MapAgentSpecRegistry::new();
    agent_reg.register_spec(root).unwrap();
    agent_reg.register_spec(remote).unwrap();

    let mut backends = MapBackendRegistry::with_default_remote_backends();
    backends
        .register_backend_factory(Arc::new(StaticBackendFactory {
            backend: "test-backend",
            result: DelegateRunResult {
                agent_id: "remote-worker".into(),
                status: DelegateRunStatus::Completed,
                termination: TerminationReason::NaturalEnd,
                status_reason: None,
                response: Some("from custom backend".into()),
                output: crate::backend::BackendRunOutput::from_text(Some(
                    "from custom backend".into(),
                )),
                steps: 1,
                run_id: None,
                inbox: None,
                state: None,
            },
            validate_count: validate_count.clone(),
            build_count: build_count.clone(),
        }))
        .unwrap();

    let regs = RegistrySet {
        agents: Arc::new(agent_reg),
        tools: Arc::new(MapToolRegistry::new()),
        models: Arc::new(model_reg),
        providers: Arc::new(provider_reg),
        plugins: Arc::new(MapPluginSource::new()),
        backends: Arc::new(backends) as Arc<dyn BackendRegistry>,
    };

    let run = resolve(&regs, "root").unwrap();
    assert_eq!(validate_count.load(Ordering::SeqCst), 1);
    assert_eq!(build_count.load(Ordering::SeqCst), 0);
    let tool = run.tools.get("agent_run_remote-worker").unwrap();
    let output = tool
        .execute(
            serde_json::json!({ "prompt": "delegate this" }),
            &ToolCallContext::test_default(),
        )
        .await
        .unwrap();

    assert!(output.result.is_success());
    assert_eq!(output.result.data["response"], "from custom backend");
    assert_eq!(validate_count.load(Ordering::SeqCst), 2);
    assert_eq!(build_count.load(Ordering::SeqCst), 1);
}

#[test]
fn resolve_model_not_found() {
    let mut spec = make_spec("a");
    spec.model_id = "nonexistent-model".into();

    let regs = build_registries(
        vec![],
        "other-model",
        ModelSpec::new("other-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![],
        spec,
    );

    let err = resolve(&regs, "a").unwrap_err();
    assert!(matches!(err, ResolveError::ModelNotFound(ref id) if id == "nonexistent-model"));
}

#[test]
fn resolve_provider_not_found() {
    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "missing-provider", "n"),
        "other-provider",
        Arc::new(MockExecutor),
        vec![],
        make_spec("a"),
    );

    let err = resolve(&regs, "a").unwrap_err();
    assert!(matches!(err, ResolveError::ProviderNotFound(ref id) if id == "missing-provider"));
}

#[test]
fn resolve_invalid_retry_config_fails() {
    let spec = make_spec("a").with_section(
        "retry",
        serde_json::json!({
            "max_retries": "not-a-number"
        }),
    );

    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![],
        spec,
    );

    let err = resolve(&regs, "a").unwrap_err();
    match err {
        ResolveError::InvalidPluginConfig {
            plugin,
            key,
            message,
        } => {
            assert_eq!(plugin, "retry");
            assert_eq!(key, "retry");
            assert!(!message.is_empty(), "expected non-empty error message");
        }
        other => panic!("expected InvalidPluginConfig, got: {other:?}"),
    }
}

#[test]
fn resolve_plugin_not_found() {
    let spec = AgentSpec {
        plugin_ids: vec!["missing-plugin".into()],
        ..make_spec("a")
    };

    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![],
        spec,
    );

    let err = resolve(&regs, "a").unwrap_err();
    assert!(matches!(err, ResolveError::PluginNotFound(ref id) if id == "missing-plugin"));
}

#[test]
fn resolve_tool_allow_list() {
    let spec = AgentSpec {
        allowed_tools: Some(vec!["read".into()]),
        // Override Default's "*" pattern: this test wants the literal
        // allow list to be the *only* allow set.
        allowed_tool_patterns: Some(vec![]),
        ..make_spec("a")
    };

    let regs = build_registries(
        vec![
            ("read", Arc::new(MockTool { id: "read".into() })),
            ("write", Arc::new(MockTool { id: "write".into() })),
            (
                "delete",
                Arc::new(MockTool {
                    id: "delete".into(),
                }),
            ),
        ],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![],
        spec,
    );

    let run = resolve(&regs, "a").unwrap();
    assert_eq!(run.tools.len(), 1);
    assert!(run.tools.contains_key("read"));
}

#[test]
fn resolve_tool_exclude_list() {
    let spec = AgentSpec {
        excluded_tools: Some(vec!["delete".into()]),
        ..make_spec("a")
    };

    let regs = build_registries(
        vec![
            ("read", Arc::new(MockTool { id: "read".into() })),
            ("write", Arc::new(MockTool { id: "write".into() })),
            (
                "delete",
                Arc::new(MockTool {
                    id: "delete".into(),
                }),
            ),
        ],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![],
        spec,
    );

    let run = resolve(&regs, "a").unwrap();
    assert_eq!(run.tools.len(), 2);
    assert!(run.tools.contains_key("read"));
    assert!(run.tools.contains_key("write"));
    assert!(!run.tools.contains_key("delete"));
}

#[test]
fn resolve_tool_allow_and_exclude_combined() {
    let spec = AgentSpec {
        allowed_tools: Some(vec!["read".into(), "write".into(), "delete".into()]),
        excluded_tools: Some(vec!["delete".into()]),
        // Override Default's "*" pattern: this test pins the allow set
        // to the literal list so `exec` stays out.
        allowed_tool_patterns: Some(vec![]),
        ..make_spec("a")
    };

    let regs = build_registries(
        vec![
            ("read", Arc::new(MockTool { id: "read".into() })),
            ("write", Arc::new(MockTool { id: "write".into() })),
            (
                "delete",
                Arc::new(MockTool {
                    id: "delete".into(),
                }),
            ),
            ("exec", Arc::new(MockTool { id: "exec".into() })),
        ],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![],
        spec,
    );

    let run = resolve(&regs, "a").unwrap();
    assert_eq!(run.tools.len(), 2);
    assert!(run.tools.contains_key("read"));
    assert!(run.tools.contains_key("write"));
    assert!(!run.tools.contains_key("delete"));
    assert!(!run.tools.contains_key("exec"));
}

#[test]
fn resolve_empty_plugins_yields_empty_env() {
    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![],
        make_spec("a"),
    );

    let run = resolve(&regs, "a").unwrap();
    assert_eq!(run.env.plugins.len(), 2); // LoopActionHandlersPlugin + MaxRoundsPlugin
    // env has action handlers but no hooks
}

#[test]
fn resolve_error_display_strings() {
    let cases = vec![
        (
            ResolveError::AgentNotFound("x".into()),
            "agent not found: x",
        ),
        (
            ResolveError::ModelNotFound("y".into()),
            "model not found: y",
        ),
        (
            ResolveError::ProviderNotFound("z".into()),
            "provider not found: z",
        ),
        (
            ResolveError::PluginNotFound("w".into()),
            "plugin not found: w",
        ),
        (
            ResolveError::RemoteAgentNotDirectlyRunnable("r".into()),
            "remote agent `r` cannot be resolved locally — use it as a delegate instead",
        ),
        (
            ResolveError::ToolIdConflict {
                tool_id: "my_tool".into(),
                source_a: "global".into(),
                source_b: "plugin".into(),
            },
            "tool ID conflict: \"my_tool\" registered by both global and plugin",
        ),
    ];
    for (err, expected) in cases {
        assert_eq!(err.to_string(), expected);
    }
}

// -- AgentResolver bridge tests --

#[test]
fn registry_set_resolver_resolves_agent() {
    use crate::registry::AgentResolver;

    let spec = make_spec("my-agent");

    let regs = build_registries(
        vec![
            ("read", Arc::new(MockTool { id: "read".into() })),
            ("write", Arc::new(MockTool { id: "write".into() })),
        ],
        "test-model",
        ModelSpec::new("test-model", "p", "claude-test"),
        "p",
        Arc::new(MockExecutor),
        vec![],
        spec,
    );

    let resolver = RegistrySetResolver::new(regs);
    let resolved = AgentResolver::resolve(&resolver, "my-agent").unwrap();
    assert_eq!(resolved.id(), "my-agent");
    assert_eq!(resolved.model_id(), "test-model");
    assert_eq!(resolved.upstream_model, "claude-test");
    assert_eq!(resolved.system_prompt(), "You are helpful.");
    assert_eq!(resolved.tools.len(), 2);
    assert!(resolved.tools.contains_key("read"));
}

#[test]
fn registry_set_resolver_not_found() {
    use crate::registry::AgentResolver;

    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![],
        make_spec("existing"),
    );

    let resolver = RegistrySetResolver::new(regs);
    let err = AgentResolver::resolve(&resolver, "missing").unwrap_err();
    assert!(matches!(err, RuntimeError::ResolveFailed { .. }));
}

// -- Config validation tests --

/// Plugin that declares a config schema for eager validation.
struct ValidatedPlugin {
    name: &'static str,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct ValidatedConfig {
    pub mode: String,
    pub threshold: u32,
}

struct ValidatedConfigKey;
impl awaken_runtime_contract::PluginConfigKey for ValidatedConfigKey {
    const KEY: &'static str = "validated";
    type Config = ValidatedConfig;
}

impl Plugin for ValidatedPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: self.name }
    }

    fn config_schemas(&self) -> Vec<ConfigSchema> {
        vec![ConfigSchema::for_key::<ValidatedConfigKey>()]
    }
}

#[test]
fn validate_sections_valid_config_passes() {
    let spec = AgentSpec {
        plugin_ids: vec!["vp".into()],
        ..make_spec("a")
    }
    .with_section(
        "validated",
        serde_json::json!({"mode": "strict", "threshold": 42}),
    );

    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![("vp", Arc::new(ValidatedPlugin { name: "vp" }))],
        spec,
    );

    // Should succeed — config is valid
    let run = resolve(&regs, "a");
    assert!(run.is_ok());
}

#[test]
fn validate_sections_invalid_config_fails() {
    let spec = AgentSpec {
        plugin_ids: vec!["vp".into()],
        ..make_spec("a")
    }
    .with_section(
        "validated",
        serde_json::json!({"mode": 123, "threshold": "not_a_number"}),
    );

    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![("vp", Arc::new(ValidatedPlugin { name: "vp" }))],
        spec,
    );

    let err = resolve(&regs, "a").unwrap_err();
    match err {
        ResolveError::InvalidPluginConfig {
            plugin,
            key,
            message,
        } => {
            assert_eq!(plugin, "vp");
            assert_eq!(key, "validated");
            // JSON Schema validation error — exact message depends on jsonschema crate
            assert!(!message.is_empty(), "expected non-empty error message");
        }
        other => panic!("expected InvalidPluginConfig, got: {other:?}"),
    }
}

#[test]
fn validate_sections_missing_section_is_ok() {
    // Plugin declares schema but spec has no corresponding section — should pass
    let spec = AgentSpec {
        plugin_ids: vec!["vp".into()],
        ..make_spec("a")
    };

    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![("vp", Arc::new(ValidatedPlugin { name: "vp" }))],
        spec,
    );

    assert!(resolve(&regs, "a").is_ok());
}

#[test]
fn validate_sections_no_schema_plugin_still_works() {
    // Plugin without config_schemas — should not block any sections
    let spec = AgentSpec {
        plugin_ids: vec!["log".into()],
        ..make_spec("a")
    }
    .with_section("random_key", serde_json::json!({"anything": true}));

    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![("log", Arc::new(MockPlugin { name: "log" }))],
        spec,
    );

    // Resolves OK (unclaimed key just logs a warning, doesn't error)
    assert!(resolve(&regs, "a").is_ok());
}

// -- Plugin tool registration tests --

/// Plugin that registers a tool via PluginRegistrar.
struct ToolPlugin {
    name: &'static str,
    tool_id: &'static str,
}

impl Plugin for ToolPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: self.name }
    }

    fn register(
        &self,
        registrar: &mut PluginRegistrar,
    ) -> Result<(), awaken_runtime_contract::StateError> {
        registrar.register_tool(
            self.tool_id,
            Arc::new(MockTool {
                id: self.tool_id.into(),
            }),
        )?;
        Ok(())
    }
}

#[test]
fn resolve_plugin_registered_tools_are_available() {
    let spec = AgentSpec {
        plugin_ids: vec!["tp".into()],
        ..make_spec("a")
    };

    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![(
            "tp",
            Arc::new(ToolPlugin {
                name: "tp",
                tool_id: "plugin_tool",
            }),
        )],
        spec,
    );

    let run = resolve(&regs, "a").unwrap();
    assert!(run.tools.contains_key("plugin_tool"));
}

#[test]
fn resolve_plugin_tool_conflict_with_global_tool() {
    let spec = AgentSpec {
        plugin_ids: vec!["tp".into()],
        ..make_spec("a")
    };

    let regs = build_registries(
        vec![(
            "conflicting",
            Arc::new(MockTool {
                id: "conflicting".into(),
            }),
        )],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![(
            "tp",
            Arc::new(ToolPlugin {
                name: "tp",
                tool_id: "conflicting",
            }),
        )],
        spec,
    );

    let err = resolve(&regs, "a").unwrap_err();
    assert!(matches!(
        err,
        ResolveError::ToolIdConflict {
            ref tool_id,
            ..
        } if tool_id == "conflicting"
    ));
}

#[test]
fn resolve_plugin_tools_respect_exclude_filter() {
    let spec = AgentSpec {
        plugin_ids: vec!["tp".into()],
        excluded_tools: Some(vec!["plugin_tool".into()]),
        ..make_spec("a")
    };

    let regs = build_registries(
        vec![(
            "global_tool",
            Arc::new(MockTool {
                id: "global_tool".into(),
            }),
        )],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![(
            "tp",
            Arc::new(ToolPlugin {
                name: "tp",
                tool_id: "plugin_tool",
            }),
        )],
        spec,
    );

    let run = resolve(&regs, "a").unwrap();
    assert!(!run.tools.contains_key("plugin_tool"));
    assert!(run.tools.contains_key("global_tool"));
}

#[test]
fn resolve_plugin_tools_respect_allow_filter() {
    let spec = AgentSpec {
        plugin_ids: vec!["tp".into()],
        allowed_tools: Some(vec!["plugin_tool".into()]),
        // Override Default's "*" pattern: the filter must hide
        // `global_tool` even though it's registered.
        allowed_tool_patterns: Some(vec![]),
        ..make_spec("a")
    };

    let regs = build_registries(
        vec![(
            "global_tool",
            Arc::new(MockTool {
                id: "global_tool".into(),
            }),
        )],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![(
            "tp",
            Arc::new(ToolPlugin {
                name: "tp",
                tool_id: "plugin_tool",
            }),
        )],
        spec,
    );

    let run = resolve(&regs, "a").unwrap();
    assert!(run.tools.contains_key("plugin_tool"));
    assert!(!run.tools.contains_key("global_tool"));
}

// -----------------------------------------------------------------------
// Additional coverage: multi-plugin, empty specs, filter combos, defaults
// -----------------------------------------------------------------------

#[test]
fn resolve_multiple_plugins_all_loaded() {
    let spec = AgentSpec {
        plugin_ids: vec!["p1".into(), "p2".into(), "p3".into()],
        ..make_spec("a")
    };

    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![
            ("p1", Arc::new(MockPlugin { name: "p1" })),
            ("p2", Arc::new(MockPlugin { name: "p2" })),
            ("p3", Arc::new(MockPlugin { name: "p3" })),
        ],
        spec,
    );

    let run = resolve(&regs, "a").unwrap();
    // 3 user plugins + LoopActionHandlersPlugin + MaxRoundsPlugin
    assert_eq!(run.env.plugins.len(), 5);
}

#[test]
fn resolve_no_tools_yields_empty_tool_set() {
    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![],
        make_spec("a"),
    );

    let run = resolve(&regs, "a").unwrap();
    assert!(run.tools.is_empty());
}

#[test]
fn resolve_spec_max_rounds_propagated() {
    let spec = AgentSpec {
        max_rounds: 42,
        ..make_spec("a")
    };

    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![],
        spec,
    );

    let run = resolve(&regs, "a").unwrap();
    assert_eq!(run.max_rounds(), 42);
}

#[test]
fn resolve_spec_system_prompt_propagated() {
    let spec = AgentSpec {
        system_prompt: "Custom instructions for the agent.".into(),
        ..make_spec("a")
    };

    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![],
        spec,
    );

    let run = resolve(&regs, "a").unwrap();
    assert_eq!(run.system_prompt(), "Custom instructions for the agent.");
}

#[test]
fn resolve_upstream_model_from_model_spec() {
    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "claude-opus-4-20250514"),
        "p",
        Arc::new(MockExecutor),
        vec![],
        make_spec("a"),
    );

    let run = resolve(&regs, "a").unwrap();
    assert_eq!(run.upstream_model, "claude-opus-4-20250514");
}

#[test]
fn resolve_empty_allow_list_removes_all_tools() {
    let spec = AgentSpec {
        allowed_tools: Some(vec![]),
        // Override Default's "*" pattern: an empty allow set must mean
        // "no tools", not "all tools via wildcard".
        allowed_tool_patterns: Some(vec![]),
        ..make_spec("a")
    };

    let regs = build_registries(
        vec![
            ("read", Arc::new(MockTool { id: "read".into() })),
            ("write", Arc::new(MockTool { id: "write".into() })),
        ],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![],
        spec,
    );

    let run = resolve(&regs, "a").unwrap();
    assert!(run.tools.is_empty(), "empty allow list removes all tools");
}

#[test]
fn resolve_exclude_nonexistent_tool_is_noop() {
    let spec = AgentSpec {
        excluded_tools: Some(vec!["nonexistent".into()]),
        ..make_spec("a")
    };

    let regs = build_registries(
        vec![("read", Arc::new(MockTool { id: "read".into() }))],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![],
        spec,
    );

    let run = resolve(&regs, "a").unwrap();
    assert_eq!(run.tools.len(), 1);
    assert!(run.tools.contains_key("read"));
}

#[test]
fn resolve_default_spec_has_expected_defaults() {
    let spec = make_spec("default-agent");

    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![],
        spec,
    );

    let run = resolve(&regs, "default-agent").unwrap();
    assert_eq!(run.max_rounds(), 16);
    assert_eq!(run.max_continuation_retries(), 2);
    assert!(run.context_policy().is_none());
    assert!(run.spec.allowed_tools.is_none());
    assert!(run.spec.excluded_tools.is_none());
    assert!(run.spec.plugin_ids.is_empty());
    assert!(run.spec.delegates.is_empty());
}

#[test]
fn resolver_agent_ids_returns_all_registered() {
    use crate::registry::AgentResolver;

    let mut agent_reg = MapAgentSpecRegistry::new();
    agent_reg.register_spec(make_spec("a1")).unwrap();
    agent_reg.register_spec(make_spec("a2")).unwrap();
    agent_reg.register_spec(make_spec("a3")).unwrap();

    let mut model_reg = MapModelRegistry::new();
    model_reg
        .register_model(ModelSpec::new("test-model", "p", "n"))
        .unwrap();

    let mut provider_reg = MapProviderRegistry::new();
    provider_reg
        .register_provider("p", Arc::new(MockExecutor))
        .unwrap();

    let regs = RegistrySet {
        agents: Arc::new(agent_reg),
        tools: Arc::new(MapToolRegistry::new()),
        models: Arc::new(model_reg),
        providers: Arc::new(provider_reg),
        plugins: Arc::new(MapPluginSource::new()),
        #[cfg(feature = "a2a")]
        backends: Arc::new(MapBackendRegistry::with_default_remote_backends())
            as Arc<dyn BackendRegistry>,
    };

    let resolver = RegistrySetResolver::new(regs);
    let mut ids = resolver.agent_ids();
    ids.sort();
    assert_eq!(ids, vec!["a1", "a2", "a3"]);
}

#[test]
fn filter_tools_respects_pattern_fields() {
    use std::collections::HashMap;

    // Three-tool map.
    let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
    tools.insert("Bash".into(), Arc::new(MockTool { id: "Bash".into() }));
    tools.insert(
        "mcp:weather".into(),
        Arc::new(MockTool {
            id: "mcp:weather".into(),
        }),
    );
    tools.insert(
        "mcp:fs".into(),
        Arc::new(MockTool {
            id: "mcp:fs".into(),
        }),
    );

    // Spec: allow [Bash] + pattern [mcp:*], exclude [mcp:fs].
    let mut spec: AgentSpec =
        serde_json::from_str(r#"{"id":"a","model_id":"m","system_prompt":""}"#).unwrap();
    spec.allowed_tools = Some(vec!["Bash".into()]);
    spec.allowed_tool_patterns = Some(vec!["mcp:*".into()]);
    spec.excluded_tools = Some(vec!["mcp:fs".into()]);

    super::filter::filter_tools(&mut tools, &spec);

    let mut ids: Vec<_> = tools.keys().cloned().collect();
    ids.sort();
    assert_eq!(ids, vec!["Bash".to_string(), "mcp:weather".to_string()]);
}

#[test]
fn inject_default_plugins_adds_required_plugins() {
    let plugins = inject_default_plugins(vec![], 10);
    assert_eq!(plugins.len(), 2);
    // The two default plugins: LoopActionHandlersPlugin and MaxRoundsPlugin
    let names: Vec<&str> = plugins.iter().map(|p| p.descriptor().name).collect();
    assert!(names.contains(&"__loop_action_handlers"));
    assert!(names.contains(&"stop-condition:max-rounds"));
}

// -- Plugin config persistence tests --
//
// Verify that AgentSpec.sections survive plugin activation/deactivation
// cycles.  Config for inactive plugins must not be discarded — the user
// may re-enable a plugin later and expects its config to still be there.

/// Plugin that reads typed config from AgentSpec.sections during on_activate.
struct StatefulPlugin {
    name: &'static str,
}

#[derive(
    Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
struct StatefulPluginConfig {
    pub level: String,
    pub max_items: u32,
}

struct StatefulPluginConfigKey;
impl awaken_runtime_contract::PluginConfigKey for StatefulPluginConfigKey {
    const KEY: &'static str = "stateful";
    type Config = StatefulPluginConfig;
}

impl Plugin for StatefulPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: self.name }
    }

    fn config_schemas(&self) -> Vec<ConfigSchema> {
        vec![ConfigSchema::for_key::<StatefulPluginConfigKey>()]
    }

    fn on_activate(
        &self,
        agent_spec: &AgentSpec,
        _patch: &mut crate::state::MutationBatch,
    ) -> Result<(), awaken_runtime_contract::StateError> {
        // Verify we can read typed config during activation
        let _config = agent_spec.config::<StatefulPluginConfigKey>()?;
        Ok(())
    }
}

fn stateful_config() -> serde_json::Value {
    serde_json::json!({"level": "debug", "max_items": 100})
}

#[test]
fn config_sections_preserved_when_plugin_removed_from_plugin_ids() {
    // Agent has config section for "stateful" plugin but plugin is NOT in plugin_ids.
    let spec = AgentSpec {
        plugin_ids: vec![], // No plugins active
        ..make_spec("a")
    }
    .with_section("stateful", stateful_config());

    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![("sp", Arc::new(StatefulPlugin { name: "sp" }))],
        spec,
    );

    // Resolve succeeds — unclaimed section is just a warning, not an error.
    let resolved = resolve(&regs, "a").unwrap();

    // The section is still present in the resolved spec.
    assert!(
        resolved.spec.sections.contains_key("stateful"),
        "config section must survive even when its plugin is not active"
    );
    assert_eq!(resolved.spec.sections["stateful"], stateful_config());
}

#[test]
fn reactivating_plugin_picks_up_existing_config_section() {
    // Step 1: Resolve WITHOUT the plugin — config section survives.
    let spec_without = AgentSpec {
        plugin_ids: vec![],
        ..make_spec("a")
    }
    .with_section("stateful", stateful_config());

    let regs_without = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![("sp", Arc::new(StatefulPlugin { name: "sp" }))],
        spec_without,
    );
    let resolved_without = resolve(&regs_without, "a").unwrap();
    assert!(resolved_without.spec.sections.contains_key("stateful"));

    // Step 2: Resolve WITH the plugin re-enabled — config validates and activates.
    let spec_with = AgentSpec {
        plugin_ids: vec!["sp".into()],
        sections: resolved_without.spec.sections.clone(),
        ..make_spec("a")
    };

    let regs_with = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![("sp", Arc::new(StatefulPlugin { name: "sp" }))],
        spec_with,
    );

    // Should succeed — config section validates against plugin's schema.
    let resolved_with = resolve(&regs_with, "a").unwrap();
    assert_eq!(resolved_with.spec.sections["stateful"], stateful_config());
}

#[test]
fn on_activate_reads_typed_config_from_sections() {
    let config = StatefulPluginConfig {
        level: "warn".into(),
        max_items: 50,
    };
    let spec = AgentSpec {
        plugin_ids: vec!["sp".into()],
        ..make_spec("a")
    }
    .with_section("stateful", serde_json::to_value(&config).unwrap());

    // Verify typed read works at spec level
    let read_config = spec.config::<StatefulPluginConfigKey>().unwrap();
    assert_eq!(read_config, config);

    // Resolve succeeds (on_activate also reads the config without error)
    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![("sp", Arc::new(StatefulPlugin { name: "sp" }))],
        spec,
    );
    assert!(resolve(&regs, "a").is_ok());
}

#[test]
fn on_deactivate_does_not_clear_sections() {
    let plugin = StatefulPlugin { name: "sp" };

    // Simulate deactivation
    let mut patch = crate::state::MutationBatch::new();
    plugin.on_deactivate(&mut patch).unwrap();

    // Default on_deactivate is a no-op — no mutations emitted.
    assert!(
        patch.is_empty(),
        "on_deactivate should not emit mutations that clear config sections"
    );
}

#[test]
fn multiple_plugin_sections_survive_partial_activation() {
    // Agent has config for two plugins but only activates one.
    let spec = AgentSpec {
        plugin_ids: vec!["vp".into()], // Only ValidatedPlugin active
        ..make_spec("a")
    }
    .with_section(
        "validated",
        serde_json::json!({"mode": "strict", "threshold": 10}),
    )
    .with_section("stateful", stateful_config()); // StatefulPlugin NOT active

    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![
            ("vp", Arc::new(ValidatedPlugin { name: "vp" })),
            ("sp", Arc::new(StatefulPlugin { name: "sp" })),
        ],
        spec,
    );

    let resolved = resolve(&regs, "a").unwrap();

    // Both sections survive — active plugin's config is validated,
    // inactive plugin's config is kept without validation.
    assert!(resolved.spec.sections.contains_key("validated"));
    assert!(
        resolved.spec.sections.contains_key("stateful"),
        "inactive plugin's config section must be preserved"
    );
}

#[test]
fn config_defaults_when_section_absent_and_plugin_active() {
    // Plugin is active but has no config section — should use defaults.
    let spec = AgentSpec {
        plugin_ids: vec!["sp".into()],
        ..make_spec("a")
    };
    // No .with_section("stateful", ...) — section absent.

    let read_config = spec.config::<StatefulPluginConfigKey>().unwrap();
    assert_eq!(read_config, StatefulPluginConfig::default());

    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "p", "n"),
        "p",
        Arc::new(MockExecutor),
        vec![("sp", Arc::new(StatefulPlugin { name: "sp" }))],
        spec,
    );
    assert!(resolve(&regs, "a").is_ok());
}

// -- ContextWindowPolicy / ModelSpec clamping --------------------------------

#[test]
fn build_plugin_chain_clamps_context_policy_to_model_capabilities() {
    use awaken_runtime_contract::contract::inference::ContextWindowPolicy;

    let spec = AgentSpec {
        context_policy: Some(ContextWindowPolicy {
            max_context_tokens: 200_000,
            max_output_tokens: 16_384,
            ..Default::default()
        }),
        ..make_spec("a")
    };

    let model_spec = ModelSpec {
        context_window: Some(32_000),
        max_output_tokens: Some(4_096),
        ..ModelSpec::new("test-model", "p", "u")
    };

    let regs = build_registries(
        vec![],
        "test-model",
        model_spec,
        "p",
        Arc::new(MockExecutor),
        vec![],
        spec,
    );

    let run = resolve(&regs, "a").unwrap();
    let runtime_policy = run
        .context_policy()
        .expect("resolved agent should carry effective context policy");
    assert_eq!(runtime_policy.max_context_tokens, 32_000);
    assert_eq!(runtime_policy.max_output_tokens, 4_096);
    let plugin_arc = run
        .env
        .plugins
        .iter()
        .find(|p| p.descriptor().name == crate::context::CONTEXT_TRANSFORM_PLUGIN_ID)
        .cloned()
        .expect("ContextTransformPlugin must be installed when context_policy is set");
    let transform = plugin_arc
        .as_any()
        .downcast_ref::<crate::context::ContextTransformPlugin>()
        .expect("plugin must be ContextTransformPlugin");
    assert_eq!(transform.policy().max_context_tokens, 32_000);
    assert_eq!(transform.policy().max_output_tokens, 4_096);
}

#[test]
fn resolved_agent_context_policy_clamps_autocompact_to_model_usable_input() {
    use awaken_runtime_contract::contract::inference::ContextWindowPolicy;

    let spec = AgentSpec {
        context_policy: Some(ContextWindowPolicy {
            max_context_tokens: 200_000,
            max_output_tokens: 16_384,
            autocompact_threshold: Some(150_000),
            ..Default::default()
        }),
        ..make_spec("a")
    };

    let model_spec = ModelSpec {
        context_window: Some(100_000),
        max_output_tokens: Some(8_192),
        ..ModelSpec::new("test-model", "p", "u")
    };

    let regs = build_registries(
        vec![],
        "test-model",
        model_spec,
        "p",
        Arc::new(MockExecutor),
        vec![],
        spec,
    );

    let run = resolve(&regs, "a").unwrap();
    let policy = run
        .context_policy()
        .expect("resolved agent should carry effective context policy");
    assert_eq!(policy.max_context_tokens, 100_000);
    assert_eq!(policy.max_output_tokens, 8_192);
    assert_eq!(policy.autocompact_threshold, Some(91_808));
}

#[test]
fn build_plugin_chain_backfills_common_model_capabilities() {
    use awaken_runtime_contract::contract::inference::ContextWindowPolicy;

    let spec = AgentSpec {
        context_policy: Some(ContextWindowPolicy {
            max_context_tokens: 300_000,
            max_output_tokens: 50_000,
            ..Default::default()
        }),
        ..make_spec("a")
    };

    let regs = build_registries(
        vec![],
        "test-model",
        ModelSpec::new("test-model", "openai", "gpt-4o"),
        "openai",
        Arc::new(MockExecutor),
        vec![],
        spec,
    );

    let run = resolve(&regs, "a").unwrap();
    let plugin_arc = run
        .env
        .plugins
        .iter()
        .find(|p| p.descriptor().name == crate::context::CONTEXT_TRANSFORM_PLUGIN_ID)
        .cloned()
        .expect("ContextTransformPlugin must be installed when context_policy is set");
    let transform = plugin_arc
        .as_any()
        .downcast_ref::<crate::context::ContextTransformPlugin>()
        .expect("plugin must be ContextTransformPlugin");
    assert_eq!(transform.policy().max_context_tokens, 128_000);
    assert_eq!(transform.policy().max_output_tokens, 16_384);
}

#[test]
fn build_plugin_chain_uses_provider_capability_source_alias() {
    use awaken_runtime_contract::contract::inference::ContextWindowPolicy;

    let spec = AgentSpec {
        context_policy: Some(ContextWindowPolicy {
            max_context_tokens: 300_000,
            max_output_tokens: 50_000,
            ..Default::default()
        }),
        ..make_spec("a")
    };
    let regs = build_registries_with_provider_source(
        ModelSpec::new("test-model", "prod-openai", "gpt-4o-mini"),
        "prod-openai",
        "openai",
        spec,
    );

    let run = resolve(&regs, "a").unwrap();
    let plugin_arc = run
        .env
        .plugins
        .iter()
        .find(|p| p.descriptor().name == crate::context::CONTEXT_TRANSFORM_PLUGIN_ID)
        .cloned()
        .expect("ContextTransformPlugin must be installed when context_policy is set");
    let transform = plugin_arc
        .as_any()
        .downcast_ref::<crate::context::ContextTransformPlugin>()
        .expect("plugin must be ContextTransformPlugin");
    assert_eq!(transform.policy().max_context_tokens, 128_000);
    assert_eq!(transform.policy().max_output_tokens, 16_384);
}

#[test]
fn pool_reconciliation_uses_backfilled_member_capabilities() {
    use awaken_runtime_contract::contract::inference::ContextWindowPolicy;

    let spec = AgentSpec {
        model_id: "my-pool".into(),
        context_policy: Some(ContextWindowPolicy {
            max_context_tokens: 2_000_000,
            max_output_tokens: 100_000,
            ..Default::default()
        }),
        ..make_spec("agent-1")
    };
    let regs = build_pool_registries(
        vec![
            ModelSpec::new("m0", "openai", "gpt-4o"),
            ModelSpec::new("m1", "openai", "gpt-4.1"),
        ],
        ModelPoolSpec::new("my-pool", ["m0", "m1"]),
        "openai",
        Arc::new(MockExecutor),
        spec,
    );

    let run = resolve(&regs, "agent-1").expect("pool model_id resolves");
    let plugin_arc = run
        .env
        .plugins
        .iter()
        .find(|p| p.descriptor().name == crate::context::CONTEXT_TRANSFORM_PLUGIN_ID)
        .cloned()
        .expect("ContextTransformPlugin must be installed when context_policy is set");
    let transform = plugin_arc
        .as_any()
        .downcast_ref::<crate::context::ContextTransformPlugin>()
        .expect("plugin must be ContextTransformPlugin");
    assert_eq!(transform.policy().max_context_tokens, 128_000);
    assert_eq!(transform.policy().max_output_tokens, 16_384);
}
