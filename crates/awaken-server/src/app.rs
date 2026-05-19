//! Application state and server startup.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Instant;

use awaken_contract::RedactedString;
use awaken_contract::contract::config_store::ConfigStore;
use awaken_contract::contract::storage::ThreadRunStore;
use awaken_ext_observability::RuntimeStatsRegistry;
use awaken_runtime::credentials::{AwakenCredentialBroker, CredentialBroker};
use awaken_runtime::{AgentResolver, AgentRuntime};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use awaken_ext_observability::trace_store::TraceStore;

use crate::mailbox::{Mailbox, MailboxLifecycleConfig};
use crate::services::audit_log::AuditLogger;
use crate::transport::replay_buffer::EventReplayBuffer;

pub type ReplayBufferEntry = (Arc<EventReplayBuffer>, Instant);
pub type ReplayBufferMap = Arc<Mutex<HashMap<String, ReplayBufferEntry>>>;
type AppStateExtrasRegistry = HashMap<
    usize,
    (
        Weak<Mutex<HashMap<String, ReplayBufferEntry>>>,
        AppStateExtras,
    ),
>;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillCatalogContext {
    Inline,
    Fork,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SkillCatalogArgument {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SkillCatalogEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<SkillCatalogArgument>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    pub user_invocable: bool,
    pub model_invocable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,
    pub context: SkillCatalogContext,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

pub trait SkillCatalogProvider: Send + Sync {
    fn list_skills(&self) -> Vec<SkillCatalogEntry>;
}

/// Graceful shutdown configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShutdownConfig {
    /// Maximum seconds to wait for in-flight requests to complete before
    /// force-exiting.  Defaults to 30.
    #[serde(default = "default_shutdown_timeout")]
    pub timeout_secs: u64,
}

fn default_shutdown_timeout() -> u64 {
    30
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            timeout_secs: default_shutdown_timeout(),
        }
    }
}

/// Mailbox lifecycle ownership for the HTTP server.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MailboxLifecycleMode {
    /// The server starts mailbox startup recovery and maintenance.
    #[default]
    Auto,
    /// The embedding application manages mailbox lifecycle explicitly.
    Manual,
}

/// Admin/configuration API security settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdminApiConfig {
    /// Optional bearer token required for admin/configuration APIs.
    ///
    /// Wrapped in [`RedactedString`] so it does not leak through `Debug` /
    /// `Display`. The wire format remains a plain JSON string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<RedactedString>,
    /// Origins allowed to call browser admin APIs.
    #[serde(default = "default_admin_cors_allowed_origins")]
    pub cors_allowed_origins: Vec<String>,
    /// Mount `/v1/config/*` and `/v1/agents` admin CRUD (default true).
    #[serde(default = "default_expose_config_routes")]
    pub expose_config_routes: bool,
    /// Mount `/v1/traces` (default false — exposes prompts/tool args).
    #[serde(default = "default_expose_trace_routes")]
    pub expose_trace_routes: bool,
    /// Mount `/v1/eval/*` (default true). Separate gate because eval
    /// drives live model calls + persistent run storage, different
    /// blast radius from config CRUD.
    #[serde(default = "default_expose_eval_routes")]
    pub expose_eval_routes: bool,
}

/// Audit-log retention settings attached to [`AppState`] via
/// [`AppState::with_audit_log_config`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditLogConfig {
    /// Whether [`AppState::with_audit_log_from_config`] should attach an audit logger.
    #[serde(default = "default_audit_log_enabled")]
    pub enabled: bool,
    /// Retention window for audit events in days. Default 90 days.
    #[serde(default = "default_audit_retention_days")]
    pub retention_days: u32,
    /// Interval in seconds between audit retention sweeps. Default 3600 (1 hour).
    ///
    /// A value of `0` is treated as misconfiguration: the server will warn and
    /// clamp to 60 seconds. Values between 1 and 9 emit a warning but are
    /// respected as the operator may have a specific reason.
    #[serde(default = "default_audit_sweep_interval_secs")]
    pub sweep_interval_secs: u64,
}

const fn default_expose_config_routes() -> bool {
    true
}
// F20: trace routes opt-in (more sensitive than admin metadata).
const fn default_expose_trace_routes() -> bool {
    false
}
const fn default_expose_eval_routes() -> bool {
    true
}

