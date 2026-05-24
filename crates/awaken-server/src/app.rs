//! Application state and server startup.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use awaken_contract::RedactedString;
use awaken_contract::contract::config_store::ConfigStore;
use awaken_contract::contract::event_store::EventStore;
use awaken_contract::contract::storage::ThreadRunStore;
use awaken_ext_observability::RuntimeStatsRegistry;
use awaken_runtime::credentials::{AwakenCredentialBroker, CredentialBroker};
use awaken_runtime::{AgentResolver, AgentRuntime};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use awaken_ext_observability::trace_store::TraceStore;

use crate::mailbox::{Mailbox, MailboxLifecycleConfig};
mod modules;
use crate::services::audit_log::AuditLogger;
use crate::transport::replay_buffer::EventReplayBuffer;
pub use modules::{
    AdminModuleState, AdminRunRoutesState, ConfigModuleState, ConfigRoutesState, EvalModuleState,
    EvalRoutesState, EventModuleState, ProtocolModuleState, ProtocolRoutesState, RunModuleState,
    RunRoutesState, SystemRoutesState, TraceModuleState, TraceRoutesState,
};

pub type ReplayBufferEntry = (Arc<EventReplayBuffer>, Instant);
pub type ReplayBufferMap = Arc<Mutex<HashMap<String, ReplayBufferEntry>>>;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShutdownConfig {
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

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MailboxLifecycleMode {
    #[default]
    Auto,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdminApiConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<RedactedString>,
    #[serde(default = "default_admin_cors_allowed_origins")]
    pub cors_allowed_origins: Vec<String>,
    #[serde(default = "default_expose_config_routes")]
    pub expose_config_routes: bool,
    #[serde(default = "default_expose_trace_routes")]
    pub expose_trace_routes: bool,
    #[serde(default = "default_expose_eval_routes")]
    pub expose_eval_routes: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditLogConfig {
    #[serde(default = "default_audit_log_enabled")]
    pub enabled: bool,
    #[serde(default = "default_audit_retention_days")]
    pub retention_days: u32,
    #[serde(default = "default_audit_sweep_interval_secs")]
    pub sweep_interval_secs: u64,
}

const fn default_expose_config_routes() -> bool {
    true
}
const fn default_expose_trace_routes() -> bool {
    false // F20: opt-in (traces expose prompts/tool args)
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub address: String,
    #[serde(default = "default_sse_buffer")]
    pub sse_buffer_size: usize,
    #[serde(default = "default_replay_buffer_capacity")]
    pub replay_buffer_capacity: usize,
    #[serde(default)]
    pub shutdown: ShutdownConfig,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_requests: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a_extended_card_bearer_token: Option<RedactedString>,
    #[serde(default)]
    pub mailbox_lifecycle: MailboxLifecycleMode,
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

pub(crate) fn admin_api_config(state: &ServerState) -> AdminApiConfig {
    let mut config = state.admin_api_config.clone();

    if let Some(token) = admin_api_bearer_token_from_env() {
        config.bearer_token = Some(token);
    }
    if let Some(origins) = admin_cors_allowed_origins_from_env() {
        config.cors_allowed_origins = origins;
    }

    config
}

fn admin_cors_allowed_origins_for_state(state: &ServerState) -> Vec<String> {
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

#[derive(Clone)]
pub struct ServerState {
    pub runtime: Arc<AgentRuntime>,
    pub mailbox: Arc<Mailbox>,
    pub store: Arc<dyn ThreadRunStore>,
    pub resolver: Arc<dyn AgentResolver>,
    pub config: ServerConfig,
    pub config_store: Option<Arc<dyn ConfigStore>>,
    pub config_runtime_manager: Option<Arc<crate::services::config_runtime::ConfigRuntimeManager>>,
    pub skill_catalog_provider: Option<Arc<dyn SkillCatalogProvider>>,
    pub replay_buffers: ReplayBufferMap,
    pub mcp_http: Arc<crate::protocols::mcp::http::McpHttpState>,
    pub(crate) admin_api_config: AdminApiConfig,
    pub(crate) audit_log_config: AuditLogConfig,
    pub(crate) runtime_stats: Option<Arc<RuntimeStatsRegistry>>,
    pub(crate) audit_log: Option<Arc<AuditLogger>>,
    pub(crate) trace_store: Option<Arc<dyn TraceStore>>,
    pub(crate) event_store: Option<Arc<dyn EventStore>>,
    pub(crate) eval_run_store: Option<Arc<dyn awaken_eval::EvalRunStore>>,
    pub(crate) started_at: Instant,
    pub(crate) credential_broker: Arc<dyn CredentialBroker>,
}

pub type AppState = ServerState;

impl ServerState {
    pub fn new(
        runtime: Arc<AgentRuntime>,
        mailbox: Arc<Mailbox>,
        store: Arc<dyn ThreadRunStore>,
        resolver: Arc<dyn AgentResolver>,
        config: ServerConfig,
    ) -> Self {
        Self {
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
            admin_api_config: AdminApiConfig::default(),
            audit_log_config: AuditLogConfig::default(),
            runtime_stats: None,
            audit_log: None,
            trace_store: None,
            event_store: None,
            eval_run_store: None,
            started_at: Instant::now(),
            credential_broker: Arc::new(AwakenCredentialBroker::new()),
        }
    }

    pub fn with_credential_broker(
        mut self,
        broker: Arc<dyn awaken_runtime::credentials::CredentialBroker>,
    ) -> Self {
        self.credential_broker = broker;
        self
    }

    pub fn credential_broker(&self) -> Arc<dyn CredentialBroker> {
        self.credential_broker.clone()
    }

    #[must_use]
    pub fn with_runtime_stats(mut self, registry: Arc<RuntimeStatsRegistry>) -> Self {
        self.runtime_stats = Some(registry);
        self
    }

    pub fn runtime_stats(&self) -> Option<Arc<RuntimeStatsRegistry>> {
        self.runtime_stats.clone()
    }

    pub fn with_config_store(mut self, store: Arc<dyn ConfigStore>) -> Self {
        self.config_store = Some(store);
        self
    }

    pub fn with_config_runtime_manager(
        mut self,
        manager: Arc<crate::services::config_runtime::ConfigRuntimeManager>,
    ) -> Self {
        self.config_runtime_manager = Some(manager);
        self
    }

    pub fn with_skill_catalog_provider(mut self, provider: Arc<dyn SkillCatalogProvider>) -> Self {
        self.skill_catalog_provider = Some(provider);
        self
    }

    pub fn with_admin_api_config(mut self, config: AdminApiConfig) -> Self {
        self.admin_api_config = config;
        self
    }

    pub fn with_admin_api_bearer_token(self, token: impl Into<RedactedString>) -> Self {
        let mut config = admin_api_config(&self);
        config.bearer_token = Some(token.into());
        self.with_admin_api_config(config)
    }

    pub fn with_admin_cors_allowed_origins(self, origins: Vec<String>) -> Self {
        let mut config = admin_api_config(&self);
        config.cors_allowed_origins = origins;
        self.with_admin_api_config(config)
    }

    pub fn admin_api_config(&self) -> AdminApiConfig {
        admin_api_config(self)
    }

    #[must_use]
    pub fn with_audit_log_config(mut self, config: AuditLogConfig) -> Self {
        self.audit_log_config = config;
        self
    }

    pub fn audit_log_config(&self) -> AuditLogConfig {
        self.audit_log_config
    }

    #[must_use]
    pub fn with_audit_log(mut self, logger: Arc<AuditLogger>) -> Self {
        self.audit_log = Some(logger);
        self
    }

    pub fn audit_log(&self) -> Option<Arc<AuditLogger>> {
        self.audit_log.clone()
    }

    #[must_use]
    pub fn with_trace_store(mut self, store: Arc<dyn TraceStore>) -> Self {
        self.trace_store = Some(store);
        self
    }

    pub fn trace_store(&self) -> Option<Arc<dyn TraceStore>> {
        self.trace_store.clone()
    }

    #[must_use]
    pub fn with_event_store(mut self, store: Arc<dyn EventStore>) -> Self {
        self.event_store = Some(store);
        self
    }

    pub fn event_store(&self) -> Option<Arc<dyn EventStore>> {
        self.event_store.clone()
    }

    #[must_use]
    pub fn with_eval_run_store(mut self, store: Arc<dyn awaken_eval::EvalRunStore>) -> Self {
        self.eval_run_store = Some(store);
        self
    }

    pub fn eval_run_store(&self) -> Option<Arc<dyn awaken_eval::EvalRunStore>> {
        self.eval_run_store.clone()
    }

    #[must_use]
    pub fn with_audit_log_from_config(mut self) -> Self {
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
                self.audit_log = Some(new_logger.clone());
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

    pub fn started_at(&self) -> Instant {
        self.started_at
    }

    #[must_use]
    pub fn with_started_at(mut self, started_at: Instant) -> Self {
        self.started_at = started_at;
        self
    }

    pub fn insert_replay_buffer(&self, key: String, buffer: Arc<EventReplayBuffer>) {
        self.replay_buffers
            .lock()
            .insert(key, (buffer, Instant::now()));
    }

    pub fn get_replay_buffer(&self, key: &str) -> Option<Arc<EventReplayBuffer>> {
        self.replay_buffers
            .lock()
            .get(key)
            .map(|(buf, _)| Arc::clone(buf))
    }

    pub fn remove_replay_buffer(&self, key: &str) {
        self.replay_buffers.lock().remove(key);
    }

    pub fn purge_stale_replay_buffers(&self, max_age: std::time::Duration) {
        let now = Instant::now();
        let mut buffers = self.replay_buffers.lock();
        let before = buffers.len();
        buffers.retain(|_key, (_buf, created_at)| {
            let age = now.duration_since(*created_at);
            if age < max_age {
                return true;
            }
            false
        });
        let purged = before - buffers.len();
        if purged > 0 {
            tracing::debug!(purged, "purged stale replay buffers");
        }
    }
}

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

pub async fn serve(state: ServerState) -> std::io::Result<()> {
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

    // Retention belongs to storage, so spawn it even if trace routes are hidden.
    let _retention_handle = state.trace_store().map(|store| {
        crate::services::trace_retention::spawn_retention_loop(
            store,
            crate::services::trace_retention::RetentionConfig::default(),
        )
    });
    let protocol_relays = crate::protocol_replay_state::start_protocol_relays(&state)
        .await
        .map_err(|error| {
            std::io::Error::other(format!("failed to start protocol relays: {error}"))
        })?;

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
    protocol_relays.shutdown().await;
    result
}

pub fn build_service_router(state: ServerState) -> std::io::Result<axum::Router> {
    validate_admin_surface(&state)?;
    let max_concurrent = state.config.max_concurrent_requests;
    let admin_cors = admin_cors_layer(&state)?;
    Ok(crate::routes::build_router(&state)
        .layer(tower::limit::ConcurrencyLimitLayer::new(max_concurrent))
        .layer(admin_cors))
}

pub fn validate_admin_surface(state: &ServerState) -> std::io::Result<()> {
    crate::eval_limits::validate_eval_limits(&state.config.eval_limits)?;
    let admin = admin_api_config(state);
    let any_admin_route_exposed =
        admin.expose_config_routes || admin.expose_trace_routes || admin.expose_eval_routes;
    if !any_admin_route_exposed {
        return Ok(());
    }
    if admin.bearer_token.is_some() {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!(
            "admin, config, trace, and eval APIs require {ADMIN_API_BEARER_TOKEN_ENV} when any admin surface is exposed"
        ),
    ))
}

pub fn admin_cors_layer(state: &ServerState) -> std::io::Result<tower_http::cors::CorsLayer> {
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
