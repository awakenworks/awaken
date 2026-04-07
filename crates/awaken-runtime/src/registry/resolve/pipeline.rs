//! Resolution pipeline: `agent_id` + `RegistrySet` -> `ResolvedAgent`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::error::RuntimeError;
use crate::execution::SequentialToolExecutor;
use crate::phase::ExecutionEnv;
use crate::plugins::Plugin;
use crate::registry::{AgentResolver, ResolvedAgent};
use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::tool::Tool;

use crate::registry::traits::RegistrySet;
use awaken_contract::registry_spec::AgentSpec;

use super::error::ResolveError;

// ---------------------------------------------------------------------------
// inject_default_plugins()
// ---------------------------------------------------------------------------

/// Inject runtime-required default plugins into a plugin list.
///
/// These plugins are always needed for the agent loop to function correctly.
/// Called from both the resolve pipeline and `build_agent_env()`.
pub(crate) fn inject_default_plugins(
    mut plugins: Vec<Arc<dyn Plugin>>,
    max_rounds: usize,
) -> Vec<Arc<dyn Plugin>> {
    plugins.push(Arc::new(
        crate::loop_runner::actions::LoopActionHandlersPlugin,
    ));
    plugins.push(Arc::new(crate::policies::MaxRoundsPlugin::new(max_rounds)));
    plugins
}

// ---------------------------------------------------------------------------
// resolve()
// ---------------------------------------------------------------------------

/// Resolve an agent by ID from registries into a fully wired [`ResolvedAgent`].
///
/// Three-stage pipeline:
/// 1. **Lookup** — fetch spec, model, executor from registries.
/// 2. **Plugin pipeline** — resolve plugins, inject defaults, validate config.
/// 3. **Tool pipeline** — collect global + delegate + plugin tools, filter.
fn resolve(registries: &RegistrySet, agent_id: &str) -> Result<ResolvedAgent, ResolveError> {
    // Stage 1: Lookup
    let spec = lookup_spec(registries, agent_id)?;
    let (executor, model_name) = resolve_model_and_executor(registries, &spec)?;

    // Stage 2: Plugin pipeline
    let plugins = build_plugin_chain(registries, &spec)?;
    let env = ExecutionEnv::from_plugins(&plugins, &spec.active_hook_filter)?;

    // Stage 3: Tool pipeline
    let tools = build_tool_set(registries, &spec, &env)?;

    // Build ResolvedAgent with all fields
    let spec_arc = Arc::new(spec);

    Ok(ResolvedAgent {
        spec: spec_arc,
        model: model_name,
        tools,
        llm_executor: executor,
        tool_executor: Arc::new(SequentialToolExecutor),
        context_summarizer: None,
        env,
    })
}

// ---------------------------------------------------------------------------
// Stage 1: Lookup
// ---------------------------------------------------------------------------

/// Fetch and validate the agent spec from registry.
fn lookup_spec(registries: &RegistrySet, agent_id: &str) -> Result<AgentSpec, ResolveError> {
    let spec = registries
        .agents
        .get_agent(agent_id)
        .ok_or_else(|| ResolveError::AgentNotFound(agent_id.into()))?;

    #[cfg(feature = "a2a")]
    if spec.endpoint.is_some() {
        return Err(ResolveError::RemoteAgentNotDirectlyRunnable(
            spec.id.clone(),
        ));
    }

    Ok(spec)
}

/// Resolve model and LLM executor, applying retry decoration if configured.
fn resolve_model_and_executor(
    registries: &RegistrySet,
    spec: &AgentSpec,
) -> Result<(Arc<dyn LlmExecutor>, String), ResolveError> {
    let model = registries
        .models
        .get_model(&spec.model)
        .ok_or_else(|| ResolveError::ModelNotFound(spec.model.clone()))?;

    let executor = registries
        .providers
        .get_provider(&model.provider)
        .ok_or_else(|| ResolveError::ProviderNotFound(model.provider.clone()))?;

    let executor = match spec.config::<crate::engine::RetryConfigKey>() {
        Ok(policy) if policy.max_retries > 0 || !policy.fallback_models.is_empty() => {
            Arc::new(crate::engine::RetryingExecutor::new(executor, policy)) as Arc<dyn LlmExecutor>
        }
        _ => executor,
    };

    Ok((executor, model.model_name.clone()))
}

