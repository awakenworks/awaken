//! Resolution: `agent_id` + `RegistrySet` → `ResolvedRun`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::contract::executor::LlmExecutor;
use crate::contract::tool::Tool;
use crate::plugins::Plugin;
use crate::runtime::ExecutionEnv;

use super::spec::AgentSpec;
use super::traits::RegistrySet;

// ---------------------------------------------------------------------------
// ResolvedRun
// ---------------------------------------------------------------------------

/// Fully resolved agent run — holds live references, not serializable.
///
/// Produced by [`resolve`] from a [`RegistrySet`] and an agent ID.
/// Passed to the loop runner as the single runtime configuration.
impl std::fmt::Debug for ResolvedRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedRun")
            .field("spec", &self.spec)
            .field("model_name", &self.model_name)
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .field("plugins_count", &self.plugins.len())
            .finish_non_exhaustive()
    }
}

pub struct ResolvedRun {
    /// The source agent definition.
    pub spec: AgentSpec,
    /// Resolved LLM executor.
    pub executor: Arc<dyn LlmExecutor>,
    /// Actual model name for API calls.
    pub model_name: String,
    /// Resolved tools (after allow/exclude filtering).
    pub tools: HashMap<String, Arc<dyn Tool>>,
    /// Resolved plugins.
    pub plugins: Vec<Arc<dyn Plugin>>,
    /// Execution environment built from resolved plugins.
    pub env: ExecutionEnv,
}

// ---------------------------------------------------------------------------
// ResolveError
// ---------------------------------------------------------------------------

/// Errors from the resolution process.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("agent not found: {0}")]
    AgentNotFound(String),
    #[error("model not found: {0}")]
    ModelNotFound(String),
    #[error("provider not found: {0}")]
    ProviderNotFound(String),
    #[error("plugin not found: {0}")]
    PluginNotFound(String),
    #[error("env build error: {0}")]
    EnvBuild(#[from] crate::error::StateError),
}

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
pub fn resolve(registries: &RegistrySet, agent_id: &str) -> Result<ResolvedRun, ResolveError> {
    let spec = registries
        .agents
        .get_agent(agent_id)
        .ok_or_else(|| ResolveError::AgentNotFound(agent_id.into()))?
        .clone();

    let model = registries
        .models
        .get_model(&spec.model)
        .ok_or_else(|| ResolveError::ModelNotFound(spec.model.clone()))?;

    let executor = registries
        .providers
        .get_provider(&model.provider)
        .ok_or_else(|| ResolveError::ProviderNotFound(model.provider.clone()))?;

    let tools = resolve_tools(registries, &spec);
    let plugins = resolve_plugins(registries, &spec)?;
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
// Internal helpers
// ---------------------------------------------------------------------------

/// Resolve tools with allow/exclude filtering.
///
/// - `allowed_tools = None` → all tools from `ToolRegistry`.
/// - `allowed_tools = Some(list)` → only those IDs.
/// - `excluded_tools` → removed from the included set.
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
        if !excluded.contains(id) {
            if let Some(tool) = registries.tools.get_tool(id) {
                tools.insert((*id).to_string(), tool);
            }
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
    use crate::contract::executor::{InferenceExecutionError, InferenceRequest};
    use crate::contract::inference::{StopReason, StreamResult, TokenUsage};
    use crate::contract::tool::{ToolDescriptor, ToolError, ToolResult};
    use crate::plugins::PluginDescriptor;
    use crate::registry::memory::{
        MapAgentSpecRegistry, MapModelRegistry, MapPluginSource, MapProviderRegistry,
        MapToolRegistry,
    };
    use crate::registry::traits::ModelEntry;
    use async_trait::async_trait;
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

        async fn execute(&self, _args: Value) -> Result<ToolResult, ToolError> {
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
                text: String::new(),
                tool_calls: vec![],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(StopReason::EndTurn),
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
            max_rounds: 16,
            plugin_ids: vec![],
            allowed_tools: None,
            excluded_tools: None,
            sections: Default::default(),
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
        assert_eq!(run.plugins.len(), 1);
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
        assert!(run.plugins.is_empty());
        // env is empty — no hooks registered
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
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_string(), expected);
        }
    }
}
