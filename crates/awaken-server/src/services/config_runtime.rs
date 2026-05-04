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
    AgentSpec, ConfigRecord, McpRestartPolicy, McpServerSpec, McpTransportKind, ModelBindingSpec,
    PeriodicRefresher, ProviderSpec,
};
use awaken_ext_mcp::{
    McpServerConnectionConfig, McpServerStatusSnapshot, McpToolRegistry, McpToolRegistryManager,
};
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
        "unsupported provider adapter: {0} (valid names mirror genai::adapter::AdapterKind — see https://docs.rs/genai/latest/genai/adapter/enum.AdapterKind.html)"
    )]
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

/// Holds A2A-discovered agent specs (those with `endpoint` or `registry`
/// set). Built once from the runtime's pre-apply registry; subsequent
/// `ConfigRuntimeManager::apply()` calls overlay these on top of the
/// ConfigStore-derived registry. Pure code-defined regular agents flow
/// only via ConfigStore (Phase 2 unification) and do NOT appear here.
#[derive(Default)]
struct DiscoveredAgentRegistry {
    exact: HashMap<String, AgentSpec>,
    plain: HashMap<String, AgentSpec>,
}

impl DiscoveredAgentRegistry {
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

impl AgentSpecRegistry for DiscoveredAgentRegistry {
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

// AgentSpecRegistryWithDiscovery: ConfigStore-side agents (base) ⊕
// runtime-discovered remote agents (overlay). Resolves a given id by
// preferring base; falls through to discovery only if base does not
// contain the id.
overlay_registry!(AgentSpecRegistryWithDiscovery, AgentSpecRegistry, get_agent -> Option<AgentSpec>, agent_ids);
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

/// Provider executor factory backed by genai.
///
/// Every executor this factory builds shares the same credential broker —
/// so token caches, single-flight refreshes, and metrics are unified
/// across all providers in this process. The default constructor creates
/// a fresh broker, suitable for tests; production wiring should pass the
/// `AppState`-scoped broker via [`with_broker`](Self::with_broker).
pub struct GenaiProviderExecutorFactory {
    broker: Arc<dyn awaken_runtime::credentials::CredentialBroker>,
}

impl Default for GenaiProviderExecutorFactory {
    fn default() -> Self {
        Self {
            broker: Arc::new(awaken_runtime::credentials::AwakenCredentialBroker::new()),
        }
    }
}

impl GenaiProviderExecutorFactory {
    /// Construct a factory bound to the given broker. The broker is
    /// shared across all executors this factory builds, which is what
    /// production wiring wants.
    pub fn with_broker(broker: Arc<dyn awaken_runtime::credentials::CredentialBroker>) -> Self {
        Self { broker }
    }
}

impl ProviderExecutorFactory for GenaiProviderExecutorFactory {
    fn build(&self, spec: &ProviderSpec) -> Result<Arc<dyn LlmExecutor>, ConfigRuntimeError> {
        build_genai_provider_executor(spec, Arc::clone(&self.broker))
    }
}

#[async_trait]
pub trait ManagedMcpRegistry: Send + Sync {
    fn tool_registry(&self) -> Arc<dyn ToolRegistry>;
    fn periodic_refresh_running(&self) -> bool;
    fn start_periodic_refresh(&self, interval: Duration) -> Result<(), ConfigRuntimeError>;
    async fn stop_periodic_refresh(&self) -> bool;
    fn server_status(&self, server_name: &str) -> Option<McpServerStatusSnapshot>;
    async fn reconnect(&self, server_name: &str) -> Result<(), ConfigRuntimeError>;
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

    fn server_status(&self, server_name: &str) -> Option<McpServerStatusSnapshot> {
        self.manager.server_status_snapshot(server_name).ok()
    }

