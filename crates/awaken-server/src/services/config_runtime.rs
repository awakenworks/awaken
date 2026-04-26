use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use awaken_contract::contract::config_store::{ConfigChangeNotifier, ConfigStore};
use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::storage::StorageError;
use awaken_contract::{
    AgentSpec, McpRestartPolicy, McpServerSpec, McpTransportKind, ModelBindingSpec,
    PeriodicRefresher, ProviderSpec,
};
use awaken_ext_mcp::{McpServerConnectionConfig, McpToolRegistry, McpToolRegistryManager};
use awaken_runtime::engine::GenaiExecutor;
use awaken_runtime::registry::BackendRegistry;
use awaken_runtime::registry::memory::{
    MapAgentSpecRegistry, MapModelRegistry, MapProviderRegistry,
};
use awaken_runtime::registry::resolve::RegistrySetResolver;
use awaken_runtime::registry::{
    AgentSpecRegistry, ModelBinding, PluginSource, RegistrySet, ToolRegistry,
};
use awaken_runtime::{AgentResolver, AgentRuntime};
use genai::adapter::AdapterKind;
use genai::resolver::{AuthData, Endpoint};
use genai::{Client, ModelIden, ServiceTarget, WebConfig};
use parking_lot::{Mutex, RwLock};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const CONFIG_LOAD_PAGE_SIZE: usize = 1024;

const NS_AGENTS: &str = "agents";
const NS_MODELS: &str = "models";
const NS_PROVIDERS: &str = "providers";
const NS_MCP_SERVERS: &str = "mcp-servers";

/// Per-provider executor cache entry: the spec used to build the cached
/// executor and the executor itself.
type ProviderExecutorCache = HashMap<String, (ProviderSpec, Arc<dyn LlmExecutor>)>;

#[derive(Debug, thiserror::Error)]
pub enum ConfigRuntimeError {
    #[error("runtime does not expose a configurable registry snapshot")]
    RuntimeNotConfigurable,
    #[error(
        "config store is partially initialized; bootstrap requires all managed namespaces to be empty or all core namespaces populated"
    )]
    PartialBootstrap,
    #[error("unsupported provider adapter: {0}")]
    UnsupportedProviderAdapter(String),
    #[error("invalid managed config: {0}")]
    InvalidConfig(String),
    #[error("periodic refresh error: {0}")]
    PeriodicRefresh(String),
    #[error("config change listener error: {0}")]
    ChangeListener(String),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

#[derive(Default)]
struct RemoteAgentFallbackRegistry {
    exact: HashMap<String, AgentSpec>,
    plain: HashMap<String, AgentSpec>,
}

impl RemoteAgentFallbackRegistry {
    fn from_registry(registry: Arc<dyn AgentSpecRegistry>) -> Option<Arc<dyn AgentSpecRegistry>> {
        let mut exact = HashMap::new();
        let mut plain = HashMap::new();

        for id in registry.agent_ids() {
            let Some(spec) = registry.get_agent(&id) else {
                continue;
            };
            if spec.endpoint.is_none() && spec.registry.is_none() {
                continue;
            }
            plain.entry(spec.id.clone()).or_insert_with(|| spec.clone());
            exact.insert(id, spec);
        }

        if exact.is_empty() {
            None
        } else {
            Some(Arc::new(Self { exact, plain }) as Arc<dyn AgentSpecRegistry>)
        }
    }
}

impl AgentSpecRegistry for RemoteAgentFallbackRegistry {
    fn get_agent(&self, id: &str) -> Option<AgentSpec> {
        self.exact
            .get(id)
            .cloned()
            .or_else(|| self.plain.get(id).cloned())
    }

    fn agent_ids(&self) -> Vec<String> {
        let mut ids: Vec<_> = self.exact.keys().cloned().collect();
        ids.sort();
        ids
    }
}

macro_rules! overlay_registry {
    ($name:ident, $trait:ident, $get:ident -> $ret:ty, $ids:ident) => {
        struct $name {
            base: Arc<dyn $trait>,
            overlay: Arc<dyn $trait>,
        }

        impl $name {
            fn new(base: Arc<dyn $trait>, overlay: Arc<dyn $trait>) -> Self {
                Self { base, overlay }
            }
        }

        impl $trait for $name {
            fn $get(&self, id: &str) -> $ret {
                self.base.$get(id).or_else(|| self.overlay.$get(id))
            }

            fn $ids(&self) -> Vec<String> {
                let mut ids = self.base.$ids();
                ids.extend(self.overlay.$ids());
                ids.sort();
                ids.dedup();
                ids
            }
        }
    };
}

overlay_registry!(OverlayAgentRegistry, AgentSpecRegistry, get_agent -> Option<AgentSpec>, agent_ids);
overlay_registry!(OverlayToolRegistry, ToolRegistry, get_tool -> Option<Arc<dyn awaken_contract::contract::tool::Tool>>, tool_ids);

#[derive(Clone)]
struct DynamicMcpToolRegistry {
    registry: McpToolRegistry,
}

impl DynamicMcpToolRegistry {
    fn new(registry: McpToolRegistry) -> Self {
        Self { registry }
    }
}