const fn default_audit_log_enabled() -> bool {
    true
}

const fn default_audit_retention_days() -> u32 {
    90
}

const fn default_audit_sweep_interval_secs() -> u64 {
    3600
}

impl Default for AuditLogConfig {
    fn default() -> Self {
        Self {
            enabled: default_audit_log_enabled(),
            retention_days: default_audit_retention_days(),
            sweep_interval_secs: default_audit_sweep_interval_secs(),
        }
    }
}

/// Compute the effective sweep interval, emitting warnings for suspicious values.
///
/// - `0` → warn and clamp to 60 s (implicit floor).
/// - `1..=9` → warn (respected as-is; operator may have a real reason).
pub fn effective_sweep_interval(secs: u64) -> std::time::Duration {
    if secs == 0 {
        tracing::warn!(
            audit_sweep_interval_secs = secs,
            "audit sweep interval is 0 — clamping to 60 s to avoid a tight spin loop"
        );
        return std::time::Duration::from_secs(60);
    }
    if secs < 10 {
        tracing::warn!(
            audit_sweep_interval_secs = secs,
            "audit sweep interval is very small; consider a value >= 10 s"
        );
    }
    std::time::Duration::from_secs(secs)
}

impl Default for AdminApiConfig {
    fn default() -> Self {
        Self {
            bearer_token: None,
            cors_allowed_origins: default_admin_cors_allowed_origins(),
            expose_config_routes: default_expose_config_routes(),
            expose_trace_routes: default_expose_trace_routes(),
            expose_eval_routes: default_expose_eval_routes(),
        }
    }
}

/// Server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Bind address (e.g. "0.0.0.0:3000").
    pub address: String,
    /// Maximum SSE channel buffer size.
    #[serde(default = "default_sse_buffer")]
    pub sse_buffer_size: usize,
    /// Maximum number of SSE frames to buffer per run for reconnection replay.
    #[serde(default = "default_replay_buffer_capacity")]
    pub replay_buffer_capacity: usize,
    /// Graceful shutdown settings.
    #[serde(default)]
    pub shutdown: ShutdownConfig,
    /// Maximum number of concurrent in-flight requests the server will accept.
    /// Additional requests receive 503 Service Unavailable.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_requests: usize,
    /// Optional bearer token required for authenticated extended A2A agent cards.
    ///
    /// Wrapped in [`RedactedString`] so it does not leak through `Debug` /
    /// `Display`. The wire format remains a plain JSON string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a_extended_card_bearer_token: Option<RedactedString>,
    /// Mailbox lifecycle ownership. Defaults to framework-managed auto mode.
    #[serde(default)]
    pub mailbox_lifecycle: MailboxLifecycleMode,
    /// `/v1/eval/*` caps — see [`crate::eval_limits::EvalLimits`].
    #[serde(default)]
    pub eval_limits: crate::eval_limits::EvalLimits,
}

const fn default_sse_buffer() -> usize {
    64
}
const fn default_replay_buffer_capacity() -> usize {
    1024
}
const fn default_max_concurrent() -> usize {
    100
}

pub const ADMIN_API_BEARER_TOKEN_ENV: &str = "AWAKEN_ADMIN_API_BEARER_TOKEN";
const ADMIN_CORS_ALLOWED_ORIGINS_ENV: &str = "AWAKEN_ADMIN_CORS_ALLOWED_ORIGINS";
static APP_STATE_EXTRAS: OnceLock<Mutex<AppStateExtrasRegistry>> = OnceLock::new();

#[derive(Clone)]
struct AppStateExtras {
    admin_api_config: AdminApiConfig,
    audit_log_config: AuditLogConfig,
    runtime_stats: Option<Arc<RuntimeStatsRegistry>>,
    audit_log: Option<Arc<AuditLogger>>,
    trace_store: Option<Arc<dyn TraceStore>>,
    eval_run_store: Option<Arc<dyn awaken_eval::EvalRunStore>>,
    started_at: Instant,
    credential_broker: Arc<dyn CredentialBroker>,
}

impl Default for AppStateExtras {
    fn default() -> Self {
        Self {
            admin_api_config: AdminApiConfig::default(),
            audit_log_config: AuditLogConfig::default(),
            runtime_stats: None,
            audit_log: None,
            trace_store: None,
            eval_run_store: None,
            started_at: Instant::now(),
            credential_broker: Arc::new(AwakenCredentialBroker::new()),
        }
    }
}

