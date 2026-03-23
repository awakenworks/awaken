//! Resolution pipeline: `agent_id` + `RegistrySet` -> `ResolvedRun`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::agent::config::AgentConfig;
use crate::error::RuntimeError;
use crate::execution::SequentialToolExecutor;
use crate::phase::ExecutionEnv;
use crate::plugins::Plugin;
use crate::registry::{AgentResolver, ResolvedAgent};
use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::tool::Tool;

use crate::registry::traits::RegistrySet;
use awaken_contract::registry_spec::AgentSpec;

use super::ResolvedRun;
use super::error::ResolveError;

// ---------------------------------------------------------------------------
// resolve()
// ---------------------------------------------------------------------------

/// Resolve an agent by ID from registries into a fully wired [`ResolvedRun`].
///
/// 1. Look up `AgentSpec` from `AgentSpecRegistry`.
/// 2. Look up `ModelEntry` from `ModelRegistry`.
/// 3. Look up `LlmExecutor` from `ProviderRegistry` via `ModelEntry.provider`.
/// 4. Resolve tools with allow/exclude filtering.
/// 5. Resolve plugins by ID.
/// 6. Build `ExecutionEnv` from plugins.
fn resolve(registries: &RegistrySet, agent_id: &str) -> Result<ResolvedRun, ResolveError> {
    let spec = registries
        .agents
        .get_agent(agent_id)
        .ok_or_else(|| ResolveError::AgentNotFound(agent_id.into()))?;

    // Remote agents run via A2A protocol — they don't need local model/provider
    // resolution. They should be used as delegates, not resolved directly.
    if spec.endpoint.is_some() {
        return Err(ResolveError::RemoteAgentNotDirectlyRunnable(
            spec.id.clone(),
        ));
    }

    let model = registries
        .models
        .get_model(&spec.model)
        .ok_or_else(|| ResolveError::ModelNotFound(spec.model.clone()))?;

    let executor = registries
        .providers
        .get_provider(&model.provider)
        .ok_or_else(|| ResolveError::ProviderNotFound(model.provider.clone()))?;

    // Wrap executor with retry policy if configured in agent spec sections.
    let executor = match spec.config::<crate::engine::RetryConfigKey>() {
        Ok(policy) if policy.max_retries > 0 || !policy.fallback_models.is_empty() => {
            Arc::new(crate::engine::RetryingExecutor::new(executor, policy)) as Arc<dyn LlmExecutor>
        }
        _ => executor,
    };

    let mut tools = resolve_tools(registries, &spec);

    // Wire delegate agent tools from spec.delegates (IDs referencing other agents)
    if !spec.delegates.is_empty() {
        for delegate_id in &spec.delegates {
            let delegate_spec = registries
                .agents
                .get_agent(delegate_id)
                .ok_or_else(|| ResolveError::AgentNotFound(delegate_id.clone()))?;

            let description: String = delegate_spec.system_prompt.chars().take(100).collect();

            let tool: Arc<dyn Tool> = if let Some(endpoint) = &delegate_spec.endpoint {
                let mut config = crate::extensions::a2a::A2aConfig::new(&endpoint.base_url);
                if let Some(token) = &endpoint.bearer_token {
                    config = config.with_bearer_token(token);
                }
                config = config
                    .with_poll_interval(std::time::Duration::from_millis(endpoint.poll_interval_ms))
                    .with_timeout(std::time::Duration::from_millis(endpoint.timeout_ms));
                Arc::new(crate::extensions::a2a::AgentTool::remote(
                    delegate_id,
                    &description,
                    config,
                ))
            } else {
                let resolver: Arc<dyn crate::registry::AgentResolver> =
                    Arc::new(registries.clone());
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

    let mut plugins = resolve_plugins(registries, &spec)?;
    plugins.push(Arc::new(
        crate::loop_runner::actions::LoopActionHandlersPlugin,
    ));
    plugins.push(Arc::new(crate::policies::MaxRoundsPlugin::new(
        spec.max_rounds,
    )));
    plugins.push(Arc::new(crate::plugins::AllowAllToolsPlugin));

    // Default compaction plugin: tracks compaction boundaries in state.
    // Only added if the agent has a context_policy configured.
    if spec.context_policy.is_some() {
        let compaction_config = spec
            .config::<crate::context::CompactionConfigKey>()
            .unwrap_or_default();
        plugins.push(Arc::new(crate::context::CompactionPlugin::new(
            compaction_config,
        )));
    }

    // Eager validation: check spec sections against plugin-declared schemas
    validate_sections(&spec, &plugins)?;

    let env = ExecutionEnv::from_plugins(&plugins)?;

    Ok(ResolvedRun {
        spec,
        executor,
        model_name: model.model_name.clone(),
        tools,
        plugins,
        env,
    })
}

// ---------------------------------------------------------------------------
// AgentResolver implementation
// ---------------------------------------------------------------------------

impl AgentResolver for RegistrySet {
    /// Resolve an agent by ID into a `ResolvedAgent` (config + env).
    ///
    /// Bridges the registry resolution (`ResolvedRun`) into the runtime's
    /// `AgentConfig` + `ExecutionEnv` pair that the loop runner expects.
    fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        let run = resolve(self, agent_id).map_err(|e| RuntimeError::ResolveFailed {
            message: e.to_string(),
        })?;

        let mut env = run.env;

        let config = AgentConfig {
            id: run.spec.id,
            model_id: run.spec.model,
            model: run.model_name,
            system_prompt: run.spec.system_prompt,
            max_rounds: run.spec.max_rounds,
            tools: run.tools,
            llm_executor: run.executor,
            tool_executor: Arc::new(SequentialToolExecutor),
            context_policy: run.spec.context_policy,
            context_summarizer: None,
            max_continuation_retries: run.spec.max_continuation_retries,
        };

        // Register built-in context truncation transform when policy is set
        if let Some(ref policy) = config.context_policy {
            env.request_transforms
                .push(Arc::new(crate::context::ContextTransform::new(
                    policy.clone(),
                )));
        }

        Ok(ResolvedAgent { config, env })
    }

    fn agent_ids(&self) -> Vec<String> {
        self.agents.agent_ids()
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

/// Resolve tools with allow/exclude filtering.
///
/// - `allowed_tools = None` -> all tools from `ToolRegistry`.
/// - `allowed_tools = Some(list)` -> only those IDs.
/// - `excluded_tools` -> removed from the included set.
fn resolve_tools(registries: &RegistrySet, spec: &AgentSpec) -> HashMap<String, Arc<dyn Tool>> {
    let all_ids = registries.tools.tool_ids();

    let included: HashSet<&str> = match &spec.allowed_tools {
        Some(allow) => allow.iter().map(|s| s.as_str()).collect(),
        None => all_ids.iter().map(|s| s.as_str()).collect(),
    };

    let excluded: HashSet<&str> = spec
        .excluded_tools
        .as_ref()
        .map(|v| v.iter().map(|s| s.as_str()).collect())
        .unwrap_or_default();

    let mut tools = HashMap::new();
    for id in &included {
        if !excluded.contains(id)
            && let Some(tool) = registries.tools.get_tool(id)
        {
            tools.insert((*id).to_string(), tool);
        }
    }
    tools
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
    use crate::plugins::PluginDescriptor;
    use crate::registry::memory::{
        MapAgentSpecRegistry, MapModelRegistry, MapPluginSource, MapProviderRegistry,
        MapToolRegistry,
    };
    use crate::registry::traits::ModelEntry;
    use async_trait::async_trait;
    use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
    use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    use awaken_contract::contract::tool::{ToolCallContext, ToolDescriptor, ToolError, ToolResult};
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
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success(&self.id, Value::Null))
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
            tool_reg.register(id, tool);
        }

        let mut model_reg = MapModelRegistry::new();
        model_reg.register(model_id, model_entry);

        let mut provider_reg = MapProviderRegistry::new();
        provider_reg.register(provider_id, executor);

        let mut plugin_reg = MapPluginSource::new();
        for (id, plugin) in plugins {
            plugin_reg.register(id, plugin);
        }

        let mut agent_reg = MapAgentSpecRegistry::new();
        agent_reg.register(spec);

        RegistrySet {
            agents: Arc::new(agent_reg),
            tools: Arc::new(tool_reg),
            models: Arc::new(model_reg),
            providers: Arc::new(provider_reg),
            plugins: Arc::new(plugin_reg),
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
        assert_eq!(run.spec.id, "agent-1");
        assert_eq!(run.model_name, "claude-opus-4-20250514");
        assert_eq!(run.tools.len(), 2);
        assert!(run.tools.contains_key("read"));
        assert!(run.tools.contains_key("write"));
        assert_eq!(run.plugins.len(), 4); // user plugin + LoopActionHandlersPlugin + MaxRoundsPlugin + AllowAllToolsPlugin
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
        assert_eq!(run.plugins.len(), 3); // LoopActionHandlersPlugin + MaxRoundsPlugin + AllowAllToolsPlugin
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
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_string(), expected);
        }
    }

    // -- AgentResolver bridge tests --

    #[test]
    fn registry_set_as_agent_resolver() {
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

        let resolved = AgentResolver::resolve(&regs, "my-agent").unwrap();
        assert_eq!(resolved.config.id, "my-agent");
        assert_eq!(resolved.config.model_id, "test-model");
        assert_eq!(resolved.config.model, "claude-test");
        assert_eq!(resolved.config.system_prompt, "You are helpful.");
        assert_eq!(resolved.config.tools.len(), 2);
        assert!(resolved.config.tools.contains_key("read"));
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

        let err = AgentResolver::resolve(&regs, "missing").unwrap_err();
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
}