impl ToolRegistry for DynamicMcpToolRegistry {
    fn get_tool(&self, id: &str) -> Option<Arc<dyn awaken_contract::contract::tool::Tool>> {
        self.registry.get(id)
    }

    fn tool_ids(&self) -> Vec<String> {
        self.registry.ids()
    }
}

pub trait ProviderExecutorFactory: Send + Sync {
    fn build(&self, spec: &ProviderSpec) -> Result<Arc<dyn LlmExecutor>, ConfigRuntimeError>;
}

#[derive(Default)]
pub struct GenaiProviderExecutorFactory;

impl ProviderExecutorFactory for GenaiProviderExecutorFactory {
    fn build(&self, spec: &ProviderSpec) -> Result<Arc<dyn LlmExecutor>, ConfigRuntimeError> {
        build_genai_provider_executor(spec)
    }
}

#[async_trait]
pub trait ManagedMcpRegistry: Send + Sync {
    fn tool_registry(&self) -> Arc<dyn ToolRegistry>;
    fn periodic_refresh_running(&self) -> bool;
    fn start_periodic_refresh(&self, interval: Duration) -> Result<(), ConfigRuntimeError>;
    async fn stop_periodic_refresh(&self) -> bool;
}

#[async_trait]
pub trait McpRegistryFactory: Send + Sync {
    async fn connect(
        &self,
        specs: &[McpServerSpec],
    ) -> Result<Option<Arc<dyn ManagedMcpRegistry>>, ConfigRuntimeError>;
}

#[derive(Default)]
pub struct DefaultMcpRegistryFactory;

#[derive(Clone)]
struct RealManagedMcpRegistry {
    manager: McpToolRegistryManager,
    tool_registry: Arc<dyn ToolRegistry>,
}

#[async_trait]
impl ManagedMcpRegistry for RealManagedMcpRegistry {
    fn tool_registry(&self) -> Arc<dyn ToolRegistry> {
        Arc::clone(&self.tool_registry)
    }

    fn periodic_refresh_running(&self) -> bool {
        self.manager.periodic_refresh_running()
    }

    fn start_periodic_refresh(&self, interval: Duration) -> Result<(), ConfigRuntimeError> {
        self.manager
            .start_periodic_refresh(interval)
            .map_err(|error| ConfigRuntimeError::InvalidConfig(error.to_string()))
    }

    async fn stop_periodic_refresh(&self) -> bool {
        self.manager.stop_periodic_refresh().await
    }
}

#[async_trait]
impl McpRegistryFactory for DefaultMcpRegistryFactory {
    async fn connect(
        &self,
        specs: &[McpServerSpec],
    ) -> Result<Option<Arc<dyn ManagedMcpRegistry>>, ConfigRuntimeError> {
        if specs.is_empty() {
            return Ok(None);
        }

        let configs = specs
            .iter()
            .map(mcp_spec_to_connection_config)
            .collect::<Result<Vec<_>, _>>()?;
        let manager = McpToolRegistryManager::connect(configs)
            .await
            .map_err(|error| {
                ConfigRuntimeError::InvalidConfig(format!("failed to connect MCP servers: {error}"))
            })?;

        Ok(Some(Arc::new(RealManagedMcpRegistry {
            tool_registry: Arc::new(DynamicMcpToolRegistry::new(manager.registry())),
            manager,
        }) as Arc<dyn ManagedMcpRegistry>))
    }
}

#[derive(Clone)]
struct ActiveMcpRegistry {
    specs: Vec<McpServerSpec>,
    handle: Arc<dyn ManagedMcpRegistry>,
    tool_registry: Arc<dyn ToolRegistry>,
}

struct PreparedMcpRegistry {
    tool_registry: Option<Arc<dyn ToolRegistry>>,
    next_state: Option<ActiveMcpRegistry>,
    state_changed: bool,
}

impl PreparedMcpRegistry {
    async fn cleanup(self) {
        if let Some(active) = self.next_state {
            active.handle.stop_periodic_refresh().await;
        }
    }
}

struct ManagedConfigSnapshot {
    providers: Vec<ProviderSpec>,
    models: Vec<ModelBindingSpec>,
    agents: Vec<AgentSpec>,
    mcp_servers: Vec<McpServerSpec>,
    fingerprint: u64,
}

struct ChangeListenerRuntime {
    stop_tx: Option<oneshot::Sender<()>>,
    join: JoinHandle<()>,
}