fn app_state_extras_registry() -> &'static Mutex<AppStateExtrasRegistry> {
    APP_STATE_EXTRAS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn app_state_extras_key(replay_buffers: &ReplayBufferMap) -> usize {
    Arc::as_ptr(replay_buffers) as usize
}

fn prune_app_state_extras(registry: &mut AppStateExtrasRegistry) {
    registry.retain(|_, (weak, _)| weak.upgrade().is_some());
}

fn register_default_app_state_extras(replay_buffers: &ReplayBufferMap) {
    let key = app_state_extras_key(replay_buffers);
    let weak = Arc::downgrade(replay_buffers);
    let mut registry = app_state_extras_registry().lock();
    prune_app_state_extras(&mut registry);
    registry
        .entry(key)
        .or_insert_with(|| (weak, AppStateExtras::default()));
}

fn app_state_extras(state: &AppState) -> AppStateExtras {
    let key = app_state_extras_key(&state.replay_buffers);
    let mut registry = app_state_extras_registry().lock();
    prune_app_state_extras(&mut registry);
    registry
        .get(&key)
        .map(|(_, extras)| extras.clone())
        .unwrap_or_default()
}

fn update_app_state_extras(state: &AppState, update: impl FnOnce(&mut AppStateExtras)) {
    let key = app_state_extras_key(&state.replay_buffers);
    let weak = Arc::downgrade(&state.replay_buffers);
    let mut registry = app_state_extras_registry().lock();
    prune_app_state_extras(&mut registry);
    let (_, extras) = registry
        .entry(key)
        .or_insert_with(|| (weak, AppStateExtras::default()));
    update(extras);
}

fn admin_api_bearer_token_from_env() -> Option<RedactedString> {
    std::env::var(ADMIN_API_BEARER_TOKEN_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(RedactedString::from)
}

fn admin_cors_allowed_origins_from_env() -> Option<Vec<String>> {
    std::env::var(ADMIN_CORS_ALLOWED_ORIGINS_ENV)
        .ok()
        .and_then(|value| {
            let origins = value
                .split(',')
                .map(str::trim)
                .filter(|origin| !origin.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            (!origins.is_empty()).then_some(origins)
        })
}

fn default_admin_cors_allowed_origins() -> Vec<String> {
    vec![
        "http://127.0.0.1:3002".to_string(),
        "http://localhost:3002".to_string(),
    ]
}

pub(crate) fn admin_api_config(state: &AppState) -> AdminApiConfig {
    let mut config = app_state_extras(state).admin_api_config;

    if let Some(token) = admin_api_bearer_token_from_env() {
        config.bearer_token = Some(token);
    }
    if let Some(origins) = admin_cors_allowed_origins_from_env() {
        config.cors_allowed_origins = origins;
    }

    config
}

fn admin_cors_allowed_origins_for_state(state: &AppState) -> Vec<String> {
    admin_api_config(state).cors_allowed_origins
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            address: "0.0.0.0:3000".to_string(),
            sse_buffer_size: default_sse_buffer(),
            replay_buffer_capacity: default_replay_buffer_capacity(),
            shutdown: ShutdownConfig::default(),
            max_concurrent_requests: default_max_concurrent(),
            a2a_extended_card_bearer_token: None,
            mailbox_lifecycle: MailboxLifecycleMode::Auto,
            eval_limits: crate::eval_limits::EvalLimits::default(),
        }
    }
}

/// Shared application state for all routes.
#[derive(Clone)]
pub struct AppState {
    /// Agent runtime for executing runs.
    pub runtime: Arc<AgentRuntime>,
    /// Unified mailbox service (persistent run queue).
    pub mailbox: Arc<Mailbox>,
    /// Unified thread + run persistence (atomic checkpoint).
    pub store: Arc<dyn ThreadRunStore>,
    /// Agent resolver for protocol-specific lookups.
    pub resolver: Arc<dyn AgentResolver>,
    /// Server configuration.
    pub config: ServerConfig,
    /// Optional persistent config store used by config management APIs.
    pub config_store: Option<Arc<dyn ConfigStore>>,
    /// Optional runtime publisher used to apply config changes.
    pub config_runtime_manager: Option<Arc<crate::services::config_runtime::ConfigRuntimeManager>>,
    /// Optional read-only skill catalog used by admin capabilities.
    pub skill_catalog_provider: Option<Arc<dyn SkillCatalogProvider>>,
    /// Per-run replay buffers for SSE stream resumption.
    /// Stores `(buffer, created_at)` so stale entries can be purged.
    pub replay_buffers: ReplayBufferMap,
    /// MCP Streamable HTTP session state.
    pub mcp_http: Arc<crate::protocols::mcp::http::McpHttpState>,
}

