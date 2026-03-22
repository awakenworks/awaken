//! Fluent builder API for constructing `AgentRuntime`.

use std::sync::Arc;

use awaken_contract::StateError;
use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::storage::ThreadRunStore;
use awaken_contract::contract::tool::Tool;
use awaken_contract::registry_spec::AgentSpec;

use crate::state::StateStore;

use crate::agent::stop_conditions::StopPolicy;
use crate::plugins::Plugin;
use crate::registry::composite::{CompositeAgentSpecRegistry, RemoteAgentSource};
use crate::registry::memory::{
    MapAgentRegistry, MapAgentSpecRegistry, MapModelRegistry, MapPluginSource, MapProviderRegistry,
    MapStopPolicyRegistry, MapToolRegistry,
};
use crate::registry::traits::{AgentSpecRegistry, ModelEntry, RegistrySet};
use crate::runtime::AgentRuntime;

/// Error returned when the builder cannot construct the runtime.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("state error: {0}")]
    State(#[from] StateError),
    #[error("agent registry conflict: {0}")]
    AgentRegistryConflict(String),
    #[error("stop policy registry conflict: {0}")]
    StopPolicyConflict(String),
}

/// Fluent API for constructing an `AgentRuntime`.
///
/// Collects agent specs, tools, plugins, models, providers, and optionally
/// a store, then builds the fully resolved runtime.
pub struct AgentRuntimeBuilder {
    agents: MapAgentSpecRegistry,
    tools: MapToolRegistry,
    models: MapModelRegistry,
    providers: MapProviderRegistry,
    plugins: MapPluginSource,
    store: Option<StateStore>,
    thread_run_store: Option<Arc<dyn ThreadRunStore>>,
    agent_registries: MapAgentRegistry,
    stop_policies: MapStopPolicyRegistry,
    remote_sources: Vec<RemoteAgentSource>,
}

impl AgentRuntimeBuilder {
    pub fn new() -> Self {
        Self {
            agents: MapAgentSpecRegistry::new(),
            tools: MapToolRegistry::new(),
            models: MapModelRegistry::new(),
            providers: MapProviderRegistry::new(),
            plugins: MapPluginSource::new(),
            store: None,
            thread_run_store: None,
            agent_registries: MapAgentRegistry::new(),
            stop_policies: MapStopPolicyRegistry::new(),
            remote_sources: Vec::new(),
        }
    }

    /// Register an agent spec.
    pub fn with_agent_spec(mut self, spec: AgentSpec) -> Self {
        self.agents.register(spec);
        self
    }

    /// Register multiple agent specs.
    pub fn with_agent_specs(mut self, specs: impl IntoIterator<Item = AgentSpec>) -> Self {
        for spec in specs {
            self.agents.register(spec);
        }
        self
    }

    /// Register a tool by ID.
    pub fn with_tool(mut self, id: impl Into<String>, tool: Arc<dyn Tool>) -> Self {
        self.tools.register(id, tool);
        self
    }

    /// Register a plugin by ID.
    pub fn with_plugin(mut self, id: impl Into<String>, plugin: Arc<dyn Plugin>) -> Self {
        self.plugins.register(id, plugin);
        self
    }

    /// Register a model entry by ID.
    pub fn with_model(mut self, id: impl Into<String>, entry: ModelEntry) -> Self {
        self.models.register(id, entry);
        self
    }

    /// Register a provider (LLM executor) by ID.
    pub fn with_provider(mut self, id: impl Into<String>, executor: Arc<dyn LlmExecutor>) -> Self {
        self.providers.register(id, executor);
        self
    }

    /// Set the state store.
    pub fn with_store(mut self, store: StateStore) -> Self {
        self.store = Some(store);
        self
    }

    /// Set the thread run store for persistence.
    pub fn with_thread_run_store(mut self, store: Arc<dyn ThreadRunStore>) -> Self {
        self.thread_run_store = Some(store);
        self
    }

    /// Register an agent in the agent registry (for sub-agent lookup).
    pub fn with_agent_registry_entry(mut self, id: impl Into<String>, spec: AgentSpec) -> Self {
        self.agent_registries.register(id, spec);
        self
    }

    /// Register a stop policy.
    pub fn with_stop_policy(mut self, id: impl Into<String>, policy: Arc<dyn StopPolicy>) -> Self {
        self.stop_policies.register(id, policy);
        self
    }

    /// Add a remote A2A agent source for discovery.
    ///
    /// When remote sources are configured, the builder creates a
    /// [`CompositeAgentSpecRegistry`] that combines local agents with
    /// agents discovered from remote A2A endpoints.
    pub fn with_remote_agents(
        mut self,
        base_url: impl Into<String>,
        bearer_token: Option<String>,
    ) -> Self {
        self.remote_sources.push(RemoteAgentSource {
            base_url: base_url.into(),
            bearer_token,
        });
        self
    }

    /// Build the `AgentRuntime` from the accumulated configuration.
    pub fn build(self) -> Result<AgentRuntime, BuildError> {
        let agents: Arc<dyn AgentSpecRegistry> = if self.remote_sources.is_empty() {
            Arc::new(self.agents)
        } else {
            let mut composite = CompositeAgentSpecRegistry::new(Arc::new(self.agents));
            for source in self.remote_sources {
                composite.add_remote(source);
            }
            Arc::new(composite)
        };

        let registry_set = RegistrySet {
            agents,
            tools: Arc::new(self.tools),
            models: Arc::new(self.models),
            providers: Arc::new(self.providers),
            plugins: Arc::new(self.plugins),
        };

        let resolver: Arc<dyn crate::runtime::AgentResolver> = Arc::new(registry_set);

        let mut runtime = AgentRuntime::new(resolver);

        if let Some(store) = self.thread_run_store {
            runtime = runtime.with_thread_run_store(store);
        }

        Ok(runtime)
    }
}

