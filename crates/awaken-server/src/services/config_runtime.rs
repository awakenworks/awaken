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
    AgentSpec, ConfigRecord, McpRestartPolicy, McpServerSpec, McpTransportKind, ModelSpec,
    PeriodicRefresher, ProviderSpec, SkillSpecSink,
};
use awaken_ext_mcp::{
    DefaultSamplingHandler, McpServerConnectionConfig, McpServerStatusSnapshot, McpToolRegistry,
    McpToolRegistryManager, SamplingHandler, SamplingHandlerFactory,
};
use awaken_runtime::AgentRuntime;
use awaken_runtime::engine::GenaiExecutor;
use awaken_runtime::registry::BackendRegistry;
use awaken_runtime::registry::{AgentSpecRegistry, PluginSource, ToolRegistry};
use genai::adapter::AdapterKind;
use genai::resolver::{AuthData, Endpoint};
use genai::{Client, ModelIden, ServiceTarget, WebConfig};
use parking_lot::{Mutex, RwLock};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

mod managed_config;
mod provider_capability_discovery;
mod publish;
mod registry_compile;
mod skill_publish;
#[cfg(test)]
mod skill_tests;
mod versioned_publish;

use managed_config::ManagedConfigSnapshot;

const CONFIG_LOAD_PAGE_SIZE: usize = 1024;

const NS_AGENTS: &str = "agents";
const NS_MODELS: &str = "models";
const NS_PROVIDERS: &str = "providers";
const NS_MCP_SERVERS: &str = "mcp-servers";
const NS_TOOLS: &str = "tools";
const NS_SKILLS: &str = "skills";

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
    #[error("versioned registry error: {0}")]
    VersionedRegistry(String),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

/// Holds A2A-discovered agent specs (those with `endpoint` or `registry`
/// set). Built once from the runtime's pre-apply registry; subsequent
/// `ConfigRuntimeManager::apply()` calls overlay these on top of the
/// ConfigStore-derived registry. Pure code-defined regular agents flow
/// only via ConfigStore and do NOT appear here.
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
/// `ServerState`-scoped broker via [`with_broker`](Self::with_broker).
pub struct GenaiProviderExecutorFactory;

/// Provider executor factory backed by genai and a caller-supplied credential broker.
pub struct BrokeredGenaiProviderExecutorFactory {
    broker: Arc<dyn awaken_runtime::credentials::CredentialBroker>,
}

impl Default for GenaiProviderExecutorFactory {
    fn default() -> Self {
        Self
    }
}

impl GenaiProviderExecutorFactory {
    /// Construct a factory bound to the given broker. The broker is
    /// shared across all executors this factory builds, which is what
    /// production wiring wants.
    pub fn with_broker(
        broker: Arc<dyn awaken_runtime::credentials::CredentialBroker>,
    ) -> BrokeredGenaiProviderExecutorFactory {
        BrokeredGenaiProviderExecutorFactory { broker }
    }
}

impl ProviderExecutorFactory for GenaiProviderExecutorFactory {
    fn build(&self, spec: &ProviderSpec) -> Result<Arc<dyn LlmExecutor>, ConfigRuntimeError> {
        build_genai_provider_executor(spec)
    }
}

impl ProviderExecutorFactory for BrokeredGenaiProviderExecutorFactory {
    fn build(&self, spec: &ProviderSpec) -> Result<Arc<dyn LlmExecutor>, ConfigRuntimeError> {
        build_genai_provider_executor_with_broker(spec, Arc::clone(&self.broker))
    }
}

#[async_trait]
pub trait ManagedMcpRegistry: Send + Sync {
    fn tool_registry(&self) -> Arc<dyn ToolRegistry>;
    fn periodic_refresh_running(&self) -> bool;
    fn start_periodic_refresh(&self, interval: Duration) -> Result<(), ConfigRuntimeError>;
    async fn stop_periodic_refresh(&self) -> bool;
    async fn close(&self) -> Result<(), ConfigRuntimeError> {
        self.stop_periodic_refresh().await;
        Ok(())
    }
    /// Async so the implementation can pull the live HTTP session id
    /// from the transport — without this, a silent session-id rotation
    /// (after `MCP session expired` triggers a fresh `initialize`)
    /// wouldn't surface to admin/observability consumers.
    async fn server_status(&self, _server_name: &str) -> Option<McpServerStatusSnapshot> {
        None
    }
    async fn reconnect(&self, server_name: &str) -> Result<(), ConfigRuntimeError> {
        Err(ConfigRuntimeError::InvalidConfig(format!(
            "MCP registry does not support reconnect for server '{server_name}'"
        )))
    }
}

#[async_trait]
pub trait McpRegistryFactory: Send + Sync {
    async fn connect(
        &self,
        specs: &[McpServerSpec],
    ) -> Result<Option<Arc<dyn ManagedMcpRegistry>>, ConfigRuntimeError>;
}

/// Resolves a per-agent [`SamplingHandler`] by walking the live runtime
/// registry: `agent.model_id` → `ModelSpec` → provider →
/// `LlmExecutor` → [`DefaultSamplingHandler`].
///
/// Wired into [`DefaultMcpRegistryFactory`] so server-initiated
/// `sampling/createMessage` requests during an agent's MCP tool call
/// route to the **same** executor that agent uses for its own inference,
/// not a fixed registry-level handler. Closes the multi-agent sampling
/// leak documented in the MCP audit.
///
/// Holds a `Weak<AgentRuntime>` so the factory doesn't extend the
/// runtime's lifetime. On `for_agent` we upgrade — if the runtime is
/// already torn down, return `None` and let the transport's fallback
/// handler (also typically `None` at registry construction) decide.
/// Cache state held by [`RegistryDrivenSamplingHandlerFactory`].
///
/// `version` is the [`AgentRuntime::registry_version`] under which the
/// `entries` were built. Each publish bumps the runtime's version (see
/// `replace_registry_set`), so a version mismatch on lookup means the
/// underlying `ModelSpec` / provider / `LlmExecutor` may have
/// changed — wipe the cache before serving. Without this, a published
/// config that changes (say) a model's `upstream_model` while leaving
/// the agent spec's `model_id` intact would keep routing sampling
/// requests to the previous executor, silently.
struct SamplingFactoryCacheState {
    version: u64,
    entries: HashMap<(String, String), Arc<dyn SamplingHandler>>,
}

impl SamplingFactoryCacheState {
    fn empty() -> Self {
        Self {
            version: 0,
            entries: HashMap::new(),
        }
    }
}

pub(crate) struct RegistryDrivenSamplingHandlerFactory {
    runtime: Weak<AgentRuntime>,
    cache: std::sync::Mutex<SamplingFactoryCacheState>,
}

impl RegistryDrivenSamplingHandlerFactory {
    pub(crate) fn new(runtime: Weak<AgentRuntime>) -> Self {
        Self {
            runtime,
            cache: std::sync::Mutex::new(SamplingFactoryCacheState::empty()),
        }
    }
}