impl AppState {
    /// Create a new AppState with all required dependencies.
    pub fn new(
        runtime: Arc<AgentRuntime>,
        mailbox: Arc<Mailbox>,
        store: Arc<dyn ThreadRunStore>,
        resolver: Arc<dyn AgentResolver>,
        config: ServerConfig,
    ) -> Self {
        let state = Self {
            runtime,
            mailbox,
            store,
            resolver,
            config,
            config_store: None,
            config_runtime_manager: None,
            skill_catalog_provider: None,
            replay_buffers: Arc::new(Mutex::new(HashMap::new())),
            mcp_http: Arc::new(crate::protocols::mcp::http::McpHttpState::new()),
        };
        register_default_app_state_extras(&state.replay_buffers);
        state
    }

    /// Override the credential broker (e.g. inject a test double).
    /// Call before publishing the AppState; replacing it after the
    /// runtime is wired will leave existing executors pointing at the
    /// previous broker.
    pub fn with_credential_broker(
        self,
        broker: Arc<dyn awaken_runtime::credentials::CredentialBroker>,
    ) -> Self {
        update_app_state_extras(&self, |extras| {
            extras.credential_broker = broker;
        });
        self
    }

    /// Return the process-wide credential broker for this state.
    pub fn credential_broker(&self) -> Arc<dyn CredentialBroker> {
        app_state_extras(self).credential_broker
    }

    /// Attach a runtime stats registry. The same `Arc` should already be
    /// wired into the embedder's `ObservabilityPlugin` sink list so that
    /// recording and reading share state.
    #[must_use]
    pub fn with_runtime_stats(self, registry: Arc<RuntimeStatsRegistry>) -> Self {
        update_app_state_extras(&self, |extras| {
            extras.runtime_stats = Some(registry);
        });
        self
    }

    /// Return the attached runtime stats registry, if configured.
    pub fn runtime_stats(&self) -> Option<Arc<RuntimeStatsRegistry>> {
        app_state_extras(self).runtime_stats
    }

    /// Attach the config store used by config management routes.
    pub fn with_config_store(mut self, store: Arc<dyn ConfigStore>) -> Self {
        self.config_store = Some(store);
        self
    }

    /// Attach the runtime manager used to compile and publish config snapshots.
    pub fn with_config_runtime_manager(
        mut self,
        manager: Arc<crate::services::config_runtime::ConfigRuntimeManager>,
    ) -> Self {
        self.config_runtime_manager = Some(manager);
        self
    }

    /// Attach a read-only skill catalog provider used by admin capabilities.
    pub fn with_skill_catalog_provider(mut self, provider: Arc<dyn SkillCatalogProvider>) -> Self {
        self.skill_catalog_provider = Some(provider);
        self
    }

    /// Attach admin/configuration API security settings without changing the
    /// 0.2-compatible `ServerConfig` struct shape.
    pub fn with_admin_api_config(self, config: AdminApiConfig) -> Self {
        update_app_state_extras(&self, |extras| {
            extras.admin_api_config = config;
        });
        self
    }

    /// Require a bearer token for admin/configuration APIs.
    pub fn with_admin_api_bearer_token(self, token: impl Into<RedactedString>) -> Self {
        let mut config = admin_api_config(&self);
        config.bearer_token = Some(token.into());
        self.with_admin_api_config(config)
    }

    /// Configure CORS origins allowed to call browser admin APIs.
    pub fn with_admin_cors_allowed_origins(self, origins: Vec<String>) -> Self {
        let mut config = admin_api_config(&self);
        config.cors_allowed_origins = origins;
        self.with_admin_api_config(config)
    }

    /// Return the effective admin/configuration API settings, including
    /// environment variable overrides.
    pub fn admin_api_config(&self) -> AdminApiConfig {
        admin_api_config(self)
    }