    async fn reconnect(&self, server_name: &str) -> Result<(), ConfigRuntimeError> {
        self.manager
            .reconnect(server_name)
            .await
            .map_err(|e| ConfigRuntimeError::InvalidConfig(e.to_string()))
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
    /// Runtime A2A-discovery layer; merged on top of ConfigStore agents
    /// at every `apply()`. None when no remote agents were registered
    /// at builder time.
    discovered_agents: Option<Arc<dyn AgentSpecRegistry>>,
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
    /// Optional audit logger — if set, `apply_seed` emits a `SeedApply` event
    /// per non-empty bucket of the resulting [`SeedReport`].
    audit_log: Option<Arc<crate::services::audit_log::AuditLogger>>,
}

impl ConfigRuntimeManager {
    pub fn new(
        runtime: Arc<AgentRuntime>,
        store: Arc<dyn ConfigStore>,
    ) -> Result<Self, ConfigRuntimeError> {
        let registries = runtime
            .registry_set()
            .ok_or(ConfigRuntimeError::RuntimeNotConfigurable)?;
        let discovered_agents = DiscoveredAgentRegistry::from_registry(registries.agents.clone());

        Ok(Self {
            runtime,
            store,
            tools: registries.tools,
            plugins: registries.plugins,
            backends: registries.backends,
            discovered_agents,
            provider_factory: Arc::new(GenaiProviderExecutorFactory::default()),
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
            audit_log: None,
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

    /// Attach an audit logger. When set, [`Self::apply_seed`] emits a
    /// `SeedApply` audit event per non-empty bucket of the resulting report.
    #[must_use]
    pub fn with_audit_log(mut self, logger: Arc<crate::services::audit_log::AuditLogger>) -> Self {
        self.audit_log = Some(logger);
        self
    }

    /// Apply a built-in spec seed to the underlying ConfigStore.
    ///
    /// Idempotent and version-aware. See
    /// [`apply_builtin_seed`](crate::services::builtin_seed::apply_builtin_seed)
    /// for the full decision matrix and concurrency precondition.
    ///
    /// Holds the apply-lock; will block on a concurrent `apply()`/PUT/DELETE.
    /// This ensures seed writes are serialized with runtime-registry publishes
    /// so a concurrent HTTP write cannot race with the boot seed.
    ///
    /// Typical bootstrap sequence:
    /// 1. `manager.apply_seed(&seed).await?` — write/refresh built-ins.
    /// 2. `manager.apply().await?` — publish the resulting registry.
    pub async fn apply_seed(
        &self,
        seed: &awaken_contract::BuiltinSeedSet,
    ) -> Result<crate::services::builtin_seed::SeedReport, ConfigRuntimeError> {
        let _guard = self.lock_apply().await;
        let report = crate::services::builtin_seed::apply_builtin_seed(self.store.as_ref(), seed)
            .await
            .map_err(map_seed_error)?;
        if let Some(audit) = &self.audit_log {
            audit.emit_seed_report(&report).await;
        }
        Ok(report)
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

    /// Return the live status snapshot for a managed MCP server.
    ///
    /// Returns `None` when no MCP registry is active (i.e. the runtime has no
    /// MCP servers configured) or the server name is unknown to the registry.
    pub fn mcp_server_status(&self, server_name: &str) -> Option<McpServerStatusSnapshot> {
        self.active_mcp_registry
            .lock()
            .as_ref()
            .and_then(|active| active.handle.server_status(server_name))
    }

    /// Trigger an immediate reconnect for the named MCP server.
    ///
    /// Returns an error when no MCP registry is active or the server name is
    /// unknown.
    pub async fn mcp_server_reconnect(&self, server_name: &str) -> Result<(), ConfigRuntimeError> {
        let handle = self
            .active_mcp_registry
            .lock()
            .as_ref()
            .map(|active| Arc::clone(&active.handle));
        match handle {
            Some(h) => h.reconnect(server_name).await,
            None => Err(ConfigRuntimeError::InvalidConfig(
                "no MCP registry is active".to_string(),
            )),
        }
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
                            // with a zero-duration timeout. A subscriber
                            // error here must surface — drain stops and
                            // the outer loop re-receives, hits the same
                            // error, and triggers a reconnect.
                            loop {
                                match tokio::time::timeout(
                                    Duration::ZERO,
                                    subscriber.next(),
                                )
                                .await
                                {
                                    Ok(Ok(_event)) => continue,
                                    Ok(Err(error)) => {
                                        tracing::warn!(
                                            error = %error,
                                            "config change listener receive failed while draining debounce window"
                                        );
                                        break;
                                    }
                                    Err(_elapsed) => break,
                                }
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
        let agents = match &self.discovered_agents {
            Some(fallback) => Arc::new(AgentSpecRegistryWithDiscovery::new(
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

/// Build an LLM executor from a [`ProviderSpec`].
///
/// Auth wiring branches on credential kind:
///
/// - **Static bearer / env-fallback** (0.4.0 default): the api_key (or
///   genai's per-adapter env var, when api_key is absent) is handed
///   directly to genai's synchronous `with_auth_resolver_fn`. The broker
///   is **not** consulted — there is no token to refresh and no token
///   endpoint to single-flight against. This keeps the inference hot path
///   identical to 0.4.0 and avoids the cache / lock churn the broker
///   would otherwise add per request.
///
/// - **Dynamic** (`service_account_json`, future cloud creds): material
///   is registered with the broker, and an async auth resolver consults
///   `broker.token_for(provider, scope)` per chat. This lets the broker's
///   cache + single-flight handle token rotation transparently.
///
/// Misconfigured material is rejected here (eager validation) rather
/// than at first inference. The provided broker is shared with all
/// dynamic providers built by the same caller; passing the
/// `AppState::credential_broker` is the production wiring.
pub fn build_genai_provider_executor(
    spec: &ProviderSpec,
    broker: Arc<dyn awaken_runtime::credentials::CredentialBroker>,
) -> Result<Arc<dyn LlmExecutor>, ConfigRuntimeError> {
    use awaken_runtime::credentials::{CredentialKind, build_material};

    let adapter_kind = parse_adapter_kind(&spec.adapter)?;
    let kind = CredentialKind::from_options(&spec.adapter_options)
        .map_err(ConfigRuntimeError::InvalidConfig)?;

    // Eager-validate material shape (malformed SA JSON, kind/adapter
    // mismatch, missing api_key for non-bearer kinds, disabled feature).
    // Bearer goes through `build_material` for the same eager check; the
    // returned `Option<CredentialMaterial>` is discarded for that branch
    // because the bearer wiring reads `spec.api_key` directly to bypass
    // the broker entirely. Non-bearer kinds *do* register the returned
    // material with the broker.
    let material = build_material(&spec.adapter, kind, spec.api_key.as_ref())
        .map_err(ConfigRuntimeError::InvalidConfig)?;

    let mut builder = Client::builder().with_model_mapper_fn(move |model: ModelIden| {
        Ok(ModelIden::new(adapter_kind, model.model_name.to_string()))
    });

    if matches!(kind, CredentialKind::Bearer) {
        // Static bearer / env-fallback path — identical wiring to 0.4.0.
        // Broker is bypassed entirely: there's no token to refresh and no
        // token endpoint to single-flight against, so cache/lock churn
        // would be pure overhead.
        if let Some(api_key) = spec.api_key.as_ref().filter(|k| !k.is_empty()) {
            let key = api_key.expose_secret().to_owned();
            builder = builder
                .with_auth_resolver_fn(move |_| Ok(Some(AuthData::from_single(key.clone()))));
        }
        // else: env-fallback — leave genai's default resolver
        // (VENDOR_API_KEY env var) in place.
    } else if let Some(material) = material {
        // Dynamic kind: register with the broker; the async resolver
        // consults `token_for` per chat call. Provider id and scope are
        // captured as `Arc<str>` so each invocation just bumps refcounts
        // rather than cloning two `String`s.
        broker.register(spec.id.clone(), material);

        let provider_id: Arc<str> = Arc::from(spec.id.as_str());
        let scope: Arc<str> = Arc::from(scopes_from_options(&spec.adapter_options)?);
        let broker_for_resolver = Arc::clone(&broker);

        // genai's `IntoAuthResolverAsyncFn` requires the closure to return
        // a `Pin<Box<dyn Future<Output = Result<Option<AuthData>>> + Send>>`,
        // not a bare `async` block. The explicit type erases the concrete
        // future type so the trait bound resolves.
        type ResolverFuture = std::pin::Pin<
            Box<dyn std::future::Future<Output = genai::resolver::Result<Option<AuthData>>> + Send>,
        >;
        let resolver_fn = move |_iden: ModelIden| -> ResolverFuture {
            let broker = Arc::clone(&broker_for_resolver);
            let provider_id = Arc::clone(&provider_id);
            let scope = Arc::clone(&scope);
            Box::pin(async move {
                let issued = broker.token_for(&provider_id, &scope).await.map_err(|e| {
                    genai::resolver::Error::Custom(format!(
                        "credential broker error for provider '{provider_id}': {e}"
                    ))
                })?;
                Ok(Some(AuthData::from_single(issued.bearer().to_owned())))
            })
        };
        builder = builder.with_auth_resolver(
            genai::resolver::AuthResolver::from_resolver_async_fn(resolver_fn),
        );
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

/// Default OAuth scope used when the provider does not list any in
/// `adapter_options.scopes`. `cloud-platform` covers Vertex AI's needs.
const DEFAULT_OAUTH_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// Read `adapter_options.scopes` (string array) and join with spaces, the
/// form Google's OAuth endpoint accepts. Returns the default scope when
/// the field is absent.
fn scopes_from_options(options: &BTreeMap<String, Value>) -> Result<String, ConfigRuntimeError> {
    let Some(value) = options.get("scopes") else {
        return Ok(DEFAULT_OAUTH_SCOPE.to_owned());
    };
    let arr = value.as_array().ok_or_else(|| {
        ConfigRuntimeError::InvalidConfig(
            "adapter_options.scopes must be an array of strings".into(),
        )
    })?;
    if arr.is_empty() {
        return Ok(DEFAULT_OAUTH_SCOPE.to_owned());
    }
    let mut joined = String::new();
    for (i, item) in arr.iter().enumerate() {
        let s = item.as_str().ok_or_else(|| {
            ConfigRuntimeError::InvalidConfig(
                "adapter_options.scopes must be an array of strings".into(),
            )
        })?;
        if i > 0 {
            joined.push(' ');
        }
        joined.push_str(s);
    }
    Ok(joined)
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

/// Probe-style candidate list for adapter discovery.
///
/// Each entry is a lowercase adapter name we *want* to expose if genai
/// recognises it. Authoritative validation happens via
/// [`AdapterKind::from_lower_str`]: unknown candidates are silently filtered
/// out, so adding a forward-looking name here is safe even before genai
/// ships support — the entry becomes a no-op.
///
/// To pick up a brand-new genai adapter:
/// 1. Append its lowercase name to `ADAPTER_CANDIDATES`.
/// 2. The runtime auto-discovers it through `AdapterKind::from_lower_str`
///    — no enum import or match-arm change needed.
///
/// Forward-looking entries are speculative names common LLM providers go by
/// (e.g. `bedrock`, `azure`). They cost nothing today and auto-light-up the
/// moment genai adopts them.
const ADAPTER_CANDIDATES: &[&str] = &[
    // Currently shipping in upstream genai 0.6
    "anthropic",
    "openai",
    "openai_resp",
    "deepseek",
    "gemini",
    "ollama",
    "ollama_cloud",
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
    "vertex",
    "github_copilot",
    // Forward-looking — no-op until genai recognises them
    "bedrock",
    "azure",
    "azure_openai",
    "mistral",
    "perplexity",
    "watsonx",
    "huggingface",
    "replicate",
];

/// Canonical list of provider adapter identifiers supported by the runtime.
///
/// Computed by probing each candidate name through
/// [`AdapterKind::from_lower_str`], so the result reflects whatever the
/// linked genai version actually supports — not a hand-maintained snapshot.
pub fn supported_adapters() -> Vec<&'static str> {
    ADAPTER_CANDIDATES
        .iter()
        .copied()
        .filter(|name| AdapterKind::from_lower_str(name).is_some())
        .collect()
}

fn parse_adapter_kind(adapter: &str) -> Result<AdapterKind, ConfigRuntimeError> {
    let normalized = adapter.trim().to_ascii_lowercase();
    // Awaken-specific aliases mapped before delegating to genai. These predate
    // the unified `from_lower_str` path and are kept for backwards compatibility.
    if matches!(normalized.as_str(), "openai-resp" | "responses") {
        return Ok(AdapterKind::OpenAIResp);
    }
    AdapterKind::from_lower_str(&normalized)
        .ok_or_else(|| ConfigRuntimeError::UnsupportedProviderAdapter(adapter.to_string()))
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
    let mut out = Vec::with_capacity(entries.len());
    for (_, value) in entries {
        let record: ConfigRecord<T> = ConfigRecord::from_value(value.clone())
            .map_err(|error| StorageError::Serialization(error.to_string()))
            .map_err(ConfigRuntimeError::Storage)?;
        if record.meta.hidden {
            continue;
        }
        out.push(record.spec);
    }
    Ok(out)
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

fn map_seed_error(error: crate::services::builtin_seed::SeedError) -> ConfigRuntimeError {
    use crate::services::builtin_seed::SeedError;
    match error {
        SeedError::Storage(e) => ConfigRuntimeError::Storage(e),
        SeedError::Serde(e) => {
            ConfigRuntimeError::Storage(StorageError::Serialization(e.to_string()))
        }
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

    /// Fresh per-call broker — equivalent to what the old
    /// no-broker `build_genai_provider_executor` constructed internally.
    /// Tests that don't care about broker state share-ability use this.
    fn test_broker() -> Arc<dyn awaken_runtime::credentials::CredentialBroker> {
        Arc::new(awaken_runtime::credentials::AwakenCredentialBroker::new())
    }

    #[test]
    fn build_genai_with_valid_headers_succeeds() {
        let mut options = BTreeMap::new();
        options.insert("headers".into(), json!({"OpenAI-Organization": "org-xyz"}));
        let spec = provider_spec_with_options(options);
        build_genai_provider_executor(&spec, test_broker()).expect("valid headers must build");
    }

    #[test]
    fn build_genai_rejects_non_object_headers() {
        let mut options = BTreeMap::new();
        options.insert("headers".into(), json!("not-an-object"));
        let spec = provider_spec_with_options(options);
        let err = match build_genai_provider_executor(&spec, test_broker()) {
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
        let err = match build_genai_provider_executor(&spec, test_broker()) {
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
        build_genai_provider_executor(&spec, test_broker())
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

    #[test]
    fn supported_adapters_round_trip_through_parser() {
        for name in supported_adapters() {
            let parsed = parse_adapter_kind(name)
                .unwrap_or_else(|err| panic!("supported adapter {name} must parse: {err:?}"));
            assert_eq!(
                parsed.as_lower_str(),
                name,
                "as_lower_str round-trip mismatch for {name}"
            );
        }
    }

    // -- credential broker integration tests ---------------------------------
    //
    // These tests exercise the path: ProviderSpec → build_material →
    // broker.register → executor build. They cover the eager-validation
    // contract (bad credentials_kind, bad SA JSON, missing api_key) and
    // backward compatibility (no api_key + bearer = silent fallback to
    // genai env vars).

    fn provider_spec_with_kind_and_key(
        adapter: &str,
        kind: Option<&str>,
        api_key: Option<&str>,
    ) -> ProviderSpec {
        let mut options: BTreeMap<String, Value> = BTreeMap::new();
        if let Some(k) = kind {
            options.insert("credentials_kind".into(), json!(k));
        }
        ProviderSpec {
            id: format!("test-{adapter}"),
            adapter: adapter.into(),
            api_key: api_key.map(|k| k.to_string().into()),
            adapter_options: options,
            ..ProviderSpec::default()
        }
    }

    #[test]
    fn supported_adapters_includes_recent_additions() {
        let names: std::collections::HashSet<&str> = supported_adapters().into_iter().collect();
        for required in ["vertex", "github_copilot", "ollama_cloud"] {
            assert!(
                names.contains(required),
                "expected adapter {required} to be exposed via supported_adapters()"
            );
        }
    }

    #[test]
    fn supported_adapters_filters_unknown_candidates() {
        // Forward-looking candidates that genai 0.6 does not yet recognise must
        // be dropped, not passed through as broken options to the admin UI.
        let names: std::collections::HashSet<&str> = supported_adapters().into_iter().collect();
        for speculative in ["bedrock", "azure", "azure_openai", "mistral", "perplexity"] {
            if AdapterKind::from_lower_str(speculative).is_none() {
                assert!(
                    !names.contains(speculative),
                    "speculative candidate {speculative} leaked into supported_adapters() despite genai not supporting it"
                );
            }
        }
    }

    #[test]
    fn unsupported_adapter_error_points_at_genai_docs() {
        let err = parse_adapter_kind("definitely-not-a-real-adapter").unwrap_err();
        let display = err.to_string();
        assert!(
            display.contains("definitely-not-a-real-adapter"),
            "error must echo the offending name, got: {display}"
        );
        assert!(
            display.contains("docs.rs/genai"),
            "error must point operators at genai's AdapterKind docs, got: {display}"
        );
    }

    #[test]
    fn build_genai_executor_for_every_supported_adapter() {
        // Stronger than the parse round-trip: every adapter exposed via
        // supported_adapters() must successfully construct an LlmExecutor end
        // to end (parse → AdapterKind → genai builder chain → wrapper). No
        // network calls happen at build time, so this is safe and offline.
        for name in supported_adapters() {
            let spec = ProviderSpec {
                id: format!("test-{name}"),
                adapter: name.to_string(),
                ..ProviderSpec::default()
            };
            build_genai_provider_executor(&spec, test_broker()).unwrap_or_else(|err| {
                panic!("supported adapter `{name}` failed to build executor: {err:?}")
            });
        }
    }

    #[test]
    fn build_genai_executor_with_api_key_for_every_supported_adapter() {
        // Same as above but exercising the auth_resolver path (api_key set).
        // Catches any adapter that rejects a static key at builder time.
        for name in supported_adapters() {
            let spec = ProviderSpec {
                id: format!("test-{name}"),
                adapter: name.to_string(),
                api_key: Some("test-secret-key".to_string().into()),
                ..ProviderSpec::default()
            };
            build_genai_provider_executor(&spec, test_broker()).unwrap_or_else(|err| {
                panic!("supported adapter `{name}` (with api_key) failed to build: {err:?}")
            });
        }
    }

    #[test]
    fn build_genai_executor_with_base_url_override_for_every_supported_adapter() {
        // Cover the third resolver path: base_url override (proxy / self-hosted).
        // Use a syntactically valid URL — genai validates the form at build time.
        for name in supported_adapters() {
            let spec = ProviderSpec {
                id: format!("test-{name}"),
                adapter: name.to_string(),
                base_url: Some("https://example.invalid/v1".to_string()),
                ..ProviderSpec::default()
            };
            build_genai_provider_executor(&spec, test_broker()).unwrap_or_else(|err| {
                panic!("supported adapter `{name}` (with base_url) failed to build: {err:?}")
            });
        }
    }

    #[test]
    fn build_genai_executor_with_full_options_for_every_supported_adapter() {
        // All resolver paths simultaneously: api_key + base_url + headers +
        // an unknown forward-compat option key. Every adapter must accept the
        // combination without breaking the builder chain.
        for name in supported_adapters() {
            let mut adapter_options = BTreeMap::new();
            adapter_options.insert(
                "headers".into(),
                json!({ "X-Awaken-Trace": "regression-test" }),
            );
            adapter_options.insert("future_extension_key".into(), json!({ "ignored": true }));
            let spec = ProviderSpec {
                id: format!("test-{name}"),
                adapter: name.to_string(),
                api_key: Some("test-secret-key".to_string().into()),
                base_url: Some("https://example.invalid/v1".to_string()),
                timeout_secs: 60,
                adapter_options,
                ..ProviderSpec::default()
            };
            build_genai_provider_executor(&spec, test_broker()).unwrap_or_else(|err| {
                panic!("supported adapter `{name}` (full options) failed to build: {err:?}")
            });
        }
    }

    #[test]
    fn build_genai_executor_clamps_zero_timeout_for_every_supported_adapter() {
        // Boundary: timeout_secs = 0 must be clamped to >=1 instead of producing
        // a Duration that breaks the executor. Same protection for every adapter.
        for name in supported_adapters() {
            let spec = ProviderSpec {
                id: format!("test-{name}"),
                adapter: name.to_string(),
                timeout_secs: 0,
                ..ProviderSpec::default()
            };
            build_genai_provider_executor(&spec, test_broker()).unwrap_or_else(|err| {
                panic!("supported adapter `{name}` (zero timeout) failed to build: {err:?}")
            });
        }
    }

    #[test]
    fn parse_adapter_kind_is_case_insensitive_for_every_supported_adapter() {
        // Every supported adapter must parse regardless of casing variations.
        // Guards against silent regressions in the to_ascii_lowercase normalisation.
        for name in supported_adapters() {
            let upper = name.to_ascii_uppercase();
            let mixed: String = name
                .chars()
                .enumerate()
                .map(|(i, c)| {
                    if i % 2 == 0 {
                        c.to_ascii_uppercase()
                    } else {
                        c
                    }
                })
                .collect();
            for variant in [name.to_string(), upper, mixed, format!("  {name}  ")] {
                parse_adapter_kind(&variant).unwrap_or_else(|err| {
                    panic!("`{variant}` (canonical: {name}) failed to parse: {err:?}")
                });
            }
        }
    }

    #[test]
    fn supported_adapters_unique_no_duplicate_names() {
        // Detect copy-paste mistakes in ADAPTER_CANDIDATES that would silently
        // duplicate an entry in the admin UI dropdown.
        let names: Vec<&'static str> = supported_adapters();
        let mut seen = std::collections::HashSet::with_capacity(names.len());
        for name in &names {
            assert!(
                seen.insert(*name),
                "duplicate entry `{name}` in supported_adapters()"
            );
        }
        // Sanity floor: PR opens with 19 known adapters, never expect to ship
        // fewer (would mean we lost coverage of an upstream-supported adapter).
        assert!(
            names.len() >= 19,
            "supported_adapters() shrank below floor of 19 (got {}): {names:?}",
            names.len()
        );
    }

    #[test]
    fn vertex_anthropic_namespaces_parse_when_routed_through_adapter_string() {
        // Vertex routes Gemini and Claude via the same adapter string; only
        // genai's namespace routing inside `from_model` discriminates publishers.
        // Ensure the adapter-name level still resolves to AdapterKind::Vertex
        // regardless of which model the caller eventually sends.
        let kind = parse_adapter_kind("vertex").expect("vertex must parse");
        assert_eq!(kind, AdapterKind::Vertex);
        // GitHub Copilot is the same shape: one adapter, multi-publisher routing.
        let kind = parse_adapter_kind("github_copilot").expect("github_copilot must parse");
        assert_eq!(kind, AdapterKind::GithubCopilot);
        // Ollama Cloud must not collide with vanilla Ollama.
        let cloud = parse_adapter_kind("ollama_cloud").expect("ollama_cloud must parse");
        let local = parse_adapter_kind("ollama").expect("ollama must parse");
        assert_ne!(
            cloud, local,
            "ollama_cloud and ollama must map to distinct kinds"
        );
        assert_eq!(cloud, AdapterKind::OllamaCloud);
        assert_eq!(local, AdapterKind::Ollama);
    }

    #[test]
    fn parse_adapter_kind_accepts_legacy_aliases() {
        assert_eq!(
            parse_adapter_kind("openai-resp").unwrap(),
            AdapterKind::OpenAIResp
        );
        assert_eq!(
            parse_adapter_kind("responses").unwrap(),
            AdapterKind::OpenAIResp
        );
        assert_eq!(
            parse_adapter_kind("  Anthropic ").unwrap(),
            AdapterKind::Anthropic
        );
    }

    #[test]
    fn build_genai_omitted_api_key_falls_back_to_env_default() {
        // 0.4.0 behaviour: a provider with no api_key should still build —
        // genai's adapter will read VENDOR_API_KEY at request time. The
        // broker integration must not break this.
        let spec = provider_spec_with_kind_and_key("openai", None, None);
        build_genai_provider_executor(&spec, test_broker())
            .expect("env-fallback bearer must build");
    }

    #[test]
    fn build_genai_explicit_bearer_succeeds() {
        let spec = provider_spec_with_kind_and_key("openai", Some("bearer"), Some("sk-test-123"));
        build_genai_provider_executor(&spec, test_broker()).expect("explicit bearer must build");
    }

    #[test]
    fn build_genai_unknown_credentials_kind_rejected_with_clear_error() {
        let spec = provider_spec_with_kind_and_key(
            "openai",
            Some("never-heard-of-it"),
            Some("sk-test-123"),
        );
        let err = build_genai_provider_executor(&spec, test_broker())
            .err()
            .expect("expected error");
        assert!(
            matches!(err, ConfigRuntimeError::InvalidConfig(ref m) if m.contains("never-heard-of-it")),
            "expected InvalidConfig naming the bad kind, got: {err:?}"
        );
    }

    #[test]
    fn build_genai_service_account_kind_with_non_vertex_adapter_rejected() {
        let spec = provider_spec_with_kind_and_key(
            "openai",
            Some("service_account_json"),
            Some(r#"{"client_email":"x@y","private_key":"-----BEGIN PRIVATE KEY-----"}"#),
        );
        let err = build_genai_provider_executor(&spec, test_broker())
            .err()
            .expect("expected error");
        assert!(
            matches!(err, ConfigRuntimeError::InvalidConfig(ref m)
                if m.contains("service_account_json") && m.contains("vertex") && m.contains("openai")),
            "expected InvalidConfig naming the kind/adapter mismatch, got: {err:?}"
        );
    }

    // Note: deeper service_account_json shape tests live in
    // `awaken_runtime::credentials::material::tests` so they don't depend
    // on `parse_adapter_kind` recognising the "vertex" adapter (which is
    // gated on the upstream genai version actually shipping it).

    /// Broker double that records every `register` / `deregister` call so
    /// tests can assert what was (or wasn't) handed to the broker.
    #[derive(Default)]
    struct RecordingBroker {
        registered: parking_lot::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl awaken_runtime::credentials::CredentialBroker for RecordingBroker {
        fn register(
            &self,
            provider_id: String,
            _material: awaken_runtime::credentials::CredentialMaterial,
        ) {
            self.registered.lock().push(provider_id);
        }
        async fn token_for(
            &self,
            _provider_id: &str,
            _scope: &str,
        ) -> Result<
            awaken_runtime::credentials::IssuedToken,
            awaken_runtime::credentials::CredentialError,
        > {
            unreachable!("static-bearer build must not call token_for");
        }
    }

    #[test]
    fn build_genai_static_bearer_does_not_register_with_broker() {
        // Item 2: static bearers go straight into genai's sync resolver,
        // bypassing the broker. If we ever regress and register them, the
        // broker would unnecessarily cache, single-flight, and lock on
        // every chat call.
        let recording: Arc<RecordingBroker> = Arc::new(RecordingBroker::default());
        let broker: Arc<dyn awaken_runtime::credentials::CredentialBroker> =
            Arc::clone(&recording) as _;

        let spec = provider_spec_with_kind_and_key("openai", Some("bearer"), Some("sk-x"));
        build_genai_provider_executor(&spec, broker).expect("static bearer must build");

        assert!(
            recording.registered.lock().is_empty(),
            "static bearer must not register with the broker"
        );
    }

    #[test]
    fn build_genai_omitted_api_key_does_not_register_with_broker() {
        // Env-var fallback — same expectation as the static bearer case:
        // there's no material to register and no token to mint.
        let recording: Arc<RecordingBroker> = Arc::new(RecordingBroker::default());
        let broker: Arc<dyn awaken_runtime::credentials::CredentialBroker> =
            Arc::clone(&recording) as _;

        let spec = provider_spec_with_kind_and_key("openai", None, None);
        build_genai_provider_executor(&spec, broker).expect("env-fallback must build");

        assert!(
            recording.registered.lock().is_empty(),
            "env-fallback must not register with the broker"
        );
    }

    #[test]
    fn scopes_from_options_default_when_absent() {
        assert_eq!(
            scopes_from_options(&BTreeMap::new()).unwrap(),
            DEFAULT_OAUTH_SCOPE
        );
    }

    #[test]
    fn scopes_from_options_joins_array_with_spaces() {
        let mut options = BTreeMap::new();
        options.insert(
            "scopes".into(),
            json!(["a.googleapis.com/auth/x", "b.googleapis.com/auth/y"]),
        );
        assert_eq!(
            scopes_from_options(&options).unwrap(),
            "a.googleapis.com/auth/x b.googleapis.com/auth/y"
        );
    }

    #[test]
    fn scopes_from_options_rejects_non_array() {
        let mut options = BTreeMap::new();
        options.insert("scopes".into(), json!("not-an-array"));
        let err = scopes_from_options(&options).unwrap_err();
        assert!(matches!(err, ConfigRuntimeError::InvalidConfig(ref m) if m.contains("scopes")));
    }

    #[test]
    fn scopes_from_options_rejects_non_string_entry() {
        let mut options = BTreeMap::new();
        options.insert("scopes".into(), json!([42]));
        let err = scopes_from_options(&options).unwrap_err();
        assert!(matches!(err, ConfigRuntimeError::InvalidConfig(ref m) if m.contains("scopes")));
    }

    #[test]
    fn parse_adapter_kind_rejects_unknown() {
        let err = parse_adapter_kind("not-a-real-adapter").unwrap_err();
        assert!(
            matches!(err, ConfigRuntimeError::UnsupportedProviderAdapter(ref s) if s == "not-a-real-adapter"),
            "expected UnsupportedProviderAdapter, got: {err:?}"
        );
    }

    fn minimal_agent_spec(id: &str) -> AgentSpec {
        AgentSpec {
            id: id.into(),
            model_id: "test-model".into(),
            system_prompt: "test prompt".into(),
            max_rounds: 1,
            ..Default::default()
        }
    }

    #[test]
    fn deserialize_namespace_decodes_legacy_bare_spec() {
        let spec = minimal_agent_spec("agent-a");
        let value = serde_json::to_value(&spec).expect("serialization must succeed");
        let entries = vec![("agent-a".to_string(), value)];
        let result: Vec<AgentSpec> =
            deserialize_namespace(&entries).expect("legacy bare spec must decode");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "agent-a");
    }

    #[test]
    fn deserialize_namespace_decodes_envelope() {
        use awaken_contract::ConfigRecord;
        let spec = minimal_agent_spec("agent-b");
        let record = ConfigRecord {
            spec,
            meta: awaken_contract::RecordMeta::new_user(),
        };
        let value = record
            .to_value()
            .expect("envelope serialization must succeed");
        let entries = vec![("agent-b".to_string(), value)];
        let result: Vec<AgentSpec> = deserialize_namespace(&entries).expect("envelope must decode");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "agent-b");
    }

    #[test]
    fn deserialize_namespace_skips_hidden_envelope() {
        use awaken_contract::{ConfigRecord, RecordMeta};
        let visible = minimal_agent_spec("visible");
        let hidden = minimal_agent_spec("hidden");

        let mut hidden_meta = RecordMeta::new_user();
        hidden_meta.hidden = true;

        let visible_record = ConfigRecord {
            spec: visible,
            meta: RecordMeta::new_user(),
        };
        let hidden_record = ConfigRecord {
            spec: hidden,
            meta: hidden_meta,
        };

        let entries = vec![
            (
                "visible".to_string(),
                visible_record.to_value().expect("serialize visible"),
            ),
            (
                "hidden".to_string(),
                hidden_record.to_value().expect("serialize hidden"),
            ),
        ];
        let result: Vec<AgentSpec> = deserialize_namespace(&entries).expect("decode must succeed");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "visible");
    }

    #[test]
    fn deserialize_namespace_mixes_legacy_and_envelope() {
        use awaken_contract::ConfigRecord;
        let bare_spec = minimal_agent_spec("bare");
        let envelope_spec = minimal_agent_spec("envelope");

        let bare_value = serde_json::to_value(&bare_spec).expect("serialize bare");
        let envelope_record = ConfigRecord {
            spec: envelope_spec,
            meta: awaken_contract::RecordMeta::new_user(),
        };
        let envelope_value = envelope_record.to_value().expect("serialize envelope");

        let entries = vec![
            ("bare".to_string(), bare_value),
            ("envelope".to_string(), envelope_value),
        ];
        let result: Vec<AgentSpec> =
            deserialize_namespace(&entries).expect("mixed decode must succeed");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, "bare");
        assert_eq!(result[1].id, "envelope");
    }

    #[test]
    fn deserialize_namespace_propagates_decode_error() {
        let bad_value = json!({"completely": "wrong"});
        let entries = vec![("bad".to_string(), bad_value)];
        let err = deserialize_namespace::<AgentSpec>(&entries)
            .expect_err("invalid spec must produce an error");
        assert!(
            matches!(
                err,
                ConfigRuntimeError::Storage(StorageError::Serialization(_))
            ),
            "expected Storage(Serialization(_)), got: {err:?}"
        );
    }

    /// Replaces the former `bootstrap_if_empty` test.  Asserts that
    /// `apply_seed` stores each spec as a ConfigRecord envelope whose
    /// `meta.source` is `RecordSource::Builtin { binary_version }`.
    #[tokio::test]
    async fn apply_seed_writes_builtin_envelope() {
        use awaken_contract::{
            BuiltinSeedSet, BuiltinSpec, ConfigRecord, ModelBindingSpec, ProviderSpec, RecordSource,
        };

        let bin_version = "test-env-ver".to_owned();
        let (manager, store) = make_manager_with_store().await;

        let seed = BuiltinSeedSet {
            binary_version: bin_version.clone(),
            specs: vec![
                BuiltinSpec::Provider(ProviderSpec {
                    id: "p1".into(),
                    adapter: "openai".into(),
                    ..Default::default()
                }),
                BuiltinSpec::Model(ModelBindingSpec {
                    id: "m1".into(),
                    provider_id: "p1".into(),
                    upstream_model: "m1-model".into(),
                    created_at: None,
                    updated_at: None,
                }),
                BuiltinSpec::Agent(Box::new(AgentSpec {
                    id: "a1".into(),
                    model_id: "m1".into(),
                    system_prompt: "seed test".into(),
                    max_rounds: 1,
                    ..Default::default()
                })),
            ],
        };

        let report = manager.apply_seed(&seed).await.expect("apply_seed");
        assert_eq!(report.created.len(), 3, "all three specs must be created");

        // Verify provider envelope and Builtin source.
        let raw_p = awaken_contract::contract::config_store::ConfigStore::get(
            store.as_ref(),
            "providers",
            "p1",
        )
        .await
        .expect("get provider")
        .expect("provider present");

        let p_obj = raw_p.as_object().expect("must be object");
        assert!(p_obj.contains_key("spec"), "provider must have 'spec' key");
        assert!(p_obj.contains_key("meta"), "provider must have 'meta' key");
        let p_rec: ConfigRecord<serde_json::Value> = ConfigRecord::from_value(raw_p).unwrap();
        assert_eq!(
            p_rec.meta.source,
            RecordSource::Builtin {
                binary_version: bin_version.clone()
            },
            "provider source must be Builtin with correct binary_version"
        );

        // Verify agent envelope.
        let raw_a = awaken_contract::contract::config_store::ConfigStore::get(
            store.as_ref(),
            "agents",
            "a1",
        )
        .await
        .expect("get agent")
        .expect("agent present");
        let a_rec: ConfigRecord<serde_json::Value> = ConfigRecord::from_value(raw_a).unwrap();
        assert_eq!(
            a_rec.meta.source,
            RecordSource::Builtin {
                binary_version: bin_version.clone()
            },
            "agent source must be Builtin"
        );

        // Verify model envelope.
        let raw_m = awaken_contract::contract::config_store::ConfigStore::get(
            store.as_ref(),
            "models",
            "m1",
        )
        .await
        .expect("get model")
        .expect("model present");
        let m_rec: ConfigRecord<serde_json::Value> = ConfigRecord::from_value(raw_m).unwrap();
        assert_eq!(
            m_rec.meta.source,
            RecordSource::Builtin {
                binary_version: bin_version
            },
            "model source must be Builtin"
        );
    }

    // ── apply_seed tests ─────────────────────────────────────────────────────

    /// Build a minimal ConfigRuntimeManager backed by an InMemoryStore.
    /// Mirrors the pattern used by `bootstrap_writes_builtin_envelope`.
    async fn make_manager_with_store() -> (
        ConfigRuntimeManager,
        Arc<dyn awaken_contract::contract::config_store::ConfigStore>,
    ) {
        use awaken_contract::contract::executor::{
            InferenceExecutionError, InferenceRequest, LlmExecutor,
        };
        use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
        use awaken_stores::InMemoryStore;

        struct Stub;
        #[async_trait::async_trait]
        impl LlmExecutor for Stub {
            async fn execute(
                &self,
                _: InferenceRequest,
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
                "stub"
            }
        }

        let store = Arc::new(InMemoryStore::new())
            as Arc<dyn awaken_contract::contract::config_store::ConfigStore>;
        let thread_store = Arc::new(InMemoryStore::new());
        let runtime = Arc::new(
            awaken_runtime::builder::AgentRuntimeBuilder::new()
                .with_provider("boot", Arc::new(Stub))
                .with_model_binding(
                    "boot",
                    awaken_runtime::registry::traits::ModelBinding {
                        provider_id: "boot".into(),
                        upstream_model: "boot-model".into(),
                    },
                )
                .with_agent_spec(AgentSpec {
                    id: "boot".into(),
                    model_id: "boot".into(),
                    system_prompt: "boot".into(),
                    max_rounds: 1,
                    ..Default::default()
                })
                .with_thread_run_store(thread_store)
                .build()
                .expect("build runtime"),
        );
        let manager = ConfigRuntimeManager::new(runtime, store.clone()).expect("manager");
        (manager, store)
    }

    #[tokio::test]
    async fn apply_seed_writes_builtin_records_to_store() {
        use awaken_contract::{
            BuiltinSeedSet, BuiltinSpec, ConfigRecord, ModelBindingSpec, ProviderSpec, RecordSource,
        };

        let (manager, store) = make_manager_with_store().await;

        let seed = BuiltinSeedSet {
            binary_version: "v1-test".to_owned(),
            specs: vec![
                BuiltinSpec::Agent(Box::new(AgentSpec {
                    id: "seed-agent".into(),
                    model_id: "m".into(),
                    system_prompt: "hello".into(),
                    max_rounds: 1,
                    ..Default::default()
                })),
                BuiltinSpec::Provider(ProviderSpec {
                    id: "seed-provider".into(),
                    adapter: "openai".into(),
                    ..Default::default()
                }),
                BuiltinSpec::Model(ModelBindingSpec {
                    id: "seed-model".into(),
                    provider_id: "seed-provider".into(),
                    upstream_model: "gpt-4o".into(),
                    created_at: None,
                    updated_at: None,
                }),
            ],
        };

        let report = manager.apply_seed(&seed).await.expect("apply_seed");
        assert_eq!(report.created.len(), 3, "expected 3 created");
        assert!(report.updated.is_empty());
        assert!(report.unchanged.is_empty());

        // Verify agent record stored with Builtin source and correct version.
        let raw = awaken_contract::contract::config_store::ConfigStore::get(
            store.as_ref(),
            "agents",
            "seed-agent",
        )
        .await
        .expect("get agent")
        .expect("agent must be present");

        let rec: ConfigRecord<serde_json::Value> = ConfigRecord::from_value(raw).unwrap();
        assert_eq!(
            rec.meta.source,
            RecordSource::Builtin {
                binary_version: "v1-test".to_owned()
            },
            "source must be Builtin with seed binary_version"
        );
    }

    #[tokio::test]
    async fn apply_seed_idempotent() {
        use awaken_contract::{BuiltinSeedSet, BuiltinSpec, ModelBindingSpec, ProviderSpec};

        let (manager, _store) = make_manager_with_store().await;

        let seed = BuiltinSeedSet {
            binary_version: "v1-idem".to_owned(),
            specs: vec![
                BuiltinSpec::Agent(Box::new(AgentSpec {
                    id: "idem-agent".into(),
                    model_id: "m".into(),
                    system_prompt: "hello".into(),
                    max_rounds: 1,
                    ..Default::default()
                })),
                BuiltinSpec::Provider(ProviderSpec {
                    id: "idem-provider".into(),
                    adapter: "openai".into(),
                    ..Default::default()
                }),
                BuiltinSpec::Model(ModelBindingSpec {
                    id: "idem-model".into(),
                    provider_id: "idem-provider".into(),
                    upstream_model: "gpt-4o".into(),
                    created_at: None,
                    updated_at: None,
                }),
            ],
        };

        manager.apply_seed(&seed).await.expect("first apply_seed");
        let report = manager.apply_seed(&seed).await.expect("second apply_seed");

        assert_eq!(
            report.unchanged.len(),
            3,
            "second call must report 3 unchanged"
        );
        assert!(report.created.is_empty());
        assert!(report.updated.is_empty());
    }

    /// Verify that `apply_seed` holds `lock_apply` for its duration, so a
    /// concurrent `apply()` blocks until the seed write completes.
    ///
    /// Strategy: acquire the lock manually, spawn `apply_seed` in a task, then
    /// release the lock and confirm the task finishes cleanly.  This asserts
    /// the lock is actually contended (the task cannot proceed while we hold it).
    #[tokio::test]
    async fn apply_seed_serializes_with_apply_lock() {
        use awaken_contract::{BuiltinSeedSet, BuiltinSpec, ProviderSpec};
        use std::sync::Arc;

        let (manager, _store) = make_manager_with_store().await;
        let manager = Arc::new(manager);

        // Hold the apply-lock ourselves to block apply_seed.
        let guard = manager.lock_apply().await;

        let manager2 = Arc::clone(&manager);
        let seed = BuiltinSeedSet {
            binary_version: "lock-test".to_owned(),
            specs: vec![BuiltinSpec::Provider(ProviderSpec {
                id: "lock-prov".into(),
                adapter: "openai".into(),
                ..Default::default()
            })],
        };

        let handle = tokio::spawn(async move {
            manager2
                .apply_seed(&seed)
                .await
                .expect("apply_seed in task")
        });

        // Give the spawned task a moment to reach lock acquisition and block.
        tokio::task::yield_now().await;
        assert!(
            !handle.is_finished(),
            "apply_seed must block while apply-lock is held"
        );

        // Release the lock; the task should now be able to complete.
        drop(guard);
        let report = handle.await.expect("task must not panic");
        assert_eq!(
            report.created.len(),
            1,
            "seed record must be created after lock release"
        );
    }
}
