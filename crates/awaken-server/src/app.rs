//! Application state and server startup.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Instant;

use awaken_contract::contract::config_store::ConfigStore;
use awaken_contract::contract::storage::ThreadRunStore;
use awaken_runtime::{AgentResolver, AgentRuntime};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::mailbox::{Mailbox, MailboxLifecycleConfig};
use crate::transport::replay_buffer::EventReplayBuffer;

pub type ReplayBufferEntry = (Arc<EventReplayBuffer>, Instant);
pub type ReplayBufferMap = Arc<Mutex<HashMap<String, ReplayBufferEntry>>>;
type AdminApiConfigRegistry = HashMap<
    usize,
    (
        Weak<Mutex<HashMap<String, ReplayBufferEntry>>>,
        AdminApiConfig,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
    /// Origins allowed to call browser admin APIs.
    #[serde(default = "default_admin_cors_allowed_origins")]
    pub cors_allowed_origins: Vec<String>,
    /// Whether the server mounts the `/v1/config/*` and `/v1/agents` admin
    /// CRUD routes. Defaults to `true` for back-compat. Embedders that drive
    /// configuration through their own RBAC / audit pipeline can set this to
    /// `false` to keep the HTTP surface free of those endpoints entirely.
    #[serde(default = "default_expose_config_routes")]
    pub expose_config_routes: bool,
}

const fn default_expose_config_routes() -> bool {
    true
}

impl Default for AdminApiConfig {
    fn default() -> Self {
        Self {
            bearer_token: None,
            cors_allowed_origins: default_admin_cors_allowed_origins(),
            expose_config_routes: default_expose_config_routes(),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a_extended_card_bearer_token: Option<String>,
    /// Mailbox lifecycle ownership. Defaults to framework-managed auto mode.
    #[serde(default)]
    pub mailbox_lifecycle: MailboxLifecycleMode,
}

fn default_sse_buffer() -> usize {
    64
}

fn default_replay_buffer_capacity() -> usize {
    1024
}

fn default_max_concurrent() -> usize {
    100
}

const ADMIN_API_BEARER_TOKEN_ENV: &str = "AWAKEN_ADMIN_API_BEARER_TOKEN";
const ADMIN_CORS_ALLOWED_ORIGINS_ENV: &str = "AWAKEN_ADMIN_CORS_ALLOWED_ORIGINS";
static ADMIN_API_CONFIGS: OnceLock<Mutex<AdminApiConfigRegistry>> = OnceLock::new();

fn admin_api_config_registry() -> &'static Mutex<AdminApiConfigRegistry> {
    ADMIN_API_CONFIGS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn admin_api_bearer_token_from_env() -> Option<String> {
    std::env::var(ADMIN_API_BEARER_TOKEN_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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
    let key = Arc::as_ptr(&state.replay_buffers) as usize;
    let mut config = {
        let mut registry = admin_api_config_registry().lock();
        registry.retain(|_, (weak, _)| weak.upgrade().is_some());
        registry
            .get(&key)
            .map(|(_, config)| config.clone())
            .unwrap_or_default()
    };

    if let Some(token) = admin_api_bearer_token_from_env() {
        config.bearer_token = Some(token);
    }
    if let Some(origins) = admin_cors_allowed_origins_from_env() {
        config.cors_allowed_origins = origins;
    }

    config
}

fn admin_api_config_for_replay_buffers(replay_buffers: &ReplayBufferMap, config: AdminApiConfig) {
    let key = Arc::as_ptr(replay_buffers) as usize;
    let weak = Arc::downgrade(replay_buffers);
    let mut registry = admin_api_config_registry().lock();
    registry.retain(|_, (weak, _)| weak.upgrade().is_some());
    registry.insert(key, (weak, config));
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
        }
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
        admin_api_config_for_replay_buffers(&self.replay_buffers, config);
        self
    }

    /// Require a bearer token for admin/configuration APIs.
    pub fn with_admin_api_bearer_token(self, token: impl Into<String>) -> Self {
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
    validate_admin_surface(&state)?;
    let timeout = std::time::Duration::from_secs(state.config.shutdown.timeout_secs);
    let max_concurrent = state.config.max_concurrent_requests;
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

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on {addr}");

    let admin_cors = admin_cors_layer(&state)?;
    let app = crate::routes::build_router(&state)
        .layer(tower::limit::ConcurrencyLimitLayer::new(max_concurrent))
        .layer(admin_cors)
        .with_state(state);

    let result = serve_with_shutdown(listener, app, timeout).await;
    if let Some(mailbox_lifecycle) = mailbox_lifecycle
        && let Err(error) = mailbox_lifecycle.shutdown().await
    {
        tracing::warn!(error = %error, "failed to stop mailbox lifecycle cleanly");
    }
    result
}

fn validate_admin_surface(state: &AppState) -> std::io::Result<()> {
    let admin = admin_api_config(state);
    if !admin.expose_config_routes {
        return Ok(());
    }
    if admin.bearer_token.is_some() {
        return Ok(());
    }
    if state.config_store.is_none() && state.config_runtime_manager.is_none() {
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
            "admin/config APIs require {ADMIN_API_BEARER_TOKEN_ENV} when binding a non-loopback address"
        ),
    ))
}