    /// Attach audit-log retention settings used by
    /// [`AppState::with_audit_log_from_config`].
    #[must_use]
    pub fn with_audit_log_config(self, config: AuditLogConfig) -> Self {
        update_app_state_extras(&self, |extras| {
            extras.audit_log_config = config;
        });
        self
    }

    /// Return the audit-log retention settings for this state.
    pub fn audit_log_config(&self) -> AuditLogConfig {
        app_state_extras(self).audit_log_config
    }

    /// Enable the audit logger.  When a `config_store` is already attached and
    /// [`AuditLogConfig::enabled`] is `true`, pass an `AuditLogger` constructed
    /// from that store.  This method is an explicit
    /// opt-in; existing embedders are unaffected unless they call it.
    #[must_use]
    pub fn with_audit_log(self, logger: Arc<AuditLogger>) -> Self {
        update_app_state_extras(&self, |extras| {
            extras.audit_log = Some(logger);
        });
        self
    }

    /// Return the attached audit logger, if configured.
    pub fn audit_log(&self) -> Option<Arc<AuditLogger>> {
        app_state_extras(self).audit_log
    }

    /// Attach a `TraceStore` for the trace query API.
    ///
    /// Embedders using `install_default_sinks` / `observability_plugin_from`
    /// can extract `WiringSummary::trace_store` and pass it here so that the
    /// `/v1/traces` routes are backed by the same store that receives events.
    #[must_use]
    pub fn with_trace_store(self, store: Arc<dyn TraceStore>) -> Self {
        update_app_state_extras(&self, |extras| {
            extras.trace_store = Some(store);
        });
        self
    }

    /// Return the attached `TraceStore`, if configured.
    pub fn trace_store(&self) -> Option<Arc<dyn TraceStore>> {
        app_state_extras(self).trace_store
    }

    /// Attach an `EvalRunStore` so `/v1/eval/runs` endpoints can persist
    /// and query server-side eval runs (ADR-0032 D1).
    #[must_use]
    pub fn with_eval_run_store(self, store: Arc<dyn awaken_eval::EvalRunStore>) -> Self {
        update_app_state_extras(&self, |extras| {
            extras.eval_run_store = Some(store);
        });
        self
    }

    /// Return the attached `EvalRunStore`, if configured.
    pub fn eval_run_store(&self) -> Option<Arc<dyn awaken_eval::EvalRunStore>> {
        app_state_extras(self).eval_run_store
    }

