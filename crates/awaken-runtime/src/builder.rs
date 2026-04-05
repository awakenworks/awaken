//! Fluent builder API for constructing `AgentRuntime`.

use std::sync::Arc;

use awaken_contract::StateError;
use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::storage::ThreadRunStore;
use awaken_contract::contract::tool::Tool;
use awaken_contract::registry_spec::AgentSpec;

use crate::plugins::Plugin;
#[cfg(feature = "a2a")]
use crate::registry::BackendRegistry;
#[cfg(feature = "a2a")]
use crate::registry::composite::{CompositeAgentSpecRegistry, RemoteAgentSource};
#[cfg(feature = "a2a")]
use crate::registry::memory::MapBackendRegistry;
use crate::registry::memory::{
    MapAgentSpecRegistry, MapModelRegistry, MapPluginSource, MapProviderRegistry, MapToolRegistry,
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
    #[error("tool registry conflict: {0}")]
    ToolRegistryConflict(String),
    #[error("model registry conflict: {0}")]
    ModelRegistryConflict(String),
    #[error("provider registry conflict: {0}")]
    ProviderRegistryConflict(String),
    #[error("plugin registry conflict: {0}")]
    PluginRegistryConflict(String),
    #[cfg(feature = "a2a")]
    #[error("backend registry conflict: {0}")]
    BackendRegistryConflict(String),
    #[error("agent validation failed: {0}")]
    ValidationFailed(String),
    #[cfg(feature = "a2a")]
    #[error("discovery failed: {0}")]
    DiscoveryFailed(#[from] crate::registry::composite::DiscoveryError),
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
    #[cfg(feature = "a2a")]
    backends: MapBackendRegistry,
    thread_run_store: Option<Arc<dyn ThreadRunStore>>,
    profile_store: Option<Arc<dyn awaken_contract::contract::profile_store::ProfileStore>>,
    errors: Vec<BuildError>,
    #[cfg(feature = "a2a")]
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
            #[cfg(feature = "a2a")]
            backends: MapBackendRegistry::with_default_remote_backends(),
            thread_run_store: None,
            profile_store: None,
            errors: Vec::new(),
            #[cfg(feature = "a2a")]
            remote_sources: Vec::new(),
        }
    }

    /// Register an agent spec.
    pub fn with_agent_spec(mut self, spec: AgentSpec) -> Self {
        if let Err(e) = self.agents.register_spec(spec) {
            self.errors.push(e);
        }
        self
    }

    /// Register multiple agent specs.
    pub fn with_agent_specs(mut self, specs: impl IntoIterator<Item = AgentSpec>) -> Self {
        for spec in specs {
            if let Err(e) = self.agents.register_spec(spec) {
                self.errors.push(e);
            }
        }
        self
    }

    /// Register a tool by ID.
    pub fn with_tool(mut self, id: impl Into<String>, tool: Arc<dyn Tool>) -> Self {
        if let Err(e) = self.tools.register_tool(id, tool) {
            self.errors.push(e);
        }
        self
    }

    /// Register a plugin by ID.
    pub fn with_plugin(mut self, id: impl Into<String>, plugin: Arc<dyn Plugin>) -> Self {
        if let Err(e) = self.plugins.register_plugin(id, plugin) {
            self.errors.push(e);
        }
        self
    }

    /// Register a model entry by ID.
    pub fn with_model(mut self, id: impl Into<String>, entry: ModelEntry) -> Self {
        if let Err(e) = self.models.register_model(id, entry) {
            self.errors.push(e);
        }
        self
    }

    /// Register a provider (LLM executor) by ID.
    pub fn with_provider(mut self, id: impl Into<String>, executor: Arc<dyn LlmExecutor>) -> Self {
        if let Err(e) = self.providers.register_provider(id, executor) {
            self.errors.push(e);
        }
        self
    }

    /// Set the thread run store for persistence.
    pub fn with_thread_run_store(mut self, store: Arc<dyn ThreadRunStore>) -> Self {
        self.thread_run_store = Some(store);
        self
    }

    /// Set the profile store for cross-run key-value persistence.
    pub fn with_profile_store(
        mut self,
        store: Arc<dyn awaken_contract::contract::profile_store::ProfileStore>,
    ) -> Self {
        self.profile_store = Some(store);
        self
    }

    /// Add a named remote A2A agent source for discovery.
    ///
    /// When remote sources are configured, the builder creates a
    /// [`CompositeAgentSpecRegistry`] that combines local agents with
    /// agents discovered from remote A2A endpoints. The `name` is used
    /// for namespaced agent lookup (e.g., `"cloud/translator"`).
    #[cfg(feature = "a2a")]
    pub fn with_remote_agents(
        mut self,
        name: impl Into<String>,
        base_url: impl Into<String>,
        bearer_token: Option<String>,
    ) -> Self {
        self.remote_sources.push(RemoteAgentSource {
            name: name.into(),
            base_url: base_url.into(),
            bearer_token,
        });
        self
    }

    /// Register a remote delegate backend factory by its backend kind.
    #[cfg(feature = "a2a")]
    pub fn with_agent_backend_factory(
        mut self,
        factory: Arc<dyn crate::extensions::a2a::AgentBackendFactory>,
    ) -> Self {
        if let Err(e) = self.backends.register_backend_factory(factory) {
            self.errors.push(e);
        }
        self
    }

    /// Build the `AgentRuntime` and validate all registered agents can
    /// resolve successfully.
    ///
    /// Performs a dry-run resolve for every registered agent, catching
    /// configuration errors (missing models, providers, plugins) at build time.
    /// Use [`build_unchecked()`](Self::build_unchecked) to skip validation.
    pub fn build(self) -> Result<AgentRuntime, BuildError> {
        let runtime = self.build_unchecked()?;
        let resolver = runtime.resolver();
        let mut errors = Vec::new();
        for agent_id in resolver.agent_ids() {
            if let Err(e) = resolver.resolve(&agent_id) {
                errors.push(format!("{agent_id}: {e}"));
            }
        }
        if !errors.is_empty() {
            return Err(BuildError::ValidationFailed(errors.join("; ")));
        }
        Ok(runtime)
    }

    /// Build the `AgentRuntime` from the accumulated configuration,
    /// skipping agent validation.
    ///
    /// Prefer [`build()`](Self::build) which validates all registered agents
    /// can resolve successfully at build time.
    pub fn build_unchecked(mut self) -> Result<AgentRuntime, BuildError> {
        if !self.errors.is_empty() {
            return Err(self.errors.remove(0));
        }

        #[cfg(feature = "a2a")]
        let (agents, composite_registry): (Arc<dyn AgentSpecRegistry>, _) =
            if self.remote_sources.is_empty() {
                (Arc::new(self.agents), None)
            } else {
                let mut composite = CompositeAgentSpecRegistry::new(Arc::new(self.agents));
                for source in self.remote_sources {
                    composite.add_remote(source);
                }
                let arc = Arc::new(composite);
                (Arc::clone(&arc) as Arc<dyn AgentSpecRegistry>, Some(arc))
            };
        #[cfg(not(feature = "a2a"))]
        let agents: Arc<dyn AgentSpecRegistry> = Arc::new(self.agents);

        let registry_set = RegistrySet {
            agents,
            tools: Arc::new(self.tools),
            models: Arc::new(self.models),
            providers: Arc::new(self.providers),
            plugins: Arc::new(self.plugins),
            #[cfg(feature = "a2a")]
            backends: Arc::new(self.backends) as Arc<dyn BackendRegistry>,
        };

        let resolver: Arc<dyn crate::registry::AgentResolver> = Arc::new(
            crate::registry::resolve::RegistrySetResolver::new(registry_set),
        );

        let mut runtime = AgentRuntime::new(resolver);

        #[cfg(feature = "a2a")]
        if let Some(composite) = composite_registry {
            runtime = runtime.with_composite_registry(composite);
        }

        if let Some(store) = self.thread_run_store {
            runtime = runtime.with_thread_run_store(store);
        }

        if let Some(store) = self.profile_store {
            runtime = runtime.with_profile_store(store);
        }

        Ok(runtime)
    }

    /// Build and initialize (async). Discovers remote agents after build.
    #[cfg(feature = "a2a")]
    pub async fn build_and_discover(self) -> Result<AgentRuntime, BuildError> {
        let runtime = self.build_unchecked()?;
        if let Some(composite) = runtime.composite_registry() {
            composite.discover().await?;
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
    use awaken_contract::contract::tool::{
        ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
    };
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
        ) -> Result<ToolOutput, ToolError> {
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
        assert_eq!(resolved.id(), "my-agent");
        assert_eq!(resolved.model, "claude-test");
        assert_eq!(resolved.system_prompt(), "Be helpful.");
        assert_eq!(resolved.max_rounds(), 10);
        assert!(resolved.tools.contains_key("search"));
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
        assert!(resolved.tools.contains_key("t1"));
        assert!(resolved.tools.contains_key("t2"));
        assert!(resolved.tools.contains_key("t3"));
    }

    #[test]
    fn build_catches_missing_model() {
        let spec = AgentSpec {
            id: "bad-agent".into(),
            model: "nonexistent-model".into(),
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
            model: "m".into(),
            system_prompt: "sys".into(),
            ..Default::default()
        };

        let result = AgentRuntimeBuilder::new()
            .with_agent_spec(spec)
            .with_model(
                "m",
                ModelEntry {
                    provider: "p".into(),
                    model_name: "n".into(),
                },
            )
            .with_provider("p", Arc::new(MockExecutor))
            .build();

        assert!(result.is_ok());
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
        assert_eq!(resolved.model, "gpt-4-turbo");
    }

    #[test]
    fn builder_with_profile_store() {
        use awaken_contract::contract::profile_store::{
            ProfileEntry, ProfileOwner as POwner, ProfileStore,
        };
        use awaken_contract::contract::storage::StorageError;

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

    #[test]
    fn duplicate_agent_spec_errors_at_build() {
        let spec = AgentSpec {
            id: "dup-agent".into(),
            model: "m".into(),
            system_prompt: "sys".into(),
            ..Default::default()
        };

        let result = AgentRuntimeBuilder::new()
            .with_agent_spec(spec.clone())
            .with_agent_spec(spec)
            .with_model(
                "m",
                ModelEntry {
                    provider: "p".into(),
                    model_name: "n".into(),
                },
            )
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
            .with_model(
                "dup-model",
                ModelEntry {
                    provider: "p".into(),
                    model_name: "n1".into(),
                },
            )
            .with_model(
                "dup-model",
                ModelEntry {
                    provider: "p".into(),
                    model_name: "n2".into(),
                },
            )
            .build();

        match result {
            Err(e) => {
                let err = e.to_string();
                assert!(
                    err.contains("dup-model"),
                    "error should mention the duplicate model ID: {err}"
                );
            }
            Ok(_) => panic!("expected build to fail for duplicate model"),
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
}