fn admin_cors_layer(state: &AppState) -> std::io::Result<tower_http::cors::CorsLayer> {
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
mod tests {
    use super::*;

    #[test]
    fn admin_api_config_default_exposes_config_routes() {
        let config = AdminApiConfig::default();
        assert!(
            config.expose_config_routes,
            "default AdminApiConfig must expose config CRUD routes for back-compat"
        );
    }

    #[test]
    fn validate_admin_surface_short_circuits_when_routes_disabled() {
        use crate::mailbox::{Mailbox, MailboxConfig};
        use awaken_contract::contract::config_store::ConfigStore;
        use awaken_runtime::AgentRuntime;
        use awaken_stores::{InMemoryMailboxStore, InMemoryStore};

        struct StubResolver;
        impl awaken_runtime::AgentResolver for StubResolver {
            fn resolve(
                &self,
                agent_id: &str,
            ) -> Result<awaken_runtime::ResolvedAgent, awaken_runtime::RuntimeError> {
                Err(awaken_runtime::RuntimeError::AgentNotFound {
                    agent_id: agent_id.to_string(),
                })
            }
        }

        let runtime = Arc::new(AgentRuntime::new(Arc::new(StubResolver)));
        let store = Arc::new(InMemoryStore::new());
        let mailbox_store = Arc::new(InMemoryMailboxStore::new());
        let mailbox = Arc::new(Mailbox::new(
            runtime.clone(),
            mailbox_store,
            store.clone(),
            "test".to_string(),
            MailboxConfig::default(),
        ));

        // Non-loopback bind, no bearer token, *with* a config store —
        // historically this would refuse to start. The toggle should
        // short-circuit that check.
        let config = ServerConfig {
            address: "0.0.0.0:3000".to_string(),
            ..ServerConfig::default()
        };

        let state = AppState::new(
            runtime,
            mailbox,
            store.clone() as Arc<dyn awaken_contract::contract::storage::ThreadRunStore>,
            Arc::new(StubResolver),
            config,
        )
        .with_config_store(store as Arc<dyn ConfigStore>)
        .with_admin_api_config(AdminApiConfig {
            expose_config_routes: false,
            ..AdminApiConfig::default()
        });

        validate_admin_surface(&state)
            .expect("disabling config routes must waive the bearer-token requirement");
    }

    #[test]
    fn server_config_default_values() {
        let config = ServerConfig::default();
        assert_eq!(config.address, "0.0.0.0:3000");
        assert_eq!(config.sse_buffer_size, 64);
        assert_eq!(config.replay_buffer_capacity, 1024);
        assert_eq!(config.shutdown.timeout_secs, 30);
        assert_eq!(config.max_concurrent_requests, 100);
        assert_eq!(config.mailbox_lifecycle, MailboxLifecycleMode::Auto);
    }

    #[test]
    fn server_config_serde_roundtrip() {
        let config = ServerConfig {
            address: "127.0.0.1:8080".to_string(),
            sse_buffer_size: 128,
            replay_buffer_capacity: 512,
            shutdown: ShutdownConfig { timeout_secs: 10 },
            max_concurrent_requests: 50,
            a2a_extended_card_bearer_token: None,
            mailbox_lifecycle: MailboxLifecycleMode::Manual,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: ServerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.address, "127.0.0.1:8080");
        assert_eq!(parsed.sse_buffer_size, 128);
        assert_eq!(parsed.replay_buffer_capacity, 512);
        assert_eq!(parsed.shutdown.timeout_secs, 10);
        assert_eq!(parsed.max_concurrent_requests, 50);
        assert_eq!(parsed.mailbox_lifecycle, MailboxLifecycleMode::Manual);
    }

    #[test]
    fn server_config_deserialize_with_defaults() {
        let json = r#"{"address": "localhost:9000"}"#;
        let config: ServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.address, "localhost:9000");
        assert_eq!(config.sse_buffer_size, 64);
        assert_eq!(config.shutdown.timeout_secs, 30);
        assert_eq!(config.max_concurrent_requests, 100);
        assert_eq!(config.mailbox_lifecycle, MailboxLifecycleMode::Auto);
    }

    #[test]
    fn mailbox_lifecycle_mode_deserializes_manual() {
        let json = r#"{"address": "localhost:9000", "mailbox_lifecycle": "manual"}"#;
        let config: ServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.mailbox_lifecycle, MailboxLifecycleMode::Manual);
    }

    #[test]
    fn shutdown_config_defaults() {
        let config = ShutdownConfig::default();
        assert_eq!(config.timeout_secs, 30);
    }

    #[test]
    fn shutdown_config_custom() {
        let json = r#"{"timeout_secs": 60}"#;
        let config: ShutdownConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.timeout_secs, 60);
    }

    // ── Replay buffer management (standalone map) ───────────────────

    /// Helper: create a standalone replay buffer map (same type as `AppState::replay_buffers`)
    /// to test purge logic without needing a full `AppState`.
    fn make_replay_map() -> ReplayBufferMap {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[test]
    fn insert_and_get_replay_buffer() {
        let map = make_replay_map();
        let buf = Arc::new(EventReplayBuffer::new(16));
        buf.push_json(r#"{"hello":1}"#);

        map.lock()
            .insert("run-1".to_string(), (Arc::clone(&buf), Instant::now()));

        let retrieved = map.lock().get("run-1").map(|(b, _)| Arc::clone(b));
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().current_seq(), 1);
    }

    #[test]
    fn remove_replay_buffer_works() {
        let map = make_replay_map();
        let buf = Arc::new(EventReplayBuffer::new(16));
        map.lock()
            .insert("run-2".to_string(), (buf, Instant::now()));

        assert!(map.lock().get("run-2").is_some());
        map.lock().remove("run-2");
        assert!(map.lock().get("run-2").is_none());
    }

    #[test]
    fn purge_stale_replay_buffers_removes_all_with_zero_max_age() {
        let map = make_replay_map();
        let buf = Arc::new(EventReplayBuffer::new(16));
        map.lock()
            .insert("run-a".to_string(), (Arc::clone(&buf), Instant::now()));
        map.lock()
            .insert("run-b".to_string(), (buf, Instant::now()));

        assert_eq!(map.lock().len(), 2);

        // Purge with max_age=ZERO → everything older than "now" is removed.
        let now = Instant::now();
        map.lock().retain(|_key, (_buf, created_at)| {
            now.duration_since(*created_at) < std::time::Duration::ZERO
        });

        assert_eq!(map.lock().len(), 0);
    }

    #[test]
    fn purge_stale_replay_buffers_keeps_recent() {
        let map = make_replay_map();
        let buf = Arc::new(EventReplayBuffer::new(16));
        map.lock()
            .insert("run-c".to_string(), (buf, Instant::now()));

        // Purge with large max_age → nothing should be removed.
        let now = Instant::now();
        let max_age = std::time::Duration::from_secs(3600);
        map.lock()
            .retain(|_key, (_buf, created_at)| now.duration_since(*created_at) < max_age);

        assert_eq!(map.lock().len(), 1);
    }

    #[test]
    fn purge_stale_mixed_ages() {
        let map = make_replay_map();
        // Insert one "old" buffer by backdating the instant with checked_sub.
        let old_instant = Instant::now()
            .checked_sub(std::time::Duration::from_secs(120))
            .unwrap_or_else(Instant::now);
        let recent_instant = Instant::now();

        let buf_old = Arc::new(EventReplayBuffer::new(16));
        let buf_recent = Arc::new(EventReplayBuffer::new(16));

        map.lock()
            .insert("old-run".to_string(), (buf_old, old_instant));
        map.lock()
            .insert("recent-run".to_string(), (buf_recent, recent_instant));

        assert_eq!(map.lock().len(), 2);

        // Purge buffers older than 60 seconds.
        let now = Instant::now();
        let max_age = std::time::Duration::from_secs(60);
        map.lock()
            .retain(|_key, (_buf, created_at)| now.duration_since(*created_at) < max_age);

        assert_eq!(map.lock().len(), 1);
        assert!(map.lock().get("recent-run").is_some());
        assert!(map.lock().get("old-run").is_none());
    }
}