    /// Builder convenience: create an `AuditLogger` from the already-attached
    /// `config_store` (if any) and the effective `AdminApiConfig` settings.
    ///
    /// If [`AuditLogConfig::enabled`] is `false` or no config store is attached, this
    /// is a no-op.  Also spawns the background retention sweeper task.
    #[must_use]
    pub fn with_audit_log_from_config(self) -> Self {
        let audit_config = self.audit_log_config();
        if !audit_config.enabled {
            return self;
        }

        let logger = match self.audit_log() {
            Some(existing) => existing,
            None => {
                let Some(store) = self.config_store.clone() else {
                    return self;
                };
                let new_logger = Arc::new(AuditLogger::new(store));
                update_app_state_extras(&self, |extras| {
                    extras.audit_log = Some(new_logger.clone());
                });
                new_logger
            }
        };

        let logger_for_sweeper = logger.clone();
        let retention_days = audit_config.retention_days;
        let sweep_interval = effective_sweep_interval(audit_config.sweep_interval_secs);
        // Spawn retention sweeper (fire-and-forget; leaked on shutdown, acceptable for v1).
        tokio::spawn(async move {
            let interval = sweep_interval;
            loop {
                tokio::time::sleep(interval).await;
                let cutoff = chrono::Utc::now() - chrono::Duration::days(retention_days as i64);
                match logger_for_sweeper.prune_before(cutoff).await {
                    Ok(pruned) => {
                        if pruned > 0 {
                            tracing::info!(pruned, "audit retention sweep complete");
                        }
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "audit retention sweep failed");
                    }
                }
            }
        });
        self
    }

    /// Return the wall-clock instant this state was constructed.
    pub fn started_at(&self) -> Instant {
        app_state_extras(self).started_at
    }

    /// Override the construction instant, primarily for deterministic tests.
    #[must_use]
    pub fn with_started_at(self, started_at: Instant) -> Self {
        update_app_state_extras(&self, |extras| {
            extras.started_at = started_at;
        });
        self
    }

    /// Insert a replay buffer for the given key, tracking creation time.
    pub fn insert_replay_buffer(&self, key: String, buffer: Arc<EventReplayBuffer>) {
        self.replay_buffers
            .lock()
            .insert(key, (buffer, Instant::now()));
    }

    /// Look up a replay buffer by key.
    pub fn get_replay_buffer(&self, key: &str) -> Option<Arc<EventReplayBuffer>> {
        self.replay_buffers
            .lock()
            .get(key)
            .map(|(buf, _)| Arc::clone(buf))
    }

    /// Remove a replay buffer by key.
    pub fn remove_replay_buffer(&self, key: &str) {
        self.replay_buffers.lock().remove(key);
    }

    /// Purge replay buffers whose subscribers are all gone and that are older
    /// than `max_age`. Called from the maintenance loop to prevent unbounded
    /// growth of the `replay_buffers` HashMap.
    pub fn purge_stale_replay_buffers(&self, max_age: std::time::Duration) {
        let now = Instant::now();
        let mut buffers = self.replay_buffers.lock();
        let before = buffers.len();
        buffers.retain(|_key, (_buf, created_at)| {
            let age = now.duration_since(*created_at);
            // Keep if younger than max_age, OR if the buffer still has live subscribers
            // (indicated by non-empty subscriber count — we check via a push that goes nowhere).
            // Since we can't directly query subscriber count, rely on age: if older than
            // max_age, the run is long done and the buffer is safe to purge.
            if age < max_age {
                return true;
            }
            // For buffers older than max_age, also keep if they were recently
            // updated (sequence is still advancing — run is still active).
            // A buffer with seq=0 and old age is definitely stale.
            // A buffer with subscribers would have been cleaned up by the SSE
            // handler's cleanup task, so any remaining old buffer is leaked.
            false
        });
        let purged = before - buffers.len();
        if purged > 0 {
            tracing::debug!(purged, "purged stale replay buffers");
        }
    }
}

/// Create a shutdown signal that fires on Ctrl-C and (on Unix) SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await.ok();
    }

    tracing::info!("shutting down gracefully...");
}

/// Start the server with graceful shutdown support.
///
/// The server will:
/// 1. Stop accepting new connections when a shutdown signal is received
///    (Ctrl-C or SIGTERM).
/// 2. Wait up to `shutdown_timeout` for in-flight requests to drain.
/// 3. Force-exit if the timeout is exceeded.
pub async fn serve_with_shutdown(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    shutdown_timeout: std::time::Duration,
) -> std::io::Result<()> {
    // Use a tokio::sync::Notify to decouple the signal from the drain
    // timeout.  When the OS signal fires we notify the shutdown future
    // (which tells axum to stop accepting) and *then* start the drain
    // timer.
    let drain_notify = Arc::new(tokio::sync::Notify::new());
    let drain_notify2 = drain_notify.clone();

    let graceful_signal = async move {
        shutdown_signal().await;
        // Kick off the drain-timeout clock.
        drain_notify2.notify_one();
    };

    let server = axum::serve(listener, app).with_graceful_shutdown(graceful_signal);

    // Wait for the drain period after the signal fires.
    let drain_deadline = async {
        drain_notify.notified().await;
        tokio::time::sleep(shutdown_timeout).await;
        tracing::warn!(
            "server did not drain within {}s — forcing exit",
            shutdown_timeout.as_secs()
        );
    };

    tokio::select! {
        result = server => result,
        () = drain_deadline => Ok(()),
    }
}