pub struct ConfigRuntimeManager {
    runtime: Arc<AgentRuntime>,
    store: Arc<dyn ConfigStore>,
    tools: Arc<dyn ToolRegistry>,
    plugins: Arc<dyn PluginSource>,
    backends: Arc<dyn BackendRegistry>,
    remote_agents: Option<Arc<dyn AgentSpecRegistry>>,
    provider_factory: Arc<dyn ProviderExecutorFactory>,
    change_notifier: Option<Arc<dyn ConfigChangeNotifier>>,
    mcp_registry_factory: Arc<dyn McpRegistryFactory>,
    apply_lock: tokio::sync::Mutex<()>,
    active_mcp_registry: Mutex<Option<ActiveMcpRegistry>>,
    last_applied_fingerprint: RwLock<Option<u64>>,
    /// Provider id → (last-built spec, cached executor). Hits skip the
    /// per-apply executor rebuild for providers whose spec is unchanged.
    /// Keys are pruned to the current providers list on every apply, so
    /// removed providers do not leak memory.
    provider_executor_cache: Mutex<ProviderExecutorCache>,
    periodic_refresh: PeriodicRefresher,
    change_listener: Mutex<Option<ChangeListenerRuntime>>,
    mcp_refresh_interval: RwLock<Option<Duration>>,
    /// Minimum interval between successive applies driven by the change
    /// listener. Bursts of events that arrive within this window coalesce
    /// into a single apply. Direct calls to [`Self::apply`] /
    /// [`Self::apply_if_changed`] are unaffected.
    min_apply_interval: Duration,
}

impl ConfigRuntimeManager {
    pub fn new(
        runtime: Arc<AgentRuntime>,
        store: Arc<dyn ConfigStore>,
    ) -> Result<Self, ConfigRuntimeError> {
        let registries = runtime
            .registry_set()
            .ok_or(ConfigRuntimeError::RuntimeNotConfigurable)?;
        let remote_agents = RemoteAgentFallbackRegistry::from_registry(registries.agents.clone());

        Ok(Self {
            runtime,
            store,
            tools: registries.tools,
            plugins: registries.plugins,
            backends: registries.backends,
            remote_agents,
            provider_factory: Arc::new(GenaiProviderExecutorFactory),
            change_notifier: None,
            mcp_registry_factory: Arc::new(DefaultMcpRegistryFactory),
            apply_lock: tokio::sync::Mutex::new(()),
            active_mcp_registry: Mutex::new(None),
            last_applied_fingerprint: RwLock::new(None),
            provider_executor_cache: Mutex::new(HashMap::new()),
            periodic_refresh: PeriodicRefresher::new(),
            change_listener: Mutex::new(None),
            mcp_refresh_interval: RwLock::new(None),
            min_apply_interval: Duration::ZERO,
        })
    }

    #[must_use]
    pub fn with_provider_factory(
        mut self,
        provider_factory: Arc<dyn ProviderExecutorFactory>,
    ) -> Self {
        self.provider_factory = provider_factory;
        self
    }

    #[must_use]
    pub fn with_change_notifier(mut self, notifier: Arc<dyn ConfigChangeNotifier>) -> Self {
        self.change_notifier = Some(notifier);
        self
    }

    #[must_use]
    pub fn with_mcp_registry_factory(mut self, factory: Arc<dyn McpRegistryFactory>) -> Self {
        self.mcp_registry_factory = factory;
        self
    }

    #[must_use]
    pub fn with_mcp_refresh_interval(self, interval: Duration) -> Self {
        if interval.is_zero() {
            return self;
        }
        *self.mcp_refresh_interval.write() = Some(interval);
        self
    }

    /// Set the minimum interval between successive applies driven by the
    /// change listener. Default is zero (no debounce). Direct calls to
    /// [`Self::apply`] / [`Self::apply_if_changed`] always run immediately
    /// regardless of this setting.
    #[must_use]
    pub fn with_min_apply_interval(mut self, interval: Duration) -> Self {
        self.min_apply_interval = interval;
        self
    }

    pub async fn bootstrap_if_empty(
        &self,
        providers: &[ProviderSpec],
        models: &[ModelBindingSpec],
        agents: &[AgentSpec],
        mcp_servers: &[McpServerSpec],
    ) -> Result<bool, ConfigRuntimeError> {
        let has_providers = !self.store.list(NS_PROVIDERS, 0, 1).await?.is_empty();
        let has_models = !self.store.list(NS_MODELS, 0, 1).await?.is_empty();
        let has_agents = !self.store.list(NS_AGENTS, 0, 1).await?.is_empty();
        let has_mcp_servers = !self.store.list(NS_MCP_SERVERS, 0, 1).await?.is_empty();

        if has_providers || has_models || has_agents || has_mcp_servers {
            if has_providers && has_models && has_agents {
                return Ok(false);
            }
            return Err(ConfigRuntimeError::PartialBootstrap);
        }

        for provider in providers {
            let json = serde_json::to_value(provider)
                .map_err(|e| StorageError::Serialization(e.to_string()))?;
            self.store.put(NS_PROVIDERS, &provider.id, &json).await?;
        }
        for model in models {
            let json = serde_json::to_value(model)
                .map_err(|e| StorageError::Serialization(e.to_string()))?;
            self.store.put(NS_MODELS, &model.id, &json).await?;
        }
        for agent in agents {
            let json = serde_json::to_value(agent)
                .map_err(|e| StorageError::Serialization(e.to_string()))?;
            self.store.put(NS_AGENTS, &agent.id, &json).await?;
        }
        for server in mcp_servers {
            let json = serde_json::to_value(server)
                .map_err(|e| StorageError::Serialization(e.to_string()))?;
            self.store.put(NS_MCP_SERVERS, &server.id, &json).await?;
        }

        Ok(true)
    }

    pub async fn apply(&self) -> Result<u64, ConfigRuntimeError> {
        let _guard = self.lock_apply().await;
        self.apply_locked().await
    }