impl Default for AgentRuntimeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
    use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    use awaken_contract::contract::tool::{ToolCallContext, ToolDescriptor, ToolError, ToolResult};
    use serde_json::Value;

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

    #[test]
    fn builder_creates_runtime() {
        let spec = AgentSpec {
            id: "test-agent".into(),
            model: "test-model".into(),
            system_prompt: "You are helpful.".into(),
            ..Default::default()
        };

        let runtime = AgentRuntimeBuilder::new()
            .with_agent_spec(spec)
            .with_tool("echo", Arc::new(MockTool { id: "echo".into() }))
            .with_model(
                "test-model",
                ModelEntry {
                    provider: "mock".into(),
                    model_name: "mock-model".into(),
                },
            )
            .with_provider("mock", Arc::new(MockExecutor))
            .build();

        assert!(runtime.is_ok());
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
            model: "m".into(),
            system_prompt: "sys".into(),
            ..Default::default()
        };
        let spec2 = AgentSpec {
            id: "agent-2".into(),
            model: "m".into(),
            system_prompt: "sys".into(),
            ..Default::default()
        };

        let runtime = AgentRuntimeBuilder::new()
            .with_agent_specs(vec![spec1, spec2])
            .with_model(
                "m",
                ModelEntry {
                    provider: "p".into(),
                    model_name: "n".into(),
                },
            )
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
            model: "test-model".into(),
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
            .with_model(
                "test-model",
                ModelEntry {
                    provider: "mock".into(),
                    model_name: "claude-test".into(),
                },
            )
            .with_provider("mock", Arc::new(MockExecutor))
            .build()
            .unwrap();

        let resolved = runtime.resolver().resolve("my-agent").unwrap();
        assert_eq!(resolved.config.id, "my-agent");
        assert_eq!(resolved.config.model, "claude-test");
        assert_eq!(resolved.config.system_prompt, "Be helpful.");
        assert_eq!(resolved.config.max_rounds, 10);
        assert!(resolved.config.tools.contains_key("search"));
    }

    #[test]
    fn builder_missing_agent_errors() {
        let runtime = AgentRuntimeBuilder::new()
            .with_model(
                "m",
                ModelEntry {
                    provider: "p".into(),
                    model_name: "n".into(),
                },
            )
            .with_provider("p", Arc::new(MockExecutor))
            .build()
            .unwrap();

        let err = runtime.resolver().resolve("nonexistent");
        assert!(err.is_err());
    }

    // -----------------------------------------------------------------------
    // Migrated from uncarve: additional builder tests
    // -----------------------------------------------------------------------

    #[test]
    fn builder_with_store() {
        let store = StateStore::new();
        let runtime = AgentRuntimeBuilder::new()
            .with_store(store)
            .build()
            .unwrap();
        // Should work without error
        let _ = runtime;
    }

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
            ) -> Result<(), awaken_contract::StateError> {
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
    fn builder_with_stop_policy() {
        use crate::agent::stop_conditions::MaxRoundsPolicy;

        let runtime = AgentRuntimeBuilder::new()
            .with_stop_policy("max_rounds", Arc::new(MaxRoundsPolicy::new(10)))
            .build()
            .unwrap();
        let _ = runtime;
    }

    #[test]
    fn builder_with_agent_registry_entry() {
        let spec = AgentSpec {
            id: "worker".into(),
            model: "m".into(),
            system_prompt: "sys".into(),
            ..Default::default()
        };
        let runtime = AgentRuntimeBuilder::new()
            .with_agent_registry_entry("worker", spec)
            .build()
            .unwrap();
        let _ = runtime;
    }

    #[test]
    fn builder_chained_tools_all_registered() {
        let spec = AgentSpec {
            id: "agent".into(),
            model: "m".into(),
            system_prompt: "sys".into(),
            ..Default::default()
        };

        let runtime = AgentRuntimeBuilder::new()
            .with_agent_spec(spec)
            .with_tool("t1", Arc::new(MockTool { id: "t1".into() }))
            .with_tool("t2", Arc::new(MockTool { id: "t2".into() }))
            .with_tool("t3", Arc::new(MockTool { id: "t3".into() }))
            .with_model(
                "m",
                ModelEntry {
                    provider: "p".into(),
                    model_name: "n".into(),
                },
            )
            .with_provider("p", Arc::new(MockExecutor))
            .build()
            .unwrap();

        let resolved = runtime.resolver().resolve("agent").unwrap();
        assert!(resolved.config.tools.contains_key("t1"));
        assert!(resolved.config.tools.contains_key("t2"));
        assert!(resolved.config.tools.contains_key("t3"));
    }

    #[test]
    fn builder_error_display() {
        let err = BuildError::AgentRegistryConflict("duplicate agent".into());
        assert!(err.to_string().contains("duplicate agent"));

        let err = BuildError::StopPolicyConflict("duplicate policy".into());
        assert!(err.to_string().contains("duplicate policy"));
    }

    #[test]
    fn builder_model_entry_provider_name() {
        let spec = AgentSpec {
            id: "agent".into(),
            model: "gpt-4".into(),
            system_prompt: "sys".into(),
            ..Default::default()
        };

        let runtime = AgentRuntimeBuilder::new()
            .with_agent_spec(spec)
            .with_model(
                "gpt-4",
                ModelEntry {
                    provider: "openai".into(),
                    model_name: "gpt-4-turbo".into(),
                },
            )
            .with_provider("openai", Arc::new(MockExecutor))
            .build()
            .unwrap();

        let resolved = runtime.resolver().resolve("agent").unwrap();
        // The model should be resolved to the actual model name
        assert_eq!(resolved.config.model, "gpt-4-turbo");
    }
}