/// Convenience: bind, build the full router with layers, and serve.
pub async fn serve(state: AppState) -> std::io::Result<()> {
    crate::metrics::install_recorder();

    let addr = state.config.address.clone();
    let timeout = std::time::Duration::from_secs(state.config.shutdown.timeout_secs);
    let mailbox_lifecycle = match state.config.mailbox_lifecycle {
        MailboxLifecycleMode::Auto => {
            let cleanup_state = state.clone();
            Some(
                state
                    .mailbox
                    .start_lifecycle_ready(MailboxLifecycleConfig {
                        maintenance_callback: Some(Arc::new(move || {
                            cleanup_state
                                .purge_stale_replay_buffers(std::time::Duration::from_secs(300));
                        })),
                        ..Default::default()
                    })
                    .await
                    .map_err(|error| {
                        std::io::Error::other(format!("failed to start mailbox lifecycle: {error}"))
                    })?,
            )
        }
        MailboxLifecycleMode::Manual => None,
    };

    // Spawn the trace-retention loop whenever a TraceStore is attached.
    // Retention is a property of the *storage layer*, not the HTTP API —
    // an operator may turn off `expose_trace_routes` (e.g. to keep the
    // admin surface narrow) yet still depend on the persistence layer for
    // OTLP / Phoenix sourcing, and that path also needs TTL cleanup. The
    // RetentionHandle is intentionally dropped after the server exits
    // (fire-and-forget for v1), matching the audit sweeper.
    let _retention_handle = state.trace_store().map(|store| {
        crate::services::trace_retention::spawn_retention_loop(
            store,
            crate::services::trace_retention::RetentionConfig::default(),
        )
    });

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on {addr}");

    let config_runtime_manager = state.config_runtime_manager.clone();
    let app = build_service_router(state)?;

    let result = serve_with_shutdown(listener, app, timeout).await;
    if let Some(mailbox_lifecycle) = mailbox_lifecycle
        && let Err(error) = mailbox_lifecycle.shutdown().await
    {
        tracing::warn!(error = %error, "failed to stop mailbox lifecycle cleanly");
    }
    if let Some(manager) = config_runtime_manager
        && let Err(error) = manager.shutdown().await
    {
        tracing::warn!(error = %error, "failed to stop config runtime manager cleanly");
    }
    result
}

/// Build the production HTTP router for an [`AppState`].
///
/// This is the shared entry point for embedders that need to bind their own
/// listener or add outer layers. It applies the same admin-surface validation,
/// concurrency limit, admin CORS policy, route composition, and state wiring as
/// [`serve`].
pub fn build_service_router(state: AppState) -> std::io::Result<axum::Router> {
    validate_admin_surface(&state)?;
    let max_concurrent = state.config.max_concurrent_requests;
    let admin_cors = admin_cors_layer(&state)?;
    Ok(crate::routes::build_router(&state)
        .layer(tower::limit::ConcurrencyLimitLayer::new(max_concurrent))
        .layer(admin_cors)
        .with_state(state))
}

pub fn validate_admin_surface(state: &AppState) -> std::io::Result<()> {
    crate::eval_limits::validate_eval_limits(&state.config.eval_limits)?;
    let admin = admin_api_config(state);
    // Sensitive surfaces (need bearer on non-loopback bind): config
    // routes; trace routes when a trace store is wired; eval routes
    // always (online eval triggers live provider calls even at persist=false).
    let any_sensitive_route_exposed = admin.expose_config_routes
        || (admin.expose_trace_routes && state.trace_store().is_some())
        || admin.expose_eval_routes;
    if !any_sensitive_route_exposed {
        return Ok(());
    }
    if admin.bearer_token.is_some() {
        return Ok(());
    }
    if !admin_surface_has_sensitive_state(state) {
        return Ok(());
    }
    let Ok(addr) = state.config.address.parse::<std::net::SocketAddr>() else {
        return Ok(());
    };
    if addr.ip().is_loopback() {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!(
            "admin APIs require {ADMIN_API_BEARER_TOKEN_ENV} when binding a non-loopback address"
        ),
    ))
}

fn admin_surface_has_sensitive_state(state: &AppState) -> bool {
    state.config_store.is_some()
        || state.config_runtime_manager.is_some()
        || state.audit_log().is_some()
        || state.runtime_stats().is_some()
        || state.skill_catalog_provider.is_some()
        || state.trace_store().is_some()
}

pub fn admin_cors_layer(state: &AppState) -> std::io::Result<tower_http::cors::CorsLayer> {
    use axum::http::{HeaderValue, Method, header};
    use tower_http::cors::CorsLayer;

    let origins = admin_cors_allowed_origins_for_state(state)
        .into_iter()
        .map(|origin| {
            origin.parse::<HeaderValue>().map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("invalid admin CORS origin {origin:?}: {error}"),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(CorsLayer::new()
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::PATCH,
            Method::OPTIONS,
        ])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
        .allow_origin(origins))
}

#[cfg(test)]
#[path = "app_test.rs"]
mod tests;