    pub async fn apply_if_changed(&self) -> Result<Option<u64>, ConfigRuntimeError> {
        let _guard = self.lock_apply().await;
        self.apply_if_changed_locked().await
    }

    pub(crate) async fn lock_apply(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.apply_lock.lock().await
    }

    pub(crate) async fn apply_locked(&self) -> Result<u64, ConfigRuntimeError> {
        let managed = self.load_managed_config().await?;
        self.publish(managed).await
    }

    async fn apply_if_changed_locked(&self) -> Result<Option<u64>, ConfigRuntimeError> {
        let managed = self.load_managed_config().await?;
        let current_fingerprint = *self.last_applied_fingerprint.read();
        if current_fingerprint == Some(managed.fingerprint) {
            return Ok(None);
        }
        self.publish(managed).await.map(Some)
    }

    pub fn start_periodic_refresh(
        self: &Arc<Self>,
        interval: Duration,
    ) -> Result<(), ConfigRuntimeError> {
        if interval.is_zero() {
            return Err(ConfigRuntimeError::PeriodicRefresh(
                "interval must be non-zero".into(),
            ));
        }

        {
            let mut current_interval = self.mcp_refresh_interval.write();
            if current_interval.is_none() {
                *current_interval = Some(interval);
            }
        }

        if let Some(active) = self.active_mcp_registry.lock().clone() {
            self.ensure_mcp_periodic_refresh(&active.handle)?;
        }
        self.start_change_listener()?;

        let weak = Arc::downgrade(self);
        self.periodic_refresh
            .start(interval, move || {
                let weak = Weak::clone(&weak);
                async move {
                    let Some(manager) = weak.upgrade() else {
                        return;
                    };
                    if let Err(error) = manager.apply_if_changed().await {
                        tracing::warn!(error = %error, "config periodic refresh failed");
                    }
                }
            })
            .map_err(ConfigRuntimeError::PeriodicRefresh)
    }

    pub async fn stop_periodic_refresh(&self) -> bool {
        let stopped_config = self.periodic_refresh.stop().await;
        let stopped_listener = self.stop_change_listener().await;
        let active = self.active_mcp_registry.lock().clone();
        let stopped_mcp = if let Some(active) = active {
            active.handle.stop_periodic_refresh().await
        } else {
            false
        };
        stopped_config || stopped_listener || stopped_mcp
    }

    pub fn periodic_refresh_running(&self) -> bool {
        self.periodic_refresh.is_running()
    }

    async fn publish(&self, managed: ManagedConfigSnapshot) -> Result<u64, ConfigRuntimeError> {
        let prepared_mcp = self.prepare_mcp_registry(&managed.mcp_servers).await?;
        let candidate = match self.compile_registry_set(
            &managed.providers,
            &managed.models,
            &managed.agents,
            prepared_mcp.tool_registry.clone(),
        ) {
            Ok(candidate) => candidate,
            Err(error) => {
                prepared_mcp.cleanup().await;
                return Err(error);
            }
        };

        if let Err(error) = self.validate_candidate(&candidate, &managed.agents) {
            prepared_mcp.cleanup().await;
            return Err(error);
        }

        let version = match self.runtime.replace_registry_set(candidate) {
            Some(version) => version,
            None => {
                prepared_mcp.cleanup().await;
                return Err(ConfigRuntimeError::RuntimeNotConfigurable);
            }
        };

        let previous_mcp = if prepared_mcp.state_changed {
            let mut active = self.active_mcp_registry.lock();
            std::mem::replace(&mut *active, prepared_mcp.next_state)
        } else {
            None
        };

        *self.last_applied_fingerprint.write() = Some(managed.fingerprint);

        if let Some(previous) = previous_mcp {
            previous.handle.stop_periodic_refresh().await;
        }

        Ok(version)
    }

    async fn prepare_mcp_registry(
        &self,
        specs: &[McpServerSpec],
    ) -> Result<PreparedMcpRegistry, ConfigRuntimeError> {
        let current = self.active_mcp_registry.lock().clone();
        if let Some(current) = current
            && current.specs == specs
        {
            self.ensure_mcp_periodic_refresh(&current.handle)?;
            return Ok(PreparedMcpRegistry {
                tool_registry: Some(current.tool_registry),
                next_state: None,
                state_changed: false,
            });
        }

        let next_state = self
            .mcp_registry_factory
            .connect(specs)
            .await?
            .map(|handle| ActiveMcpRegistry {
                specs: specs.to_vec(),
                tool_registry: handle.tool_registry(),
                handle,
            });

        if let Some(ref active) = next_state {
            self.ensure_mcp_periodic_refresh(&active.handle)?;
        }

        Ok(PreparedMcpRegistry {
            tool_registry: next_state
                .as_ref()
                .map(|active| active.tool_registry.clone()),
            next_state,
            state_changed: true,
        })
    }

    fn ensure_mcp_periodic_refresh(
        &self,
        handle: &Arc<dyn ManagedMcpRegistry>,
    ) -> Result<(), ConfigRuntimeError> {
        let interval = *self.mcp_refresh_interval.read();
        let Some(interval) = interval else {
            return Ok(());
        };
        if handle.periodic_refresh_running() {
            return Ok(());
        }
        handle.start_periodic_refresh(interval)
    }