#[async_trait]
impl SamplingHandlerFactory for RegistryDrivenSamplingHandlerFactory {
    async fn for_agent(&self, agent_spec: &AgentSpec) -> Option<Arc<dyn SamplingHandler>> {
        let runtime = self.runtime.upgrade()?;
        // Atomic capture: `registry_snapshot()` returns
        // `(version, RegistrySet)` from a single observation. Resolving
        // the agent's model + provider via this snapshot and then
        // inserting under THIS version closes the race window where a
        // separate `registry_version()` + `registry_set()` pair could
        // straddle a `replace_registry_set` and cache a stale-built
        // handler under the new version.
        let snapshot = runtime.registry_snapshot()?;
        let version = snapshot.version();
        let key = (agent_spec.id.clone(), agent_spec.model_id.clone());

        // Fast path: cache hit at this snapshot's version.
        {
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            if cache.version == version
                && let Some(cached) = cache.entries.get(&key).cloned()
            {
                return Some(cached);
            }
        }

        let registries = snapshot.registries();
        let model = registries.models.get_model(&agent_spec.model_id)?;
        let executor = registries.providers.get_provider(&model.provider_id)?;
        let handler: Arc<dyn SamplingHandler> =
            Arc::new(DefaultSamplingHandler::new(executor, model.upstream_model));

        // Insert under the SNAPSHOT's version (the one the handler
        // was actually built from), not a freshly-read live version.
        // If the registry was replaced concurrently, the next call
        // will see version > cache.version and wipe — that's correct.
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        if cache.version != version {
            cache.entries.clear();
            cache.version = version;
        }
        cache.entries.insert(key, Arc::clone(&handler));
        Some(handler)
    }
}

/// Default MCP registry factory.
///
/// When `sampling_handler_factory` is set, the connect path threads it
/// to `McpToolRegistryManager::connect_with_sampling_factory` so the
/// per-call sampling routing kicks in. When unset (e.g. an awaken
/// deployment that opts out of sampling), MCP tool calls work normally
/// but `sampling/createMessage` requests from servers get rejected with
/// "method not supported" — the same legacy behaviour as before P1c.
pub struct DefaultMcpRegistryFactory {
    sampling_handler_factory: Option<Arc<dyn SamplingHandlerFactory>>,
}

impl DefaultMcpRegistryFactory {
    /// Default factory with no sampling support — preserves the
    /// pre-R1c behaviour. New code should prefer
    /// [`DefaultMcpRegistryFactory::with_runtime`].
    pub fn new() -> Self {
        Self {
            sampling_handler_factory: None,
        }
    }

    /// Factory wired to per-agent sampling: server-initiated
    /// `sampling/createMessage` during an agent's MCP tool call routes
    /// to that agent's `LlmExecutor` via
    /// [`RegistryDrivenSamplingHandlerFactory`].
    pub fn with_runtime(runtime: Weak<AgentRuntime>) -> Self {
        Self {
            sampling_handler_factory: Some(Arc::new(RegistryDrivenSamplingHandlerFactory::new(
                runtime,
            ))),
        }
    }
}