// ---------------------------------------------------------------------------
// Stage 2: Plugin pipeline
// ---------------------------------------------------------------------------

/// Resolve plugins by ID, inject defaults, add conditional plugins, validate.
fn build_plugin_chain(
    registries: &RegistrySet,
    spec: &AgentSpec,
) -> Result<Vec<Arc<dyn Plugin>>, ResolveError> {
    // User-declared plugins
    let plugins = resolve_plugins(registries, spec)?;

    // Runtime-required default plugins
    let mut plugins = inject_default_plugins(plugins, spec.max_rounds);

    // Conditional plugins (only when context_policy is set)
    if let Some(ref policy) = spec.context_policy {
        let compaction_config = spec
            .config::<crate::context::CompactionConfigKey>()
            .unwrap_or_default();
        plugins.push(Arc::new(crate::context::CompactionPlugin::new(
            compaction_config,
        )));
        plugins.push(Arc::new(crate::context::ContextTransformPlugin::new(
            policy.clone(),
        )));
    }

    // Validate spec sections against plugin-declared schemas
    validate_sections(spec, &plugins)?;

    Ok(plugins)
}

// ---------------------------------------------------------------------------
// Stage 3: Tool pipeline
// ---------------------------------------------------------------------------

/// Collect tools from all sources, detect conflicts, apply filters.
///
/// Tool sources (merged in order):
/// 1. Global tools from `ToolRegistry` (builder-registered)
/// 2. Delegate agent tools (A2A, created from `spec.delegates`)
/// 3. Plugin-registered tools (from `ExecutionEnv`)
///
/// After merging, `allowed_tools`/`excluded_tools` filtering is applied.
fn build_tool_set(
    registries: &RegistrySet,
    spec: &AgentSpec,
    env: &ExecutionEnv,
) -> Result<HashMap<String, Arc<dyn Tool>>, ResolveError> {
    let mut tools = collect_global_tools(registries);

    // Merge delegate agent tools
    resolve_delegate_tools(registries, spec, &mut tools)?;

    // Merge plugin-registered tools (conflict with global = error)
    for (tool_id, tool) in &env.tools {
        if tools.contains_key(tool_id) {
            return Err(ResolveError::ToolIdConflict {
                tool_id: tool_id.clone(),
                source_a: "global".into(),
                source_b: "plugin".into(),
            });
        }
        tools.insert(tool_id.clone(), Arc::clone(tool));
    }

    // Apply allow/exclude filtering to the full merged tool set
    filter_tools(&mut tools, spec);

    Ok(tools)
}