    fn start_change_listener(self: &Arc<Self>) -> Result<(), ConfigRuntimeError> {
        let Some(notifier) = self.change_notifier.clone() else {
            return Ok(());
        };

        let runtime_handle = Handle::try_current()
            .map_err(|error| ConfigRuntimeError::ChangeListener(error.to_string()))?;

        let mut guard = self.change_listener.lock();
        if guard
            .as_ref()
            .is_some_and(|runtime| !runtime.join.is_finished())
        {
            return Ok(());
        }
        if guard
            .as_ref()
            .is_some_and(|runtime| runtime.join.is_finished())
        {
            *guard = None;
        }

        let (stop_tx, mut stop_rx) = oneshot::channel();
        let weak = Arc::downgrade(self);
        let min_apply_interval = self.min_apply_interval;
        let join = runtime_handle.spawn(async move {
            let retry_delay = Duration::from_secs(1);
            // `last_applied_at` is `None` until the first event-driven apply,
            // so the first event is never delayed.
            let mut last_applied_at: Option<tokio::time::Instant> = None;

            loop {
                let mut subscriber = tokio::select! {
                    _ = &mut stop_rx => break,
                    result = notifier.subscribe() => match result {
                        Ok(subscriber) => subscriber,
                        Err(error) => {
                            tracing::warn!(error = %error, "config change listener subscribe failed");
                            tokio::select! {
                                _ = &mut stop_rx => break,
                                _ = tokio::time::sleep(retry_delay) => continue,
                            }
                        }
                    }
                };

                loop {
                    let event = tokio::select! {
                        _ = &mut stop_rx => return,
                        result = subscriber.next() => result,
                    };

                    let event = match event {
                        Ok(event) => event,
                        Err(error) => {
                            tracing::warn!(error = %error, "config change listener receive failed");
                            break;
                        }
                    };

                    let Some(manager) = weak.upgrade() else {
                        return;
                    };

                    tracing::debug!(
                        namespace = %event.namespace,
                        id = %event.id,
                        kind = ?event.kind,
                        "config change notification received"
                    );

                    // Enforce the minimum apply interval and coalesce any
                    // events that arrive while we are waiting. Direct calls
                    // to `manager.apply()` are unaffected.
                    if !min_apply_interval.is_zero()
                        && let Some(last) = last_applied_at
                    {
                        let next_allowed = last + min_apply_interval;
                        let now = tokio::time::Instant::now();
                        if now < next_allowed {
                            let wait = next_allowed - now;
                            tokio::select! {
                                _ = &mut stop_rx => return,
                                _ = tokio::time::sleep(wait) => {}
                            }
                            // Drain any events that arrived during the wait
                            // so we apply once for the whole burst. The
                            // subscriber trait is async-only, so we peek
                            // with a zero-duration timeout.
                            while tokio::time::timeout(
                                Duration::ZERO,
                                subscriber.next(),
                            )
                            .await
                            .is_ok()
                            {
                                // discarded — apply_if_changed reads the
                                // latest store snapshot below.
                            }
                        }
                    }

                    if let Err(error) = manager.apply_if_changed().await {
                        tracing::warn!(error = %error, "config change apply failed");
                    }
                    last_applied_at = Some(tokio::time::Instant::now());
                }

                tokio::select! {
                    _ = &mut stop_rx => break,
                    _ = tokio::time::sleep(retry_delay) => {}
                }
            }
        });

        *guard = Some(ChangeListenerRuntime {
            stop_tx: Some(stop_tx),
            join,
        });
        Ok(())
    }

    async fn stop_change_listener(&self) -> bool {
        let runtime = {
            let mut guard = self.change_listener.lock();
            guard.take()
        };

        let Some(mut runtime) = runtime else {
            return false;
        };

        if let Some(stop_tx) = runtime.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        let _ = runtime.join.await;
        true
    }

    async fn load_managed_config(&self) -> Result<ManagedConfigSnapshot, ConfigRuntimeError> {
        let provider_values = self.load_namespace_entries(NS_PROVIDERS).await?;
        let model_values = self.load_namespace_entries(NS_MODELS).await?;
        let agent_values = self.load_namespace_entries(NS_AGENTS).await?;
        let mcp_values = self.load_namespace_entries(NS_MCP_SERVERS).await?;

        let fingerprint = fingerprint_config(&[
            (NS_PROVIDERS, &provider_values),
            (NS_MODELS, &model_values),
            (NS_AGENTS, &agent_values),
            (NS_MCP_SERVERS, &mcp_values),
        ])?;

        Ok(ManagedConfigSnapshot {
            providers: deserialize_namespace(&provider_values)?,
            models: deserialize_namespace(&model_values)?,
            agents: deserialize_namespace(&agent_values)?,
            mcp_servers: deserialize_namespace(&mcp_values)?,
            fingerprint,
        })
    }