impl Default for DefaultMcpRegistryFactory {
    fn default() -> Self {
        Self::new()
    }
}

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

    async fn close(&self) -> Result<(), ConfigRuntimeError> {
        self.manager
            .close_all()
            .await
            .map_err(|error| ConfigRuntimeError::InvalidConfig(error.to_string()))
    }

    async fn server_status(&self, server_name: &str) -> Option<McpServerStatusSnapshot> {
        self.manager.server_status_snapshot(server_name).await.ok()
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
        // No fixed fallback handler — per-agent factory (when set) is
        // request-bound routing infrastructure only. It is not
        // advertised as global MCP sampling capability, and unattributed
        // server requests (stdio / HTTP GET listener) are rejected.
        let manager = McpToolRegistryManager::connect_with_sampling_factory(
            configs,
            None,
            self.sampling_handler_factory.clone(),
        )
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
        if let Some(active) = self.next_state
            && let Err(error) = active.handle.close().await
        {
            tracing::warn!(
                error = %error,
                "failed to close prepared MCP registry after publish failure"
            );
        }
    }
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
    skill_spec_sink: Option<Arc<dyn SkillSpecSink>>,
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
    versioned_registry: Option<versioned_publish::VersionedRegistryPublicationTarget>,
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

        // Wire the MCP registry factory with a Weak handle to this
        // runtime so per-agent sampling can resolve `agent.model_id` →
        // provider → `LlmExecutor` at each `tools/call`. `Weak` is
        // load-bearing: an `Arc` here would create a self-referential
        // cycle (runtime owns ConfigRuntimeManager → manager owns
        // mcp_factory → factory owns runtime) that leaks the runtime
        // for the process lifetime.
        let mcp_registry_factory: Arc<dyn McpRegistryFactory> = Arc::new(
            DefaultMcpRegistryFactory::with_runtime(Arc::downgrade(&runtime)),
        );

        Ok(Self {
            runtime,
            store,
            tools: registries.tools,
            plugins: registries.plugins,
            backends: registries.backends,
            skill_spec_sink: None,
            discovered_agents,
            provider_factory: Arc::new(GenaiProviderExecutorFactory),
            change_notifier: None,
            mcp_registry_factory,
            apply_lock: tokio::sync::Mutex::new(()),
            active_mcp_registry: Mutex::new(None),
            last_applied_fingerprint: RwLock::new(None),
            provider_executor_cache: Mutex::new(HashMap::new()),
            periodic_refresh: PeriodicRefresher::new(),
            change_listener: Mutex::new(None),
            mcp_refresh_interval: RwLock::new(None),
            min_apply_interval: Duration::ZERO,
            audit_log: None,
            versioned_registry: None,
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
    pub fn with_skill_spec_sink(mut self, sink: Arc<dyn SkillSpecSink>) -> Self {
        self.skill_spec_sink = Some(sink);
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

    /// Backward-compatible 0.4.0 bootstrap entry point.
    ///
    /// New code should prefer [`Self::apply_seed`], which preserves built-in
    /// provenance and supports field-level user overrides.
    pub async fn bootstrap_if_empty(
        &self,
        providers: &[ProviderSpec],
        models: &[ModelSpec],
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

        let specs = providers
            .iter()
            .cloned()
            .map(awaken_contract::BuiltinSpec::provider)
            .chain(
                models
                    .iter()
                    .cloned()
                    .map(awaken_contract::BuiltinSpec::model),
            )
            .chain(
                agents
                    .iter()
                    .cloned()
                    .map(awaken_contract::BuiltinSpec::agent),
            )
            .chain(
                mcp_servers
                    .iter()
                    .cloned()
                    .map(awaken_contract::BuiltinSpec::mcp_server),
            )
            .collect();

        let seed = awaken_contract::BuiltinSeedSet {
            binary_version: "bootstrap_if_empty".into(),
            specs,
        };
        self.apply_seed(&seed).await?;
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

    pub async fn shutdown(&self) -> Result<(), ConfigRuntimeError> {
        self.periodic_refresh.stop().await;
        self.stop_change_listener().await;
        let active = self.active_mcp_registry.lock().take();
        if let Some(active) = active {
            active.handle.close().await?;
        }
        Ok(())
    }

    pub fn periodic_refresh_running(&self) -> bool {
        self.periodic_refresh.is_running()
    }

    /// Return the live status snapshot for a managed MCP server.
    ///
    /// Returns `None` when no MCP registry is active (i.e. the runtime has no
    /// MCP servers configured) or the server name is unknown to the registry.
    ///
    /// Async so the snapshot includes the **live** HTTP session id rather
    /// than a value cached at connect/reconnect — `MCP session expired`
    /// rotations re-initialise silently and the cached value goes stale.
    pub async fn mcp_server_status(&self, server_name: &str) -> Option<McpServerStatusSnapshot> {
        let handle = {
            let guard = self.active_mcp_registry.lock();
            guard.as_ref().map(|active| Arc::clone(&active.handle))
        };
        match handle {
            Some(handle) => handle.server_status(server_name).await,
            None => None,
        }
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

    /// Snapshot the static tool registry into [`BuiltinSpec::Tool`] entries.
    ///
    /// Callers splice these into their `BuiltinSeedSet::specs` to make every
    /// registered tool a first-class config record (ADR-0029). Dynamic
    /// (MCP-sourced) tools are intentionally excluded — they are governed by
    /// the MCP namespace lifecycle.
    pub fn snapshot_tool_specs(&self) -> Vec<awaken_contract::BuiltinSpec> {
        let mut out = Vec::new();
        for id in self.tools.tool_ids() {
            let Some(tool) = self.tools.get_tool(&id) else {
                continue;
            };
            let descriptor = tool.descriptor();
            out.push(awaken_contract::BuiltinSpec::tool(
                awaken_contract::ToolSpec {
                    id: descriptor.id,
                    name: descriptor.name,
                    description: descriptor.description,
                    category: descriptor.category,
                    parameters_schema: descriptor.parameters,
                },
            ));
        }
        out.sort_by(|a, b| a.id().cmp(b.id()));
        out
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

        let mut next_state = self
            .mcp_registry_factory
            .connect(specs)
            .await?
            .map(|handle| ActiveMcpRegistry {
                specs: specs.to_vec(),
                tool_registry: handle.tool_registry(),
                handle,
            });

        let refresh_error = next_state
            .as_ref()
            .and_then(|active| self.ensure_mcp_periodic_refresh(&active.handle).err());
        if let Some(error) = refresh_error {
            if let Some(active) = next_state.take()
                && let Err(close_error) = active.handle.close().await
            {
                tracing::warn!(
                    error = %close_error,
                    "failed to close prepared MCP registry after refresh setup failure"
                );
            }
            return Err(error);
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

    fn compose_tool_registry(
        &self,
        dynamic_tools: Option<Arc<dyn ToolRegistry>>,
        description_overrides: std::collections::HashMap<String, String>,
    ) -> Result<Arc<dyn ToolRegistry>, ConfigRuntimeError> {
        let base: Arc<dyn ToolRegistry> = if description_overrides.is_empty() {
            Arc::clone(&self.tools)
        } else {
            Arc::new(
                crate::services::tool_overrides::DescriptionOverrideRegistry::new(
                    Arc::clone(&self.tools),
                    description_overrides,
                ),
            ) as Arc<dyn ToolRegistry>
        };

        let Some(dynamic_tools) = dynamic_tools else {
            return Ok(base);
        };

        let base_ids: HashSet<_> = base.tool_ids().into_iter().collect();
        for tool_id in dynamic_tools.tool_ids() {
            if base_ids.contains(&tool_id) {
                return Err(ConfigRuntimeError::InvalidConfig(format!(
                    "mcp tool id conflicts with existing tool: {tool_id}"
                )));
            }
        }

        Ok(Arc::new(OverlayToolRegistry::new(base, dynamic_tools)) as Arc<dyn ToolRegistry>)
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
/// `ServerState::credential_broker` is the production wiring.
pub fn build_genai_provider_executor_with_broker(
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

/// Build a genai-backed provider executor using a fresh credential broker.
///
/// This preserves the 0.4.0 public API. Production code that wants broker
/// sharing should call [`build_genai_provider_executor_with_broker`] or use
/// [`GenaiProviderExecutorFactory::with_broker`].
pub fn build_genai_provider_executor(
    spec: &ProviderSpec,
) -> Result<Arc<dyn LlmExecutor>, ConfigRuntimeError> {
    build_genai_provider_executor_with_broker(
        spec,
        Arc::new(awaken_runtime::credentials::AwakenCredentialBroker::new()),
    )
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
/// recognises it. Final validation happens via
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
    T: serde::de::DeserializeOwned + awaken_contract::ConfigRecordMerge,
{
    let mut out = Vec::with_capacity(entries.len());
    for (_, value) in entries {
        let raw_record: ConfigRecord<Value> = ConfigRecord::from_value(value.clone())
            .map_err(|error| StorageError::Serialization(error.to_string()))
            .map_err(ConfigRuntimeError::Storage)?;
        if raw_record.meta.hidden {
            continue;
        }

        let record: ConfigRecord<T> = awaken_contract::validate_config_record(value.clone())
            .map_err(|error| StorageError::Serialization(error.to_string()))
            .map_err(ConfigRuntimeError::Storage)?;
        let effective = crate::services::config_envelope::apply_overrides(
            record.spec,
            record.meta.user_overrides.as_ref(),
        )
        .map_err(|error| StorageError::Serialization(error.to_string()))
        .map_err(ConfigRuntimeError::Storage)?;
        out.push(effective);
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
    use crate::services::builtin_seed::SeedError as E;
    use ConfigRuntimeError::{InvalidConfig, Storage};
    match error {
        E::Storage(e) => Storage(e),
        e @ E::Serde(_) => Storage(StorageError::Serialization(e.to_string())),
        e => InvalidConfig(e.to_string()),
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

    /// Verifies the factory's defensive `None` path: when the underlying
    /// runtime has been dropped, `for_agent` returns `None` instead of
    /// panicking or trying to dereference a dangling weak handle. This
    /// is the "graceful degradation" the transport's fallback handler
    /// counts on — without it the factory could mistake "runtime is
    /// gone" for "this agent can't sample" silently.
    #[tokio::test]
    async fn registry_factory_returns_none_when_runtime_dropped() {
        // Build an AgentRuntime, downgrade to Weak, then drop the Arc.
        // The factory should then refuse to produce a handler.
        let runtime = Arc::new(
            awaken_runtime::AgentRuntimeBuilder::new()
                .build()
                .expect("minimal runtime builds"),
        );
        let weak = Arc::downgrade(&runtime);
        drop(runtime);

        let factory = RegistryDrivenSamplingHandlerFactory::new(weak);
        let spec = AgentSpec {
            id: "alpha".into(),
            model_id: "any-model".into(),
            system_prompt: String::new(),
            ..AgentSpec::default()
        };
        assert!(
            factory.for_agent(&spec).await.is_none(),
            "factory must not produce a handler for a dropped runtime"
        );
    }

    /// R8 #2 / R10 #3 regression: sampling-factory cache must drop
    /// stale entries when the underlying [`AgentRuntime`]'s
    /// `RegistrySet` is replaced. The cache key
    /// `(agent_id, model_id)` alone is not sufficient — `ModelSpec` entries
    /// can change underneath without the key changing. We tag entries
    /// with `registry_version()`; a mismatch wipes the cache so the
    /// next lookup walks the live registry.
    #[tokio::test]
    async fn registry_factory_cache_invalidates_on_registry_replace() {
        use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
        use awaken_contract::contract::inference::{StreamResult, TokenUsage};
        use awaken_runtime::registry::RegistrySet;
        use awaken_runtime::registry::memory::{
            MapAgentSpecRegistry, MapModelRegistry, MapPluginSource, MapProviderRegistry,
            MapToolRegistry,
        };

        // Two distinct executors so we can tell which one the cache
        // hands back. Mock returns its label in the response text so a
        // sampling round trip would expose which executor ran — but we
        // only need Arc identity for this assertion.
        struct LabelExecutor {
            label: &'static str,
        }
        #[async_trait::async_trait]
        impl LlmExecutor for LabelExecutor {
            async fn execute(
                &self,
                _request: InferenceRequest,
            ) -> Result<StreamResult, InferenceExecutionError> {
                Ok(StreamResult {
                    content: vec![awaken_contract::contract::content::ContentBlock::text(
                        self.label.to_string(),
                    )],
                    tool_calls: vec![],
                    usage: Some(TokenUsage::default()),
                    stop_reason: None,
                    has_incomplete_tool_calls: false,
                })
            }
            fn name(&self) -> &str {
                "label"
            }
        }

        fn registry_set_pointing_at(executor_label: &'static str) -> RegistrySet {
            let mut agents = MapAgentSpecRegistry::new();
            agents
                .register_spec(AgentSpec {
                    id: "alpha".into(),
                    model_id: "m".into(),
                    system_prompt: String::new(),
                    ..Default::default()
                })
                .unwrap();
            let mut models = MapModelRegistry::new();
            models
                .register_model(ModelSpec::new(
                    "m",
                    "p",
                    format!("upstream-{executor_label}"),
                ))
                .unwrap();
            let mut providers = MapProviderRegistry::new();
            providers
                .register_provider(
                    "p",
                    Arc::new(LabelExecutor {
                        label: executor_label,
                    }) as Arc<dyn LlmExecutor>,
                )
                .unwrap();
            RegistrySet {
                agents: Arc::new(agents),
                tools: Arc::new(MapToolRegistry::new()),
                models: Arc::new(models),
                providers: Arc::new(providers),
                plugins: Arc::new(MapPluginSource::new()),
                backends: Arc::new(awaken_runtime::registry::memory::MapBackendRegistry::new()),
            }
        }

        let runtime = Arc::new(
            awaken_runtime::AgentRuntimeBuilder::new()
                .with_provider("p", Arc::new(LabelExecutor { label: "v1" }))
                .with_model(ModelSpec::new("m", "p", "upstream-v1"))
                .with_agent_spec(AgentSpec {
                    id: "alpha".into(),
                    model_id: "m".into(),
                    system_prompt: String::new(),
                    ..Default::default()
                })
                .build()
                .expect("v1 runtime builds"),
        );

        let v1_version = runtime.registry_version().expect("v1 registered");
        let factory = RegistryDrivenSamplingHandlerFactory::new(Arc::downgrade(&runtime));
        let spec = AgentSpec {
            id: "alpha".into(),
            model_id: "m".into(),
            system_prompt: String::new(),
            ..AgentSpec::default()
        };

        let handler_v1 = factory
            .for_agent(&spec)
            .await
            .expect("factory yields handler for v1");

        // Same key → same cached Arc (the cache hit path).
        let handler_v1_again = factory
            .for_agent(&spec)
            .await
            .expect("factory yields handler for v1 again");
        assert!(
            Arc::ptr_eq(&handler_v1, &handler_v1_again),
            "second lookup at the same registry version must be a cache hit (same Arc)"
        );

        // Replace the registry with a v2 set that points the SAME
        // (agent_id, model_id) pair at a DIFFERENT executor. Without
        // version-tagged invalidation, the cache would happily return
        // the v1 handler — the bug R8 #2 fixed.
        let new_version = runtime
            .replace_registry_set(registry_set_pointing_at("v2"))
            .expect("replace_registry_set yields a fresh version");
        assert!(
            new_version > v1_version,
            "replace_registry_set must bump registry_version (was {v1_version}, got {new_version})"
        );

        let handler_v2 = factory
            .for_agent(&spec)
            .await
            .expect("factory yields handler for v2");
        assert!(
            !Arc::ptr_eq(&handler_v1, &handler_v2),
            "cache must invalidate on registry_version bump — got the same Arc back, \
             which means sampling would still route to the OLD executor"
        );
    }

    /// `with_runtime` constructs a factory; `new()` (the `Default` path)
    /// gives a sampling-less factory. This pins the public surface so
    /// future refactors don't accidentally make `new()` synthesize a
    /// fixed factory that runs cross-agent (the very bug R1 fixed).
    #[test]
    fn default_factory_has_no_sampling_handler_factory() {
        let factory = DefaultMcpRegistryFactory::new();
        assert!(factory.sampling_handler_factory.is_none());
        let factory = DefaultMcpRegistryFactory::default();
        assert!(factory.sampling_handler_factory.is_none());
    }

    #[test]
    fn build_genai_with_valid_headers_succeeds() {
        let mut options = BTreeMap::new();
        options.insert("headers".into(), json!({"OpenAI-Organization": "org-xyz"}));
        let spec = provider_spec_with_options(options);
        build_genai_provider_executor_with_broker(&spec, test_broker())
            .expect("valid headers must build");
    }

    #[test]
    fn build_genai_rejects_non_object_headers() {
        let mut options = BTreeMap::new();
        options.insert("headers".into(), json!("not-an-object"));
        let spec = provider_spec_with_options(options);
        let err = match build_genai_provider_executor_with_broker(&spec, test_broker()) {
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
        let err = match build_genai_provider_executor_with_broker(&spec, test_broker()) {
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
        build_genai_provider_executor_with_broker(&spec, test_broker())
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
            build_genai_provider_executor_with_broker(&spec, test_broker()).unwrap_or_else(|err| {
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
            build_genai_provider_executor_with_broker(&spec, test_broker()).unwrap_or_else(|err| {
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
            build_genai_provider_executor_with_broker(&spec, test_broker()).unwrap_or_else(|err| {
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
            };
            build_genai_provider_executor_with_broker(&spec, test_broker()).unwrap_or_else(|err| {
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
            build_genai_provider_executor_with_broker(&spec, test_broker()).unwrap_or_else(|err| {
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
        build_genai_provider_executor_with_broker(&spec, test_broker())
            .expect("env-fallback bearer must build");
    }

    #[test]
    fn build_genai_explicit_bearer_succeeds() {
        let spec = provider_spec_with_kind_and_key("openai", Some("bearer"), Some("sk-test-123"));
        build_genai_provider_executor_with_broker(&spec, test_broker())
            .expect("explicit bearer must build");
    }

    #[test]
    fn build_genai_unknown_credentials_kind_rejected_with_clear_error() {
        let spec = provider_spec_with_kind_and_key(
            "openai",
            Some("never-heard-of-it"),
            Some("sk-test-123"),
        );
        let err = build_genai_provider_executor_with_broker(&spec, test_broker())
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
        let err = build_genai_provider_executor_with_broker(&spec, test_broker())
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
        build_genai_provider_executor_with_broker(&spec, broker).expect("static bearer must build");

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
        build_genai_provider_executor_with_broker(&spec, broker).expect("env-fallback must build");

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

    /// Replaces the former `bootstrap_if_empty` test.  Asserts that
    /// `apply_seed` stores each spec as a ConfigRecord envelope whose
    /// `meta.source` is `RecordSource::Builtin { binary_version }`.
    #[tokio::test]
    async fn apply_seed_writes_builtin_envelope() {
        use awaken_contract::{
            BuiltinSeedSet, BuiltinSpec, ConfigRecord, ModelSpec, ProviderSpec, RecordSource,
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
                BuiltinSpec::Model(ModelSpec::new("m1", "p1", "m1-model")),
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
    pub(super) async fn make_manager_with_store() -> (
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
                .with_model(ModelSpec::new("boot", "boot", "boot-model"))
                .with_agent_spec(AgentSpec {
                    id: "boot".into(),
                    model_id: "boot".into(),
                    system_prompt: "boot".into(),
                    max_rounds: 1,
                    ..Default::default()
                })
                .with_in_memory_thread_run_store(thread_store.clone())
                .build()
                .expect("build runtime"),
        );
        let manager = ConfigRuntimeManager::new(runtime, store.clone()).expect("manager");
        (manager, store)
    }

    #[tokio::test]
    async fn publish_closes_prepared_mcp_registry_when_refresh_start_fails() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct RefreshFailingRegistry {
            tool_registry: Arc<dyn ToolRegistry>,
            close_count: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl ManagedMcpRegistry for RefreshFailingRegistry {
            fn tool_registry(&self) -> Arc<dyn ToolRegistry> {
                Arc::clone(&self.tool_registry)
            }

            fn periodic_refresh_running(&self) -> bool {
                false
            }

            fn start_periodic_refresh(
                &self,
                _interval: Duration,
            ) -> Result<(), ConfigRuntimeError> {
                Err(ConfigRuntimeError::PeriodicRefresh(
                    "scripted MCP refresh failure".to_string(),
                ))
            }

            async fn stop_periodic_refresh(&self) -> bool {
                false
            }

            async fn close(&self) -> Result<(), ConfigRuntimeError> {
                self.close_count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        struct RefreshFailingFactory {
            close_count: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl McpRegistryFactory for RefreshFailingFactory {
            async fn connect(
                &self,
                specs: &[McpServerSpec],
            ) -> Result<Option<Arc<dyn ManagedMcpRegistry>>, ConfigRuntimeError> {
                assert!(!specs.is_empty(), "test must exercise a real MCP state");
                Ok(Some(Arc::new(RefreshFailingRegistry {
                    tool_registry: Arc::new(
                        awaken_runtime::registry::memory::MapToolRegistry::new(),
                    ),
                    close_count: Arc::clone(&self.close_count),
                }) as Arc<dyn ManagedMcpRegistry>))
            }
        }

        let close_count = Arc::new(AtomicUsize::new(0));
        let (manager, _) = make_manager_with_store().await;
        let manager = manager.with_mcp_registry_factory(Arc::new(RefreshFailingFactory {
            close_count: Arc::clone(&close_count),
        }));
        *manager.mcp_refresh_interval.write() = Some(Duration::from_secs(30));

        let err = manager
            .publish(ManagedConfigSnapshot {
                providers: Vec::new(),
                models: Vec::new(),
                pools: Vec::new(),
                agents: Vec::new(),
                mcp_servers: vec![McpServerSpec {
                    id: "demo".to_string(),
                    transport: McpTransportKind::Http,
                    url: Some("http://mcp.example.invalid".to_string()),
                    ..McpServerSpec::default()
                }],
                tools: Vec::new(),
                skills: Vec::new(),
                source_config_revisions: Vec::new(),
                fingerprint: 1,
            })
            .await
            .expect_err("refresh setup failure must abort publish");

        assert!(
            matches!(err, ConfigRuntimeError::PeriodicRefresh(_)),
            "unexpected error: {err:?}"
        );
        assert_eq!(
            close_count.load(Ordering::SeqCst),
            1,
            "prepared MCP registry must be closed when refresh setup fails"
        );
    }

    #[tokio::test]
    async fn apply_seed_writes_builtin_records_to_store() {
        use awaken_contract::{
            BuiltinSeedSet, BuiltinSpec, ConfigRecord, ModelSpec, ProviderSpec, RecordSource,
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
                BuiltinSpec::Model(ModelSpec::new("seed-model", "seed-provider", "gpt-4o")),
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
        use awaken_contract::{BuiltinSeedSet, BuiltinSpec, ModelSpec, ProviderSpec};

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
                BuiltinSpec::Model(ModelSpec::new("idem-model", "idem-provider", "gpt-4o")),
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

    /// ConfigStore (base) wins over discovered (overlay) for the same agent id;
    /// discovery-only ids that are never seeded still resolve via the overlay.
    #[tokio::test]
    async fn discovered_agent_overlays_seeded_agent_with_same_id() {
        use awaken_contract::registry_spec::RemoteEndpoint;
        use awaken_contract::{BuiltinSeedSet, BuiltinSpec};

        // Build the runtime with a pre-registered agent "shared" that has an
        // endpoint (so DiscoveredAgentRegistry picks it up) and a distinct
        // remote-only agent "remote-only" also with an endpoint.
        struct Stub;
        #[async_trait::async_trait]
        impl awaken_contract::contract::executor::LlmExecutor for Stub {
            async fn execute(
                &self,
                _: awaken_contract::contract::executor::InferenceRequest,
            ) -> Result<
                awaken_contract::contract::inference::StreamResult,
                awaken_contract::contract::executor::InferenceExecutionError,
            > {
                Ok(awaken_contract::contract::inference::StreamResult {
                    content: vec![],
                    tool_calls: vec![],
                    usage: Some(awaken_contract::contract::inference::TokenUsage::default()),
                    stop_reason: Some(awaken_contract::contract::inference::StopReason::EndTurn),
                    has_incomplete_tool_calls: false,
                })
            }
            fn name(&self) -> &str {
                "stub"
            }
        }

        let store = Arc::new(awaken_stores::InMemoryStore::new())
            as Arc<dyn awaken_contract::contract::config_store::ConfigStore>;
        let thread_store = Arc::new(awaken_stores::InMemoryStore::new());

        // Register a "shared" agent with an endpoint (will be captured as discovered)
        // and a "remote-only" agent with an endpoint.
        let shared_discovered = AgentSpec {
            id: "shared".into(),
            model_id: "boot".into(),
            system_prompt: "discovered-prompt".into(),
            max_rounds: 1,
            endpoint: Some(RemoteEndpoint {
                base_url: "http://remote-shared/".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let remote_only = AgentSpec {
            id: "remote-only".into(),
            model_id: "boot".into(),
            system_prompt: "remote-only-prompt".into(),
            max_rounds: 1,
            endpoint: Some(RemoteEndpoint {
                base_url: "http://remote-only/".into(),
                ..Default::default()
            }),
            ..Default::default()
        };

        let runtime = Arc::new(
            awaken_runtime::builder::AgentRuntimeBuilder::new()
                .with_provider("boot", Arc::new(Stub))
                .with_model(ModelSpec::new("boot", "boot", "boot-model"))
                .with_agent_spec(shared_discovered)
                .with_agent_spec(remote_only)
                .with_in_memory_thread_run_store(thread_store.clone())
                .build()
                .expect("build runtime"),
        );

        // Use a stub provider factory so provider executor construction always succeeds.
        struct StubFactory;
        impl ProviderExecutorFactory for StubFactory {
            fn build(
                &self,
                _spec: &ProviderSpec,
            ) -> Result<Arc<dyn awaken_contract::contract::executor::LlmExecutor>, ConfigRuntimeError>
            {
                Ok(Arc::new(Stub))
            }
        }

        let manager = ConfigRuntimeManager::new(runtime.clone(), store.clone())
            .expect("manager")
            .with_provider_factory(Arc::new(StubFactory));

        // Seed a provider + model + "shared" agent — NO endpoint on the agent,
        // so it lives in ConfigStore only (plain Builtin, not discovered).
        let seed = BuiltinSeedSet {
            binary_version: "overlay-test".to_owned(),
            specs: vec![
                BuiltinSpec::Provider(awaken_contract::ProviderSpec {
                    id: "boot-prov".into(),
                    adapter: "stub".into(),
                    ..Default::default()
                }),
                BuiltinSpec::Model(awaken_contract::ModelSpec::new(
                    "boot-model",
                    "boot-prov",
                    "gpt-4o",
                )),
                BuiltinSpec::Agent(Box::new(AgentSpec {
                    id: "shared".into(),
                    model_id: "boot-model".into(),
                    system_prompt: "seeded-prompt".into(),
                    max_rounds: 5,
                    endpoint: None,
                    ..Default::default()
                })),
            ],
        };
        manager.apply_seed(&seed).await.expect("apply_seed");
        manager.apply().await.expect("apply");

        // After apply, the active registry is installed in the runtime.
        let snapshot = runtime.registry_snapshot().expect("registry snapshot");
        let registry = &snapshot.registries().agents;

        // "shared" → seeded (base) wins over discovered (overlay).
        let shared_spec = registry.get_agent("shared").expect("shared must resolve");
        let shared_json = serde_json::to_value(&shared_spec).expect("serialize");
        assert_eq!(
            shared_json["system_prompt"], "seeded-prompt",
            "base (seeded) wins: system_prompt must be 'seeded-prompt', got {shared_json}"
        );
        assert_eq!(
            shared_json["max_rounds"], 5,
            "base (seeded) wins: max_rounds must be 5"
        );

        // "remote-only" → only in discovered layer; must still resolve.
        let remote_spec = registry
            .get_agent("remote-only")
            .expect("remote-only must resolve via overlay");
        let remote_json = serde_json::to_value(&remote_spec).expect("serialize");
        assert_eq!(
            remote_json["system_prompt"], "remote-only-prompt",
            "discovery-only agent resolves via overlay"
        );
        assert_eq!(
            remote_json["endpoint"]["base_url"], "http://remote-only/",
            "endpoint base_url must be preserved"
        );
    }

    // ── apply_overrides integration tests ────────────────────────────────────

    /// Helper: build a minimal Builtin envelope for an AgentSpec with optional
    /// user_overrides stored on the meta.
    fn builtin_agent_record(
        spec: &AgentSpec,
        binary_version: &str,
        user_overrides: Option<serde_json::Value>,
    ) -> serde_json::Value {
        use awaken_contract::{ConfigRecord, RecordMeta};
        let mut meta = RecordMeta::new_builtin(binary_version);
        meta.user_overrides = user_overrides;
        let record = ConfigRecord {
            spec: spec.clone(),
            meta,
        };
        record.to_value().expect("envelope serialize must succeed")
    }

    #[tokio::test]
    async fn apply_overrides_merges_at_read() {
        use awaken_contract::{BuiltinSeedSet, BuiltinSpec};

        let (manager, store) = make_manager_with_store().await;

        // Seed provider + model + agent so apply() can build a working registry.
        let seed = BuiltinSeedSet {
            binary_version: "v1".to_owned(),
            specs: vec![
                BuiltinSpec::Provider(ProviderSpec {
                    id: "p".into(),
                    adapter: "openai".into(),
                    ..Default::default()
                }),
                BuiltinSpec::Model(ModelSpec::new("m", "p", "gpt-4o")),
                BuiltinSpec::Agent(Box::new(AgentSpec {
                    id: "x".into(),
                    model_id: "m".into(),
                    system_prompt: "base-prompt".into(),
                    max_rounds: 5,
                    ..Default::default()
                })),
            ],
        };
        manager.apply_seed(&seed).await.expect("apply_seed");

        // Overwrite the agent record directly in the store with user_overrides.
        let base_spec = AgentSpec {
            id: "x".into(),
            model_id: "m".into(),
            system_prompt: "base-prompt".into(),
            max_rounds: 5,
            ..Default::default()
        };
        let envelope =
            builtin_agent_record(&base_spec, "v1", Some(json!({"system_prompt": "patched"})));
        store
            .put("agents", "x", &envelope)
            .await
            .expect("put must succeed");

        manager.apply().await.expect("apply must succeed");

        let snapshot = manager
            .runtime
            .registry_snapshot()
            .expect("registry snapshot");
        let spec = snapshot
            .registries()
            .agents
            .get_agent("x")
            .expect("agent x must resolve");
        assert_eq!(
            spec.system_prompt, "patched",
            "user_overrides must be applied at read time"
        );
        // Non-overridden field stays as base.
        assert_eq!(spec.max_rounds, 5);
    }

    #[tokio::test]
    async fn failed_candidate_validation_does_not_commit_provider_executor_cache() {
        use awaken_contract::{BuiltinSeedSet, BuiltinSpec, ConfigRecord, RecordMeta};

        let (manager, store) = make_manager_with_store().await;

        let seed = BuiltinSeedSet {
            binary_version: "cache-test".to_owned(),
            specs: vec![
                BuiltinSpec::Provider(ProviderSpec {
                    id: "p".into(),
                    adapter: "openai".into(),
                    ..Default::default()
                }),
                BuiltinSpec::Model(ModelSpec::new("m", "p", "gpt-4o")),
                BuiltinSpec::Agent(Box::new(AgentSpec {
                    id: "a".into(),
                    model_id: "m".into(),
                    system_prompt: "base".into(),
                    max_rounds: 1,
                    ..Default::default()
                })),
            ],
        };
        manager.apply_seed(&seed).await.expect("apply seed");
        manager.apply().await.expect("initial apply");

        let initial_cached_provider = manager
            .provider_executor_cache
            .lock()
            .get("p")
            .expect("provider cache entry")
            .0
            .clone();

        let changed_provider = ConfigRecord {
            spec: ProviderSpec {
                id: "p".into(),
                adapter: "openai".into(),
                base_url: Some("https://provider-cache-candidate.example".into()),
                timeout_secs: 17,
                ..Default::default()
            },
            meta: RecordMeta::new_builtin("cache-test"),
        };
        store
            .put(
                "providers",
                "p",
                &changed_provider
                    .to_value()
                    .expect("serialize changed provider"),
            )
            .await
            .expect("write changed provider");

        let invalid_agent = ConfigRecord {
            spec: AgentSpec {
                id: "a".into(),
                model_id: "missing-model".into(),
                system_prompt: "invalid".into(),
                max_rounds: 1,
                ..Default::default()
            },
            meta: RecordMeta::new_builtin("cache-test"),
        };
        store
            .put(
                "agents",
                "a",
                &invalid_agent.to_value().expect("serialize invalid agent"),
            )
            .await
            .expect("write invalid agent");

        manager
            .apply()
            .await
            .expect_err("invalid candidate must fail validation");

        let cached_provider = manager
            .provider_executor_cache
            .lock()
            .get("p")
            .expect("provider cache entry must remain")
            .0
            .clone();
        assert_eq!(cached_provider, initial_cached_provider);
    }

    #[tokio::test]
    async fn apply_overrides_no_user_overrides_uses_base() {
        use awaken_contract::{BuiltinSeedSet, BuiltinSpec};

        let (manager, store) = make_manager_with_store().await;

        let seed = BuiltinSeedSet {
            binary_version: "v1".to_owned(),
            specs: vec![
                BuiltinSpec::Provider(ProviderSpec {
                    id: "p".into(),
                    adapter: "openai".into(),
                    ..Default::default()
                }),
                BuiltinSpec::Model(ModelSpec::new("m", "p", "gpt-4o")),
                BuiltinSpec::Agent(Box::new(AgentSpec {
                    id: "y".into(),
                    model_id: "m".into(),
                    system_prompt: "base-prompt".into(),
                    max_rounds: 3,
                    ..Default::default()
                })),
            ],
        };
        manager.apply_seed(&seed).await.expect("apply_seed");

        // Ensure the record has no overrides (seed already writes None).
        let raw = store.get("agents", "y").await.unwrap().unwrap();
        let rec: awaken_contract::ConfigRecord<serde_json::Value> =
            awaken_contract::ConfigRecord::from_value(raw).unwrap();
        assert!(rec.meta.user_overrides.is_none());

        manager.apply().await.expect("apply must succeed");

        let snapshot = manager
            .runtime
            .registry_snapshot()
            .expect("registry snapshot");
        let spec = snapshot
            .registries()
            .agents
            .get_agent("y")
            .expect("agent y must resolve");
        assert_eq!(spec.system_prompt, "base-prompt");
        assert_eq!(spec.max_rounds, 3);
    }

    #[tokio::test]
    async fn apply_overrides_on_user_record_applies_overrides() {
        // At read time, user_overrides is applied regardless of source.
        // The semantic guard (only Builtin records should have overrides set)
        // is enforced at the write path, not at read time.
        use awaken_contract::{BuiltinSeedSet, BuiltinSpec};
        use awaken_contract::{ConfigRecord, RecordMeta};

        let (manager, store) = make_manager_with_store().await;

        // Seed supporting records but NOT the agent "z".
        let seed = BuiltinSeedSet {
            binary_version: "v1".to_owned(),
            specs: vec![
                BuiltinSpec::Provider(ProviderSpec {
                    id: "p".into(),
                    adapter: "openai".into(),
                    ..Default::default()
                }),
                BuiltinSpec::Model(ModelSpec::new("m", "p", "gpt-4o")),
            ],
        };
        manager.apply_seed(&seed).await.expect("apply_seed");

        // Write a User-source record with user_overrides set (unusual in
        // production but must work defensively at read time).
        let user_spec = AgentSpec {
            id: "z".into(),
            model_id: "m".into(),
            system_prompt: "user-base".into(),
            max_rounds: 2,
            ..Default::default()
        };
        let mut meta = RecordMeta::new_user();
        meta.user_overrides = Some(json!({"system_prompt": "user-override"}));
        let record = ConfigRecord {
            spec: user_spec,
            meta,
        };
        store
            .put("agents", "z", &record.to_value().unwrap())
            .await
            .expect("put must succeed");

        manager.apply().await.expect("apply must succeed");

        let snapshot = manager
            .runtime
            .registry_snapshot()
            .expect("registry snapshot");
        let spec = snapshot
            .registries()
            .agents
            .get_agent("z")
            .expect("agent z must resolve");
        // Overrides are applied even for User-source records at read time.
        assert_eq!(
            spec.system_prompt, "user-override",
            "user_overrides applied at read time regardless of source"
        );
    }

    #[tokio::test]
    async fn version_upgrade_preserves_user_overrides() {
        use awaken_contract::{BuiltinSeedSet, BuiltinSpec};

        let (manager, store) = make_manager_with_store().await;

        // Apply seed v1 with agent A.
        let seed_v1 = BuiltinSeedSet {
            binary_version: "v1".to_owned(),
            specs: vec![
                BuiltinSpec::Provider(ProviderSpec {
                    id: "p".into(),
                    adapter: "openai".into(),
                    ..Default::default()
                }),
                BuiltinSpec::Model(ModelSpec::new("m", "p", "gpt-4o")),
                BuiltinSpec::Agent(Box::new(AgentSpec {
                    id: "a".into(),
                    model_id: "m".into(),
                    system_prompt: "v1-prompt".into(),
                    max_rounds: 5,
                    ..Default::default()
                })),
            ],
        };
        manager.apply_seed(&seed_v1).await.expect("apply_seed v1");

        // Manually set user_overrides on the stored record.
        let raw = store.get("agents", "a").await.unwrap().unwrap();
        let mut rec: awaken_contract::ConfigRecord<serde_json::Value> =
            awaken_contract::ConfigRecord::from_value(raw).unwrap();
        rec.meta.user_overrides = Some(json!({"system_prompt": "user-prompt"}));
        store
            .put("agents", "a", &rec.to_value().unwrap())
            .await
            .expect("put with overrides");

        // Apply seed v2 with different defaults (new system_prompt + max_rounds).
        let seed_v2 = BuiltinSeedSet {
            binary_version: "v2".to_owned(),
            specs: vec![
                BuiltinSpec::Provider(ProviderSpec {
                    id: "p".into(),
                    adapter: "openai".into(),
                    ..Default::default()
                }),
                BuiltinSpec::Model(ModelSpec::new("m", "p", "gpt-4o")),
                BuiltinSpec::Agent(Box::new(AgentSpec {
                    id: "a".into(),
                    model_id: "m".into(),
                    system_prompt: "v2-prompt".into(),
                    max_rounds: 10,
                    ..Default::default()
                })),
            ],
        };
        manager.apply_seed(&seed_v2).await.expect("apply_seed v2");

        // Assert store record: binary_version = v2, user_overrides preserved.
        let raw = store.get("agents", "a").await.unwrap().unwrap();
        let stored: awaken_contract::ConfigRecord<serde_json::Value> =
            awaken_contract::ConfigRecord::from_value(raw).unwrap();
        assert_eq!(
            stored.meta.source,
            awaken_contract::RecordSource::Builtin {
                binary_version: "v2".to_owned()
            },
            "binary_version must be updated to v2"
        );
        assert_eq!(
            stored.meta.user_overrides,
            Some(json!({"system_prompt": "user-prompt"})),
            "user_overrides must be preserved across version upgrade"
        );
        // Base spec in store uses v2 values.
        assert_eq!(stored.spec["system_prompt"], "v2-prompt");
        assert_eq!(stored.spec["max_rounds"], 10);

        // Apply and resolve — effective spec should merge overrides onto v2 base.
        manager.apply().await.expect("apply must succeed");
        let snapshot = manager
            .runtime
            .registry_snapshot()
            .expect("registry snapshot");
        let spec = snapshot
            .registries()
            .agents
            .get_agent("a")
            .expect("agent a must resolve");
        assert_eq!(
            spec.system_prompt, "user-prompt",
            "user override for system_prompt must be preserved after version upgrade"
        );
        assert_eq!(
            spec.max_rounds, 10,
            "max_rounds must use v2 base (not overridden)"
        );
    }

    /// Build an `AgentRuntime` with one stub tool registered plus a
    /// `ConfigRuntimeManager` backed by an `InMemoryStore`. Seeds a minimal
    /// provider/model/agent/tool set and calls `apply_seed`.
    ///
    /// Returns `(manager, runtime, store)` so callers can write to the store
    /// directly without needing a `#[cfg(test)]` accessor.
    async fn bootstrap_with_static_tool(
        tool_id: &str,
        tool_description: &str,
    ) -> (
        Arc<ConfigRuntimeManager>,
        Arc<awaken_runtime::AgentRuntime>,
        Arc<dyn awaken_contract::contract::config_store::ConfigStore>,
    ) {
        use awaken_contract::contract::executor::{
            InferenceExecutionError, InferenceRequest, LlmExecutor,
        };
        use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
        use awaken_contract::contract::tool::{
            Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
        };
        use awaken_contract::{BuiltinSeedSet, BuiltinSpec, ModelSpec, ProviderSpec, ToolSpec};
        use awaken_stores::InMemoryStore;
        use serde_json::json;

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

        struct StubTool {
            id: String,
            description: String,
        }
        #[async_trait::async_trait]
        impl Tool for StubTool {
            fn descriptor(&self) -> ToolDescriptor {
                ToolDescriptor::new(&self.id, &self.id, &self.description)
            }
            async fn execute(
                &self,
                _args: serde_json::Value,
                _ctx: &ToolCallContext,
            ) -> Result<ToolOutput, ToolError> {
                Ok(ToolResult::success(&self.id, json!({})).into())
            }
        }

        struct StubFactory;
        impl ProviderExecutorFactory for StubFactory {
            fn build(
                &self,
                _spec: &ProviderSpec,
            ) -> Result<Arc<dyn awaken_contract::contract::executor::LlmExecutor>, ConfigRuntimeError>
            {
                Ok(Arc::new(Stub))
            }
        }

        let store = Arc::new(InMemoryStore::new())
            as Arc<dyn awaken_contract::contract::config_store::ConfigStore>;
        let thread_store = Arc::new(InMemoryStore::new());

        let runtime = Arc::new(
            awaken_runtime::builder::AgentRuntimeBuilder::new()
                .with_provider("boot", Arc::new(Stub))
                .with_model(ModelSpec::new("boot", "boot", "boot-model"))
                .with_tool(
                    tool_id,
                    Arc::new(StubTool {
                        id: tool_id.to_owned(),
                        description: tool_description.to_owned(),
                    }),
                )
                .with_in_memory_thread_run_store(thread_store.clone())
                .build()
                .expect("build runtime"),
        );

        let manager = Arc::new(
            ConfigRuntimeManager::new(runtime.clone(), store.clone())
                .expect("manager")
                .with_provider_factory(Arc::new(StubFactory)),
        );

        let seed = BuiltinSeedSet {
            binary_version: "test".to_owned(),
            specs: vec![
                BuiltinSpec::Provider(ProviderSpec {
                    id: "test-prov".into(),
                    adapter: "openai".into(),
                    ..Default::default()
                }),
                BuiltinSpec::Model(ModelSpec::new("test-model", "test-prov", "gpt-4o")),
                BuiltinSpec::Agent(Box::new(AgentSpec {
                    id: "agent-using-echo".into(),
                    model_id: "test-model".into(),
                    system_prompt: "you are a test".into(),
                    max_rounds: 1,
                    allowed_tools: None,
                    endpoint: None,
                    ..Default::default()
                })),
                BuiltinSpec::Tool(ToolSpec {
                    id: tool_id.to_owned(),
                    name: tool_id.to_owned(),
                    description: tool_description.to_owned(),
                    ..Default::default()
                }),
            ],
        };
        manager.apply_seed(&seed).await.expect("apply_seed");

        (manager, runtime, store)
    }

    #[tokio::test]
    async fn tool_description_override_applied_to_resolved_agent() {
        let (manager, runtime, store) =
            bootstrap_with_static_tool("echo", "stock description").await;

        manager.apply().await.expect("initial apply");

        let envelope = serde_json::json!({
            "spec": {
                "id": "echo",
                "name": "Echo",
                "description": "stock description",
                "category": null,
                "parameters_schema": {}
            },
            "meta": {
                "source": { "kind": "builtin", "binary_version": "test" },
                "user_overrides": { "description": "patched description" },
                "hidden": false,
                "created_at": 1,
                "updated_at": 2
            }
        });
        awaken_contract::contract::config_store::ConfigStore::put(
            store.as_ref(),
            "tools",
            "echo",
            &envelope,
        )
        .await
        .expect("write override");

        manager.apply().await.expect("apply with override");

        let resolver = runtime.resolver_arc();
        let resolved = resolver.resolve("agent-using-echo").expect("resolve");
        let descs = resolved.tool_descriptors();
        let echo = descs
            .iter()
            .find(|d| d.id == "echo")
            .expect("echo descriptor present");
        assert_eq!(echo.description, "patched description");
    }

    #[tokio::test]
    async fn snapshot_tool_specs_emits_one_entry_per_registered_tool() {
        let (manager, _runtime, _store) =
            bootstrap_with_static_tool("echo", "stock description").await;
        let specs = manager.snapshot_tool_specs();
        assert_eq!(specs.len(), 1);
        match &specs[0] {
            awaken_contract::BuiltinSpec::Tool(t) => {
                assert_eq!(t.id, "echo");
                assert_eq!(t.description, "stock description");
            }
            other => panic!("expected Tool variant, got {other:?}"),
        }
    }
}
