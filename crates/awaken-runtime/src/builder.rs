//! Fluent builder API for constructing `AgentRuntime`.

use std::sync::Arc;

use awaken_contract::StateError;
use awaken_contract::contract::commit_coordinator::CommitCoordinator;
use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::storage::ThreadRunStore;
use awaken_contract::contract::tool::Tool;
use awaken_contract::registry_spec::AgentSpec;

#[cfg(feature = "a2a")]
use crate::backend::ExecutionBackendFactory;
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
use crate::registry::snapshot::RegistryHandle;
use crate::registry::traits::{AgentSpecRegistry, ModelBinding, RegistrySet};
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
    /// ADR-0036 D8: persisted runs require an explicit `CommitCoordinator`.
    /// A `ThreadRunStore` without one would silently run with non-atomic
    /// checkpoint/event commits.
    #[error(
        "ADR-0036 D8: `with_thread_run_store` requires a paired `with_commit_coordinator` \
         supplied by the store/server integration"
    )]
    CommitCoordinatorRequired,
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
    commit_coordinator: Option<Arc<dyn CommitCoordinator>>,
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
            commit_coordinator: None,
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

    /// Register a model binding by ID.
    pub fn with_model_binding(mut self, id: impl Into<String>, binding: ModelBinding) -> Self {
        if let Err(e) = self.models.register_model(id, binding) {
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

    /// Register an explicit mock provider and model binding.
    pub fn with_mock_provider_profile(
        mut self,
        profile: crate::engine::MockProviderProfile,
    ) -> Self {
        let provider_id = profile.provider_id.clone();
        let model_id = profile.model_id.clone();
        if let Err(e) = self
            .providers
            .register_provider(provider_id, profile.executor())
        {
            self.errors.push(e);
        }
        if let Err(e) = self
            .models
            .register_model(model_id, profile.model_binding())
        {
            self.errors.push(e);
        }
        self
    }

    /// Set the thread run store for persistence.
    pub fn with_thread_run_store(mut self, store: Arc<dyn ThreadRunStore>) -> Self {
        self.thread_run_store = Some(store);
        self
    }

    /// ADR-0036 D8 test/development convenience: install an in-memory `ThreadRunStore` together
    /// with a `MemoryCommitCoordinator` that wraps the same handle, so the
    /// runtime tee and checkpoint writes share one transaction scope. Callers
    /// previously paired `with_thread_run_store(store.clone())` for tests; that
    /// shape silently produced a non-atomic mode and is now rejected at build
    /// time.
    #[cfg(feature = "test-utils")]
    pub fn with_in_memory_thread_run_store(
        mut self,
        store: Arc<awaken_stores::InMemoryStore>,
    ) -> Self {
        let coordinator = awaken_stores::MemoryCommitCoordinator::wrap(Arc::clone(&store));
        self.thread_run_store = Some(store as Arc<dyn ThreadRunStore>);
        self.commit_coordinator = Some(coordinator as Arc<dyn CommitCoordinator>);
        self
    }

    /// Wire a `CommitCoordinator` for atomic checkpoint commits across
    /// `ThreadRunStore` and `EventStore` writes (ADR-0036). When set, the
    /// runtime tees durable canonical drafts through the coordinator at
    /// checkpoint cadence instead of letting `ThreadRunStore::checkpoint`
    /// and `EventWriter::append` run in independent transactions.
    pub fn with_commit_coordinator(mut self, coordinator: Arc<dyn CommitCoordinator>) -> Self {
        self.commit_coordinator = Some(coordinator);
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
    pub fn with_agent_backend_factory(mut self, factory: Arc<dyn ExecutionBackendFactory>) -> Self {
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
        #[cfg(feature = "a2a")]
        let registries = runtime.registry_set();
        let mut errors = Vec::new();
        for agent_id in resolver.agent_ids() {
            #[cfg(feature = "a2a")]
            {
                if let Some(spec) = registries
                    .as_ref()
                    .and_then(|set| set.agents.get_agent(&agent_id))
                    && let Some(endpoint) = &spec.endpoint
                {
                    let Some(factory) = registries
                        .as_ref()
                        .and_then(|set| set.backends.get_backend_factory(&endpoint.backend))
                    else {
                        errors.push(format!(
                            "{agent_id}: unsupported remote backend '{}'",
                            endpoint.backend
                        ));
                        continue;
                    };
                    if let Err(error) = factory.validate(endpoint) {
                        errors.push(format!("{agent_id}: {error}"));
                    }
                    continue;
                }
            }

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
        // ADR-0036 D8: a thread-run store without a paired coordinator is
        // forbidden — that combination is the silently non-atomic mode the
        // ADR rejects in §117-124.
        if self.thread_run_store.is_some() && self.commit_coordinator.is_none() {
            return Err(BuildError::CommitCoordinatorRequired);
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

        let registry_handle = RegistryHandle::new(registry_set.clone());
        let resolver_impl = Arc::new(crate::registry::resolve::DynamicRegistryResolver::new(
            registry_handle.clone(),
        ));
        let resolver: Arc<dyn crate::registry::AgentResolver> = resolver_impl.clone();
        let run_resolver: Arc<dyn crate::resolution::Resolver> = resolver_impl;

        let mut runtime = AgentRuntime::new_with_execution_resolver(resolver)
            .with_run_resolver(run_resolver)
            .with_registry_handle(registry_handle);

        #[cfg(feature = "a2a")]
        if let Some(composite) = composite_registry {
            runtime = runtime.with_composite_registry(composite);
        }

        if let Some(store) = self.thread_run_store {
            runtime = runtime.with_thread_run_store(store);
        }

        if let Some(coordinator) = self.commit_coordinator {
            runtime = runtime.with_commit_coordinator(coordinator);
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
#[path = "builder_tests.rs"]
mod tests;