    async fn load_namespace_entries(
        &self,
        namespace: &str,
    ) -> Result<Vec<(String, Value)>, ConfigRuntimeError> {
        let mut entries = Vec::new();
        let mut offset = 0usize;

        loop {
            let page = self
                .store
                .list(namespace, offset, CONFIG_LOAD_PAGE_SIZE)
                .await?;
            if page.is_empty() {
                break;
            }

            offset = offset.saturating_add(page.len());
            let reached_end = page.len() < CONFIG_LOAD_PAGE_SIZE;
            entries.extend(page);
            if reached_end {
                break;
            }
        }

        Ok(entries)
    }

    fn compile_registry_set(
        &self,
        providers: &[ProviderSpec],
        models: &[ModelBindingSpec],
        agents: &[AgentSpec],
        dynamic_tools: Option<Arc<dyn ToolRegistry>>,
    ) -> Result<RegistrySet, ConfigRuntimeError> {
        let mut provider_registry = MapProviderRegistry::new();
        let mut next_cache: ProviderExecutorCache = HashMap::with_capacity(providers.len());
        let prior_cache = self.provider_executor_cache.lock().clone();
        for provider in providers {
            let executor = match prior_cache.get(&provider.id) {
                Some((cached_spec, cached_executor)) if cached_spec == provider => {
                    Arc::clone(cached_executor)
                }
                _ => self.provider_factory.build(provider)?,
            };
            next_cache.insert(
                provider.id.clone(),
                (provider.clone(), Arc::clone(&executor)),
            );
            provider_registry
                .register_provider(provider.id.clone(), executor)
                .map_err(|error| ConfigRuntimeError::InvalidConfig(error.to_string()))?;
        }
        *self.provider_executor_cache.lock() = next_cache;

        let mut model_registry = MapModelRegistry::new();
        for model in models {
            model_registry
                .register_model(model.id.clone(), ModelBinding::from(model))
                .map_err(|error| ConfigRuntimeError::InvalidConfig(error.to_string()))?;
        }

        let mut local_agents = MapAgentSpecRegistry::new();
        for agent in agents {
            local_agents
                .register_spec(agent.clone())
                .map_err(|error| ConfigRuntimeError::InvalidConfig(error.to_string()))?;
        }

        let local_agents: Arc<dyn AgentSpecRegistry> = Arc::new(local_agents);
        let agents = match &self.remote_agents {
            Some(fallback) => Arc::new(OverlayAgentRegistry::new(
                local_agents,
                Arc::clone(fallback),
            )) as Arc<dyn AgentSpecRegistry>,
            None => local_agents,
        };

        let tools = self.compose_tool_registry(dynamic_tools)?;

        Ok(RegistrySet {
            agents,
            tools,
            models: Arc::new(model_registry),
            providers: Arc::new(provider_registry),
            plugins: Arc::clone(&self.plugins),
            backends: Arc::clone(&self.backends),
        })
    }

    fn compose_tool_registry(
        &self,
        dynamic_tools: Option<Arc<dyn ToolRegistry>>,
    ) -> Result<Arc<dyn ToolRegistry>, ConfigRuntimeError> {
        let Some(dynamic_tools) = dynamic_tools else {
            return Ok(Arc::clone(&self.tools));
        };

        let base_ids: HashSet<_> = self.tools.tool_ids().into_iter().collect();
        for tool_id in dynamic_tools.tool_ids() {
            if base_ids.contains(&tool_id) {
                return Err(ConfigRuntimeError::InvalidConfig(format!(
                    "mcp tool id conflicts with existing tool: {tool_id}"
                )));
            }
        }

        Ok(Arc::new(OverlayToolRegistry::new(
            Arc::clone(&self.tools),
            dynamic_tools,
        )) as Arc<dyn ToolRegistry>)
    }

    fn validate_candidate(
        &self,
        candidate: &RegistrySet,
        local_agents: &[AgentSpec],
    ) -> Result<(), ConfigRuntimeError> {
        let resolver = RegistrySetResolver::new(candidate.clone());
        for agent in local_agents {
            if agent.endpoint.is_some() {
                continue;
            }
            resolver.resolve(&agent.id).map_err(|error| {
                ConfigRuntimeError::InvalidConfig(format!("{}: {error}", agent.id))
            })?;
        }
        Ok(())
    }
}

pub fn build_genai_provider_executor(
    spec: &ProviderSpec,
) -> Result<Arc<dyn LlmExecutor>, ConfigRuntimeError> {
    let adapter_kind = parse_adapter_kind(&spec.adapter)?;
    let mut builder = Client::builder().with_model_mapper_fn(move |model: ModelIden| {
        Ok(ModelIden::new(adapter_kind, model.model_name.to_string()))
    });

    if let Some(api_key) = spec.api_key.as_ref().filter(|value| !value.is_empty()) {
        let key = api_key.expose_secret().to_owned();
        builder =
            builder.with_auth_resolver_fn(move |_| Ok(Some(AuthData::from_single(key.clone()))));
    }

    if let Some(base_url) = spec.base_url.clone().filter(|value| !value.is_empty()) {
        let normalized = if base_url.ends_with('/') {
            base_url
        } else {
            format!("{base_url}/")
        };
        builder = builder.with_service_target_resolver_fn(move |mut target: ServiceTarget| {
            target.endpoint = Endpoint::from_owned(normalized.clone());
            Ok(target)
        });
    }

    if let Some(headers) = build_default_headers_from_options(&spec.adapter_options)? {
        builder = builder.with_web_config(WebConfig::default().with_default_headers(headers));
    }

    let client = builder.build();
    let executor = GenaiExecutor::with_client(client)
        .with_timeout(Duration::from_secs(spec.timeout_secs.max(1)));
    Ok(Arc::new(executor))
}