/// Create delegate agent tools from `spec.delegates`.
#[cfg_attr(not(feature = "a2a"), allow(unused_variables))]
fn resolve_delegate_tools(
    registries: &RegistrySet,
    spec: &AgentSpec,
    tools: &mut HashMap<String, Arc<dyn Tool>>,
) -> Result<(), ResolveError> {
    #[cfg(feature = "a2a")]
    if !spec.delegates.is_empty() {
        for delegate_id in &spec.delegates {
            let delegate_spec = registries
                .agents
                .get_agent(delegate_id)
                .ok_or_else(|| ResolveError::AgentNotFound(delegate_id.clone()))?;

            let description: String = delegate_spec.system_prompt.chars().take(100).collect();

            let tool: Arc<dyn Tool> = if let Some(endpoint) = &delegate_spec.endpoint {
                let factory = registries
                    .backends
                    .get_backend_factory(&endpoint.backend)
                    .ok_or_else(|| ResolveError::UnsupportedRemoteBackend {
                        agent_id: delegate_id.clone(),
                        backend: endpoint.backend.clone(),
                    })?;
                let backend = factory.build(endpoint).map_err(|error| {
                    ResolveError::InvalidRemoteEndpointConfig {
                        agent_id: delegate_id.clone(),
                        backend: endpoint.backend.clone(),
                        message: error.to_string(),
                    }
                })?;
                Arc::new(crate::extensions::a2a::AgentTool::with_backend(
                    delegate_id,
                    &description,
                    backend,
                ))
            } else {
                let resolver: Arc<dyn crate::registry::AgentResolver> =
                    Arc::new(RegistrySetResolver::new(registries.clone()));
                Arc::new(crate::extensions::a2a::AgentTool::local(
                    delegate_id,
                    &description,
                    resolver,
                ))
            };
            let tool_id = tool.descriptor().id;
            tools.insert(tool_id, tool);
        }
    }
    #[cfg(not(feature = "a2a"))]
    if !spec.delegates.is_empty() {
        tracing::warn!(
            agent_id = %spec.id,
            "agent has delegates but 'a2a' feature is disabled; delegates ignored"
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// AgentResolver implementation
// ---------------------------------------------------------------------------

/// Resolver that bridges `RegistrySet` into `AgentResolver`.
///
/// Separates the registry aggregation concern (`RegistrySet`) from the
/// resolution logic. `RegistrySet` stays a pure data container.
pub(crate) struct RegistrySetResolver {
    registries: RegistrySet,
}

impl RegistrySetResolver {
    pub(crate) fn new(registries: RegistrySet) -> Self {
        Self { registries }
    }
}

impl AgentResolver for RegistrySetResolver {
    fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        resolve(&self.registries, agent_id).map_err(|e| RuntimeError::ResolveFailed {
            message: e.to_string(),
        })
    }

    fn agent_ids(&self) -> Vec<String> {
        self.registries.agents.agent_ids()
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Validate spec sections against plugin-declared JSON Schemas.
///
/// For each plugin that declares `config_schemas()`, validates the
/// corresponding section in `AgentSpec.sections` against its JSON Schema.
/// Missing sections are fine (plugins fall back to defaults). Invalid
/// sections produce `ResolveError::InvalidPluginConfig`.
///
/// Also logs a warning for any section keys not claimed by any plugin.
fn validate_sections(spec: &AgentSpec, plugins: &[Arc<dyn Plugin>]) -> Result<(), ResolveError> {
    let mut claimed_keys: HashSet<&str> = HashSet::new();

    for plugin in plugins {
        let schemas = plugin.config_schemas();
        for schema in &schemas {
            claimed_keys.insert(schema.key);
            if let Some(value) = spec.sections.get(schema.key) {
                jsonschema::validate(&schema.json_schema, value).map_err(|e| {
                    ResolveError::InvalidPluginConfig {
                        plugin: plugin.descriptor().name.into(),
                        key: schema.key.into(),
                        message: e.to_string(),
                    }
                })?;
            }
        }
    }

    // Warn about unclaimed section keys
    for key in spec.sections.keys() {
        if !claimed_keys.contains(key.as_str()) {
            tracing::warn!(
                agent_id = %spec.id,
                key = %key,
                "section key not claimed by any plugin — possible typo"
            );
        }
    }

    Ok(())
}

/// Collect all global (builder-registered) tools from the registry.
fn collect_global_tools(registries: &RegistrySet) -> HashMap<String, Arc<dyn Tool>> {
    let mut tools = HashMap::new();
    for id in registries.tools.tool_ids() {
        if let Some(tool) = registries.tools.get_tool(&id) {
            tools.insert(id, tool);
        }
    }
    tools
}

/// Apply allow/exclude filtering to a mutable tool map.
///
/// - `allowed_tools = None` -> keep all.
/// - `allowed_tools = Some(list)` -> keep only those IDs.
/// - `excluded_tools` -> remove from the set.
fn filter_tools(tools: &mut HashMap<String, Arc<dyn Tool>>, spec: &AgentSpec) {
    if let Some(allow) = &spec.allowed_tools {
        let allowed: HashSet<&str> = allow.iter().map(|s| s.as_str()).collect();
        tools.retain(|id, _| allowed.contains(id.as_str()));
    }

    if let Some(exclude) = &spec.excluded_tools {
        for id in exclude {
            tools.remove(id);
        }
    }
}

/// Resolve plugins by IDs from the spec.
fn resolve_plugins(
    registries: &RegistrySet,
    spec: &AgentSpec,
) -> Result<Vec<Arc<dyn Plugin>>, ResolveError> {
    spec.plugin_ids
        .iter()
        .map(|id| {
            registries
                .plugins
                .get_plugin(id)
                .ok_or_else(|| ResolveError::PluginNotFound(id.clone()))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "a2a")]
    use crate::extensions::a2a::{
        AgentBackend, AgentBackendError, AgentBackendFactory, AgentBackendFactoryError,
        DelegateRunResult, DelegateRunStatus,
    };
    use crate::plugins::{PluginDescriptor, PluginRegistrar};
    #[cfg(feature = "a2a")]
    use crate::registry::BackendRegistry;
    #[cfg(feature = "a2a")]
    use crate::registry::memory::MapBackendRegistry;
    use crate::registry::memory::{
        MapAgentSpecRegistry, MapModelRegistry, MapPluginSource, MapProviderRegistry,
        MapToolRegistry,
    };
    use crate::registry::traits::ModelEntry;
    use async_trait::async_trait;
    #[cfg(feature = "a2a")]
    use awaken_contract::contract::event_sink::EventSink;
    use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
    use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    #[cfg(feature = "a2a")]
    use awaken_contract::contract::message::Message;
    use awaken_contract::contract::tool::{
        ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
    };
    #[cfg(feature = "a2a")]
    use awaken_contract::registry_spec::RemoteEndpoint;
    use serde_json::Value;

    // -- Mock Tool --

    struct MockTool {
        id: String,
    }

    #[async_trait]
    impl Tool for MockTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new(&self.id, &self.id, "mock tool")
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
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

    #[cfg(feature = "a2a")]
    struct StaticBackend {
        result: DelegateRunResult,
    }

    #[cfg(feature = "a2a")]
    #[async_trait]
    impl AgentBackend for StaticBackend {
        async fn execute(
            &self,
            _agent_id: &str,
            _messages: Vec<Message>,
            _event_sink: Arc<dyn EventSink>,
            _parent_run_id: Option<String>,
            _parent_tool_call_id: Option<String>,
        ) -> Result<DelegateRunResult, AgentBackendError> {
            Ok(self.result.clone())
        }
    }

    #[cfg(feature = "a2a")]
    struct StaticBackendFactory {
        backend: &'static str,
        result: DelegateRunResult,
    }

    #[cfg(feature = "a2a")]
    impl AgentBackendFactory for StaticBackendFactory {
        fn backend(&self) -> &str {
            self.backend
        }

        fn build(
            &self,
            endpoint: &RemoteEndpoint,
        ) -> Result<Arc<dyn AgentBackend>, AgentBackendFactoryError> {
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
        model_entry: ModelEntry,
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

        let mut model_reg = MapModelRegistry::new();
        model_reg
            .register_model(model_id, model_entry)
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

    fn make_spec(id: &str) -> AgentSpec {
        AgentSpec {
            id: id.into(),
            model: "test-model".into(),
            system_prompt: "You are helpful.".into(),
            ..Default::default()
        }
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
            ModelEntry {
                provider: "anthropic".into(),
                model_name: "claude-opus-4-20250514".into(),
            },
            "anthropic",
            Arc::new(MockExecutor),
            vec![("log", Arc::new(MockPlugin { name: "log" }))],
            spec,
        );

        let run = resolve(&regs, "agent-1").unwrap();
        assert_eq!(run.id(), "agent-1");
        assert_eq!(run.model, "claude-opus-4-20250514");
        assert_eq!(run.tools.len(), 2);
        assert!(run.tools.contains_key("read"));
        assert!(run.tools.contains_key("write"));
        assert_eq!(run.env.plugins.len(), 3); // user plugin + LoopActionHandlersPlugin + MaxRoundsPlugin
    }

    #[test]
    fn resolve_agent_not_found() {
        let regs = build_registries(
            vec![],
            "m",
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
        use awaken_contract::registry_spec::RemoteEndpoint;

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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
        use awaken_contract::registry_spec::RemoteEndpoint;

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
            .register_model(
                "test-model",
                ModelEntry {
                    provider: "p".into(),
                    model_name: "n".into(),
                },
            )
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
            .register_model(
                "test-model",
                ModelEntry {
                    provider: "p".into(),
                    model_name: "n".into(),
                },
            )
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
                    response: Some("from custom backend".into()),
                    steps: 1,
                    run_id: None,
                },
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
    }

    #[test]
    fn resolve_model_not_found() {
        let mut spec = make_spec("a");
        spec.model = "nonexistent-model".into();

        let regs = build_registries(
            vec![],
            "other-model",
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "missing-provider".into(),
                model_name: "n".into(),
            },
            "other-provider",
            Arc::new(MockExecutor),
            vec![],
            make_spec("a"),
        );

        let err = resolve(&regs, "a").unwrap_err();
        assert!(matches!(err, ResolveError::ProviderNotFound(ref id) if id == "missing-provider"));
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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

        let regs = build_registries(
            vec![
                ("read", Arc::new(MockTool { id: "read".into() })),
                ("write", Arc::new(MockTool { id: "write".into() })),
            ],
            "test-model",
            ModelEntry {
                provider: "p".into(),
                model_name: "claude-test".into(),
            },
            "p",
            Arc::new(MockExecutor),
            vec![],
            make_spec("my-agent"),
        );

        let resolver = RegistrySetResolver::new(regs);
        let resolved = resolver.resolve("my-agent").unwrap();
        assert_eq!(resolved.id(), "my-agent");
        assert_eq!(resolved.model_id(), "test-model");
        assert_eq!(resolved.model, "claude-test");
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
            "p",
            Arc::new(MockExecutor),
            vec![],
            make_spec("existing"),
        );

        let resolver = RegistrySetResolver::new(regs);
        let err = resolver.resolve("missing").unwrap_err();
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

    impl Plugin for ValidatedPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: self.name }
        }

        fn config_schemas(&self) -> Vec<crate::plugins::ConfigSchema> {
            vec![crate::plugins::ConfigSchema {
                key: "validated",
                json_schema: serde_json::to_value(schemars::schema_for!(ValidatedConfig)).unwrap(),
            }]
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
        ) -> Result<(), awaken_contract::StateError> {
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
            "p",
            Arc::new(MockExecutor),
            vec![],
            spec,
        );

        let run = resolve(&regs, "a").unwrap();
        assert_eq!(run.system_prompt(), "Custom instructions for the agent.");
    }

    #[test]
    fn resolve_model_name_from_model_entry() {
        let regs = build_registries(
            vec![],
            "test-model",
            ModelEntry {
                provider: "p".into(),
                model_name: "claude-opus-4-20250514".into(),
            },
            "p",
            Arc::new(MockExecutor),
            vec![],
            make_spec("a"),
        );

        let run = resolve(&regs, "a").unwrap();
        assert_eq!(run.model, "claude-opus-4-20250514");
    }

    #[test]
    fn resolve_empty_allow_list_removes_all_tools() {
        let spec = AgentSpec {
            allowed_tools: Some(vec![]),
            ..make_spec("a")
        };

        let regs = build_registries(
            vec![
                ("read", Arc::new(MockTool { id: "read".into() })),
                ("write", Arc::new(MockTool { id: "write".into() })),
            ],
            "test-model",
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            .register_model(
                "test-model",
                ModelEntry {
                    provider: "p".into(),
                    model_name: "n".into(),
                },
            )
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
        Debug,
        Clone,
        Default,
        PartialEq,
        Eq,
        serde::Serialize,
        serde::Deserialize,
        schemars::JsonSchema,
    )]
    struct StatefulPluginConfig {
        pub level: String,
        pub max_items: u32,
    }

    struct StatefulPluginConfigKey;
    impl awaken_contract::PluginConfigKey for StatefulPluginConfigKey {
        const KEY: &'static str = "stateful";
        type Config = StatefulPluginConfig;
    }

    impl Plugin for StatefulPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: self.name }
        }

        fn config_schemas(&self) -> Vec<crate::plugins::ConfigSchema> {
            vec![crate::plugins::ConfigSchema {
                key: "stateful",
                json_schema: serde_json::to_value(schemars::schema_for!(StatefulPluginConfig))
                    .unwrap(),
            }]
        }

        fn on_activate(
            &self,
            agent_spec: &AgentSpec,
            _patch: &mut crate::state::MutationBatch,
        ) -> Result<(), awaken_contract::StateError> {
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
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
            ModelEntry {
                provider: "p".into(),
                model_name: "n".into(),
            },
            "p",
            Arc::new(MockExecutor),
            vec![("sp", Arc::new(StatefulPlugin { name: "sp" }))],
            spec,
        );
        assert!(resolve(&regs, "a").is_ok());
    }
}