/// Parse `adapter_options.headers` into a [`HeaderMap`]. Returns `Ok(None)`
/// when the key is absent. Returns [`ConfigRuntimeError::InvalidConfig`] when
/// the value is not an object of `string -> string` pairs or when an entry
/// fails to parse as a valid HTTP header.
///
/// All other keys in `adapter_options` are ignored here — unknown keys are a
/// forward-compatibility surface, not an error.
fn build_default_headers_from_options(
    options: &BTreeMap<String, Value>,
) -> Result<Option<HeaderMap>, ConfigRuntimeError> {
    let Some(headers_value) = options.get("headers") else {
        return Ok(None);
    };
    let entries = headers_value.as_object().ok_or_else(|| {
        ConfigRuntimeError::InvalidConfig(
            "adapter_options.headers must be an object of string -> string pairs".into(),
        )
    })?;

    let mut map = HeaderMap::with_capacity(entries.len());
    for (name, value) in entries {
        let value_str = value.as_str().ok_or_else(|| {
            ConfigRuntimeError::InvalidConfig(format!(
                "adapter_options.headers[{name}] must be a string"
            ))
        })?;
        let header_name = HeaderName::try_from(name).map_err(|err| {
            ConfigRuntimeError::InvalidConfig(format!(
                "adapter_options.headers[{name}] invalid header name: {err}"
            ))
        })?;
        let header_value = HeaderValue::from_str(value_str).map_err(|err| {
            ConfigRuntimeError::InvalidConfig(format!(
                "adapter_options.headers[{name}] invalid header value: {err}"
            ))
        })?;
        map.insert(header_name, header_value);
    }
    Ok(Some(map))
}

/// Canonical list of provider adapter identifiers supported by the runtime.
pub fn supported_adapters() -> &'static [&'static str] {
    &[
        "anthropic",
        "openai",
        "openai_resp",
        "deepseek",
        "gemini",
        "ollama",
        "cohere",
        "together",
        "fireworks",
        "groq",
        "xai",
        "zai",
        "bigmodel",
        "aliyun",
        "mimo",
        "nebius",
    ]
}

fn parse_adapter_kind(adapter: &str) -> Result<AdapterKind, ConfigRuntimeError> {
    match adapter.trim().to_ascii_lowercase().as_str() {
        "openai" => Ok(AdapterKind::OpenAI),
        "openai_resp" | "openai-resp" | "responses" => Ok(AdapterKind::OpenAIResp),
        "anthropic" => Ok(AdapterKind::Anthropic),
        "gemini" => Ok(AdapterKind::Gemini),
        "ollama" => Ok(AdapterKind::Ollama),
        "cohere" => Ok(AdapterKind::Cohere),
        "deepseek" => Ok(AdapterKind::DeepSeek),
        "together" => Ok(AdapterKind::Together),
        "fireworks" => Ok(AdapterKind::Fireworks),
        "groq" => Ok(AdapterKind::Groq),
        "xai" => Ok(AdapterKind::Xai),
        "zai" => Ok(AdapterKind::Zai),
        "bigmodel" => Ok(AdapterKind::BigModel),
        "aliyun" => Ok(AdapterKind::Aliyun),
        "mimo" => Ok(AdapterKind::Mimo),
        "nebius" => Ok(AdapterKind::Nebius),
        other => Err(ConfigRuntimeError::UnsupportedProviderAdapter(
            other.to_string(),
        )),
    }
}

fn mcp_spec_to_connection_config(
    spec: &McpServerSpec,
) -> Result<McpServerConnectionConfig, ConfigRuntimeError> {
    if spec.id.trim().is_empty() {
        return Err(ConfigRuntimeError::InvalidConfig(
            "mcp server id cannot be empty".into(),
        ));
    }

    let mut config = match spec.transport {
        McpTransportKind::Stdio => {
            let command = spec
                .command
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    ConfigRuntimeError::InvalidConfig(format!(
                        "mcp server '{}' requires a non-empty command",
                        spec.id
                    ))
                })?;
            McpServerConnectionConfig::stdio(spec.id.clone(), command, spec.args.clone())
        }
        McpTransportKind::Http => {
            let url = spec
                .url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    ConfigRuntimeError::InvalidConfig(format!(
                        "mcp server '{}' requires a non-empty url",
                        spec.id
                    ))
                })?;
            McpServerConnectionConfig::http(spec.id.clone(), url)
        }
    };

    config.timeout_secs = spec.timeout_secs.max(1);
    config.config = Value::Object(spec.config.clone());
    config.env = spec.env.clone().into_iter().collect();
    config.restart_policy = restart_policy_to_connection_policy(&spec.restart_policy);
    Ok(config)
}

fn restart_policy_to_connection_policy(policy: &McpRestartPolicy) -> mcp::transport::RestartPolicy {
    mcp::transport::RestartPolicy {
        enabled: policy.enabled,
        max_attempts: policy.max_attempts,
        delay_ms: policy.delay_ms,
        backoff_multiplier: policy.backoff_multiplier,
        max_delay_ms: policy.max_delay_ms,
    }
}

fn deserialize_namespace<T>(entries: &[(String, Value)]) -> Result<Vec<T>, ConfigRuntimeError>
where
    T: serde::de::DeserializeOwned,
{
    entries
        .iter()
        .map(|(_, value)| {
            serde_json::from_value(value.clone())
                .map_err(|error| StorageError::Serialization(error.to_string()))
                .map_err(ConfigRuntimeError::Storage)
        })
        .collect()
}

fn fingerprint_config(
    namespaces: &[(&str, &[(String, Value)])],
) -> Result<u64, ConfigRuntimeError> {
    let mut hasher = DefaultHasher::new();

    for (namespace, entries) in namespaces {
        namespace.hash(&mut hasher);
        entries.len().hash(&mut hasher);

        for (id, value) in *entries {
            id.hash(&mut hasher);
            let canonical = canonicalize_value(value);
            let serialized = serde_json::to_vec(&canonical)
                .map_err(|error| StorageError::Serialization(error.to_string()))
                .map_err(ConfigRuntimeError::Storage)?;
            serialized.hash(&mut hasher);
        }
    }

    Ok(hasher.finish())
}

fn canonicalize_value(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonicalize_value).collect()),
        Value::Object(object) => {
            let mut keys = object.keys().cloned().collect::<Vec<_>>();
            keys.sort();

            let mut normalized = serde_json::Map::new();
            for key in keys {
                if let Some(value) = object.get(&key) {
                    normalized.insert(key, canonicalize_value(value));
                }
            }
            Value::Object(normalized)
        }
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn provider_spec_with_options(adapter_options: BTreeMap<String, Value>) -> ProviderSpec {
        ProviderSpec {
            id: "test".into(),
            adapter: "openai".into(),
            adapter_options,
            ..ProviderSpec::default()
        }
    }

    #[test]
    fn build_genai_with_valid_headers_succeeds() {
        let mut options = BTreeMap::new();
        options.insert("headers".into(), json!({"OpenAI-Organization": "org-xyz"}));
        let spec = provider_spec_with_options(options);
        build_genai_provider_executor(&spec).expect("valid headers must build");
    }

    #[test]
    fn build_genai_rejects_non_object_headers() {
        let mut options = BTreeMap::new();
        options.insert("headers".into(), json!("not-an-object"));
        let spec = provider_spec_with_options(options);
        let err = match build_genai_provider_executor(&spec) {
            Ok(_) => panic!("expected build to fail"),
            Err(e) => e,
        };
        assert!(
            matches!(err, ConfigRuntimeError::InvalidConfig(ref msg) if msg.contains("headers")),
            "expected InvalidConfig mentioning headers, got: {err:?}"
        );
    }

    #[test]
    fn build_genai_rejects_non_string_header_value() {
        let mut options = BTreeMap::new();
        options.insert("headers".into(), json!({"X-Numeric-Value": 42}));
        let spec = provider_spec_with_options(options);
        let err = match build_genai_provider_executor(&spec) {
            Ok(_) => panic!("expected build to fail"),
            Err(e) => e,
        };
        assert!(
            matches!(err, ConfigRuntimeError::InvalidConfig(ref msg) if msg.contains("X-Numeric-Value")),
            "expected InvalidConfig naming the bad header, got: {err:?}"
        );
    }

    #[test]
    fn build_genai_ignores_unknown_adapter_options() {
        let mut options = BTreeMap::new();
        options.insert("future_extension_key".into(), json!({"anything": true}));
        let spec = provider_spec_with_options(options);
        build_genai_provider_executor(&spec)
            .expect("unknown adapter_options keys must not break the build");
    }

    #[test]
    fn build_default_headers_returns_none_when_absent() {
        let result = build_default_headers_from_options(&BTreeMap::new()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn build_default_headers_parses_string_pairs() {
        let mut options = BTreeMap::new();
        options.insert(
            "headers".into(),
            json!({
                "OpenAI-Organization": "org-xyz",
                "X-Custom": "value",
            }),
        );
        let map = build_default_headers_from_options(&options)
            .unwrap()
            .expect("headers should be present");
        assert_eq!(
            map.get("openai-organization").and_then(|v| v.to_str().ok()),
            Some("org-xyz")
        );
        assert_eq!(
            map.get("x-custom").and_then(|v| v.to_str().ok()),
            Some("value")
        );
    }

    #[test]
    fn build_default_headers_rejects_invalid_header_name() {
        let mut options = BTreeMap::new();
        options.insert("headers".into(), json!({"Invalid Header Name": "value"}));
        let err = build_default_headers_from_options(&options).unwrap_err();
        assert!(
            matches!(err, ConfigRuntimeError::InvalidConfig(ref msg) if msg.contains("Invalid Header Name")),
            "expected InvalidConfig naming the bad header, got: {err:?}"
        );
    }
}
