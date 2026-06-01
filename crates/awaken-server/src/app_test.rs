use super::*;

fn state_for_admin_surface_test(address: &str, admin_api_config: AdminApiConfig) -> ServerState {
    use crate::mailbox::{Mailbox, MailboxConfig};
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

    let runtime = Arc::new(
        awaken_runtime::builder::AgentRuntimeBuilder::new()
            .build_unchecked()
            .expect("build configurable runtime"),
    );
    let store = Arc::new(InMemoryStore::new());
    let mailbox_store = Arc::new(InMemoryMailboxStore::new());
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        mailbox_store,
        store.clone(),
        "test".to_string(),
        MailboxConfig::default(),
    ));

    let config = ServerConfig {
        address: address.to_string(),
        ..ServerConfig::default()
    };

    let mut state = ServerState::new(
        runtime,
        mailbox,
        store.clone() as Arc<dyn ThreadRunStore>,
        Arc::new(StubResolver),
        config,
    );
    state.admin.admin_api_config = admin_api_config;
    state
}

struct EmptyEvalRunStore;

impl awaken_eval::EvalRunStore for EmptyEvalRunStore {
    fn write(&self, _run: &awaken_eval::EvalRun) -> Result<(), awaken_eval::EvalRunStoreError> {
        Ok(())
    }

    fn read(&self, run_id: &str) -> Result<awaken_eval::EvalRun, awaken_eval::EvalRunStoreError> {
        Err(awaken_eval::EvalRunStoreError::NotFound(run_id.to_string()))
    }

    fn list(
        &self,
        _filter: &awaken_eval::EvalRunFilter,
    ) -> Result<Vec<awaken_eval::EvalRunSummary>, awaken_eval::EvalRunStoreError> {
        Ok(Vec::new())
    }

    fn prune(&self, _older_than_secs: u64) -> Result<u64, awaken_eval::EvalRunStoreError> {
        Ok(0)
    }
}

struct EmptyTraceStore;

impl awaken_ext_observability::trace_store::TraceStore for EmptyTraceStore {
    fn append(
        &self,
        _run_id: &str,
        _event: &awaken_ext_observability::MetricsEvent,
    ) -> Result<(), awaken_ext_observability::trace_store::TraceStoreError> {
        Ok(())
    }

    fn read(
        &self,
        run_id: &str,
    ) -> Result<
        Vec<awaken_ext_observability::MetricsEvent>,
        awaken_ext_observability::trace_store::TraceStoreError,
    > {
        Err(
            awaken_ext_observability::trace_store::TraceStoreError::NotFound {
                run_id: run_id.to_string(),
            },
        )
    }

    fn list(
        &self,
        _filter: &awaken_ext_observability::trace_store::TraceFilter,
    ) -> Result<
        Vec<awaken_ext_observability::trace_store::RunSummary>,
        awaken_ext_observability::trace_store::TraceStoreError,
    > {
        Ok(Vec::new())
    }

    fn mark_referenced(
        &self,
        _run_id: &str,
        _by: awaken_ext_observability::trace_store::ReferenceKind,
    ) -> Result<(), awaken_ext_observability::trace_store::TraceStoreError> {
        Ok(())
    }

    fn prune(
        &self,
        _older_than: std::time::SystemTime,
        _except_referenced: &std::collections::HashSet<String>,
    ) -> Result<u64, awaken_ext_observability::trace_store::TraceStoreError> {
        Ok(0)
    }

    fn write_index_for_run(
        &self,
        _run_id: &str,
        _summary: &awaken_ext_observability::trace_store::RunSummary,
    ) -> Result<(), awaken_ext_observability::trace_store::TraceStoreError> {
        Ok(())
    }
}

#[test]
fn admin_api_config_default_exposes_config_routes() {
    let config = AdminApiConfig::default();
    assert!(
        config.expose_config_routes,
        "default AdminApiConfig must expose config CRUD routes for back-compat"
    );
}

#[test]
fn admin_api_config_debug_does_not_leak_bearer_token() {
    let config = AdminApiConfig {
        bearer_token: Some("admin-bearer-secret-12345".into()),
        ..AdminApiConfig::default()
    };
    let debug = format!("{config:?}");
    assert!(
        !debug.contains("admin-bearer-secret-12345"),
        "AdminApiConfig Debug must redact bearer_token, got: {debug}"
    );
}

#[test]
fn server_config_debug_does_not_leak_a2a_extended_card_bearer_token() {
    let config = ServerConfig {
        a2a_extended_card_bearer_token: Some("a2a-secret-67890".into()),
        ..ServerConfig::default()
    };
    let debug = format!("{config:?}");
    assert!(
        !debug.contains("a2a-secret-67890"),
        "ServerConfig Debug must redact a2a_extended_card_bearer_token, got: {debug}"
    );
}

#[test]
#[allow(deprecated)]
fn app_state_alias_remains_usable() {
    let state: AppState = state_for_admin_surface_test("127.0.0.1:0", AdminApiConfig::default());
    assert_eq!(state.server_config.address, "127.0.0.1:0");
}

#[test]
fn server_state_compat_builders_update_module_state() {
    let started_at = std::time::Instant::now();
    let stats = Arc::new(RuntimeStatsRegistry::new());
    let state = state_for_admin_surface_test("127.0.0.1:0", AdminApiConfig::default())
        .with_admin_api_bearer_token("admin-token")
        .with_admin_cors_allowed_origins(vec!["https://console.example".to_string()])
        .with_audit_log_config(AuditLogConfig {
            enabled: false,
            retention_days: 7,
            sweep_interval_secs: 60,
        })
        .with_runtime_stats(stats.clone())
        .with_started_at(started_at);

    let admin = state.admin_api_config();
    assert_eq!(
        admin
            .bearer_token
            .as_ref()
            .map(|token| token.expose_secret()),
        Some("admin-token")
    );
    assert_eq!(
        admin.cors_allowed_origins,
        vec!["https://console.example".to_string()]
    );
    assert_eq!(state.audit_log_config().retention_days, 7);
    assert_eq!(state.started_at(), started_at);
    assert!(Arc::ptr_eq(&state.runtime_stats().unwrap(), &stats));
}

#[tokio::test]
async fn server_state_config_compat_builders_mount_config_module() {
    use awaken_server_contract::contract::config_store::ConfigStore;
    use awaken_stores::InMemoryStore;

    let store = Arc::new(InMemoryStore::new()) as Arc<dyn ConfigStore>;
    let state = state_for_admin_surface_test("127.0.0.1:0", AdminApiConfig::default())
        .with_config_store(store)
        .with_audit_log_from_config();

    assert!(state.config_module().is_some());
    assert!(
        state.audit_log().is_some(),
        "with_audit_log_from_config should attach a logger once config is mounted"
    );
}

#[test]
fn mounted_modules_reports_only_route_mounted_optional_modules() {
    use awaken_server_contract::contract::config_store::ConfigStore;
    use awaken_stores::InMemoryStore;

    let eval_only_state = state_for_admin_surface_test(
        "127.0.0.1:0",
        AdminApiConfig {
            expose_eval_routes: true,
            ..AdminApiConfig::default()
        },
    )
    .with_eval_run_store(Arc::new(EmptyEvalRunStore));

    assert!(
        !eval_only_state.mounted_modules().contains(&"eval"),
        "eval routes require both EvalModuleState and ConfigModuleState"
    );

    let config_store = Arc::new(InMemoryStore::new()) as Arc<dyn ConfigStore>;
    let mut fully_wired_state = state_for_admin_surface_test(
        "127.0.0.1:0",
        AdminApiConfig {
            expose_config_routes: true,
            expose_eval_routes: true,
            expose_trace_routes: true,
            ..AdminApiConfig::default()
        },
    )
    .with_config_store(config_store)
    .with_eval_run_store(Arc::new(EmptyEvalRunStore));
    fully_wired_state.trace = Some(TraceModuleState {
        trace_store: Arc::new(EmptyTraceStore),
    });

    let modules = fully_wired_state.mounted_modules();
    assert!(modules.contains(&"config"));
    assert!(modules.contains(&"eval"));
    assert!(modules.contains(&"trace"));

    fully_wired_state
        .admin
        .admin_api_config
        .expose_config_routes = false;
    fully_wired_state.admin.admin_api_config.expose_eval_routes = false;
    fully_wired_state.admin.admin_api_config.expose_trace_routes = false;

    let gated_modules = fully_wired_state.mounted_modules();
    assert!(!gated_modules.contains(&"config"));
    assert!(!gated_modules.contains(&"eval"));
    assert!(!gated_modules.contains(&"trace"));
}

#[test]
fn validate_admin_surface_rejects_trace_routes_without_token_on_non_loopback() {
    // Regression for issue 1 residual: even with config routes off, an
    // exposed trace store on a non-loopback bind without a bearer token
    // must fail startup. Previously the validator short-circuited on
    // `!expose_config_routes` and never inspected trace routes.
    use crate::services::trace_retention; // pulls TraceStore via re-export
    let _ = trace_retention::RetentionConfig::default(); // sanity

    // Build a state with a trace store attached.
    let mut state = state_for_admin_surface_test(
        "0.0.0.0:3000",
        AdminApiConfig {
            expose_config_routes: false,
            expose_trace_routes: true,
            bearer_token: None,
            ..AdminApiConfig::default()
        },
    );
    let dir = std::env::temp_dir().join(format!(
        "awaken-validate-admin-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let trace_store: Arc<dyn TraceStore> =
        Arc::new(awaken_ext_observability::trace_store::file::FileTraceStore::new(&dir).unwrap());
    state.trace = Some(TraceModuleState { trace_store });

    let err = validate_admin_surface(&state).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn validate_admin_surface_rejects_eval_store_without_config_module() {
    let state = state_for_admin_surface_test(
        "127.0.0.1:0",
        AdminApiConfig {
            expose_eval_routes: true,
            bearer_token: Some(RedactedString::new("admin-token")),
            ..AdminApiConfig::default()
        },
    )
    .with_eval_run_store(Arc::new(EmptyEvalRunStore));

    let err = validate_admin_surface(&state).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        err.to_string().contains("with_config_store"),
        "error should tell operators how to wire eval routes, got: {err}"
    );
}

#[test]
fn validate_admin_surface_short_circuits_when_routes_disabled() {
    // Disabling every sensitive route surface (config + trace + eval)
    // waives the bearer-token requirement on non-loopback binds.
    let state = state_for_admin_surface_test(
        "0.0.0.0:3000",
        AdminApiConfig {
            expose_config_routes: false,
            expose_trace_routes: false,
            expose_eval_routes: false,
            ..AdminApiConfig::default()
        },
    );

    validate_admin_surface(&state)
        .expect("disabling all sensitive routes must waive the bearer-token requirement");
}

#[test]
fn build_service_router_rejects_non_loopback_admin_surface_without_token() {
    let state = state_for_admin_surface_test("0.0.0.0:3000", AdminApiConfig::default());

    let error = build_service_router(state).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        error.to_string().contains(ADMIN_API_BEARER_TOKEN_ENV),
        "error should name the required env var, got: {error}"
    );
}

#[test]
fn build_service_router_rejects_runtime_stats_admin_surface_without_token() {
    let mut state = state_for_admin_surface_test("0.0.0.0:3000", AdminApiConfig::default());
    state.config = None;
    state.run.runtime_stats = Some(Arc::new(RuntimeStatsRegistry::new()));

    let error = build_service_router(state).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        error.to_string().contains(ADMIN_API_BEARER_TOKEN_ENV),
        "error should name the required env var, got: {error}"
    );
}

#[test]
fn build_service_router_rejects_audit_log_admin_surface_without_token() {
    let mut state = state_for_admin_surface_test("0.0.0.0:3000", AdminApiConfig::default());
    state.config = None;

    let error = build_service_router(state).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        error.to_string().contains(ADMIN_API_BEARER_TOKEN_ENV),
        "error should name the required env var, got: {error}"
    );
}

#[test]
fn build_service_router_rejects_skill_catalog_admin_surface_without_token() {
    let mut state = state_for_admin_surface_test("0.0.0.0:3000", AdminApiConfig::default());
    state.config = None;

    let error = build_service_router(state).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        error.to_string().contains(ADMIN_API_BEARER_TOKEN_ENV),
        "error should name the required env var, got: {error}"
    );
}

#[test]
fn build_service_router_allows_non_loopback_admin_surface_with_token() {
    let state = state_for_admin_surface_test(
        "0.0.0.0:3000",
        AdminApiConfig {
            bearer_token: Some(RedactedString::new("admin-token")),
            ..AdminApiConfig::default()
        },
    );

    let _ =
        build_service_router(state).expect("bearer token must allow non-loopback admin surface");
}

#[tokio::test(flavor = "current_thread")]
async fn route_layer_auth_uses_env_overlay_bearer_token() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    with_admin_bearer_token_env_override("env-admin-token", async {
        let state = state_for_admin_surface_test("127.0.0.1:0", AdminApiConfig::default());
        assert!(
            state.admin.admin_api_config.bearer_token.is_none(),
            "test must prove route-layer auth uses effective env overlay, not raw module state"
        );

        let router =
            build_service_router(state).expect("env bearer token must satisfy startup auth");
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/v1/system/info")
                    .header("authorization", "Bearer env-admin-token")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_ne!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "valid env bearer token must pass route-layer middleware"
        )
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn route_layer_auth_rejects_missing_or_wrong_env_overlay_token() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    with_admin_bearer_token_env_override("env-admin-token", async {
        let state = state_for_admin_surface_test("127.0.0.1:0", AdminApiConfig::default());
        let router =
            build_service_router(state).expect("env bearer token must satisfy startup auth");

        for (label, authorization) in [
            ("missing", None),
            ("wrong", Some("Bearer wrong-token")),
            ("non-bearer", Some("Basic env-admin-token")),
        ] {
            let mut builder = Request::builder().uri("/v1/system/info");
            if let Some(value) = authorization {
                builder = builder.header("authorization", value);
            }
            let response = router
                .clone()
                .oneshot(builder.body(Body::empty()).expect("request"))
                .await
                .expect("response");
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{label} auth must be rejected by route-layer middleware"
            );
        }
    })
    .await;
}

#[test]
fn build_service_router_allows_non_loopback_when_admin_surface_disabled() {
    // Eval routes are now treated as sensitive in their own right (live
    // provider calls), so disabling config alone no longer waives the
    // requirement — the test must disable every sensitive surface.
    let state = state_for_admin_surface_test(
        "0.0.0.0:3000",
        AdminApiConfig {
            expose_config_routes: false,
            expose_trace_routes: false,
            expose_eval_routes: false,
            ..AdminApiConfig::default()
        },
    );

    let _ = build_service_router(state)
        .expect("disabled admin surface must not require a bearer token");
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
        eval_limits: crate::eval_limits::EvalLimits::default(),
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

/// Helper: create a standalone replay buffer map (same type as `ServerState::replay_buffers`)
/// to test purge logic without needing a full `ServerState`.
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

// ── effective_sweep_interval ────────────────────────────────────────────

#[test]
fn sweep_interval_zero_clamps_to_60s() {
    let duration = effective_sweep_interval(0);
    assert_eq!(
        duration,
        std::time::Duration::from_secs(60),
        "zero sweep interval must clamp to 60 s"
    );
}

#[test]
fn sweep_interval_normal_value_is_respected() {
    let duration = effective_sweep_interval(3600);
    assert_eq!(duration, std::time::Duration::from_secs(3600));
}

#[test]
fn sweep_interval_small_nonzero_is_respected() {
    // Values 1–9 should warn but still be used as-is.
    let duration = effective_sweep_interval(5);
    assert_eq!(duration, std::time::Duration::from_secs(5));
}

// ── ConfigModuleState carries pre-set audit logger ──────────────────────

#[tokio::test]
async fn config_module_state_reuses_preset_logger() {
    use crate::mailbox::{Mailbox, MailboxConfig};
    use awaken_server_contract::contract::config_store::ConfigStore;
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

    let runtime = Arc::new(
        awaken_runtime::builder::AgentRuntimeBuilder::new()
            .build_unchecked()
            .expect("build configurable runtime"),
    );
    let store = Arc::new(InMemoryStore::new());
    let mailbox_store = Arc::new(InMemoryMailboxStore::new());
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        mailbox_store,
        store.clone(),
        "test".to_string(),
        MailboxConfig::default(),
    ));

    let preset_logger = Arc::new(AuditLogger::new(store.clone() as Arc<dyn ConfigStore>));
    let preset_ptr = Arc::as_ptr(&preset_logger);

    let mut state = ServerState::new(
        runtime,
        mailbox,
        store.clone() as Arc<dyn awaken_server_contract::contract::storage::ThreadRunStore>,
        Arc::new(StubResolver),
        ServerConfig::default(),
    );
    let config_store = store as Arc<dyn ConfigStore>;
    let manager = Arc::new(
        crate::services::config_runtime::ConfigRuntimeManager::new(
            state.run.runtime.clone(),
            config_store.clone(),
        )
        .expect("config runtime manager"),
    );
    state.config =
        Some(ConfigModuleState::new(config_store, manager).with_audit_log(preset_logger));

    let stored = state
        .audit_log()
        .expect("audit_log must be Some when mounted on ConfigModuleState");
    assert_eq!(
        Arc::as_ptr(&stored),
        preset_ptr,
        "ConfigModuleState must expose the pre-set AuditLogger instance"
    );
}

fn local_component_state() -> ServerState {
    use awaken_stores::InMemoryStore;

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

    let runtime = Arc::new(
        awaken_runtime::builder::AgentRuntimeBuilder::new()
            .build_unchecked()
            .expect("build runtime"),
    );
    let store = Arc::new(InMemoryStore::new());
    ServerState::new_with_local_mailbox(
        runtime,
        store as Arc<dyn ThreadRunStore>,
        Arc::new(StubResolver),
        ServerConfig::default(),
    )
}

#[test]
fn server_state_with_local_mailbox_uses_local_components() {
    let state = local_component_state();

    assert!(
        crate::protocol_replay_state::a2a_push_webhook_outbox_for_buffers(
            &state.protocol.replay_buffers
        )
        .is_some(),
        "A2A push outbox should default to a local in-memory implementation"
    );
}

#[test]
fn with_protocol_preserves_default_a2a_push_outbox_for_new_replay_buffers() {
    let state = local_component_state();
    let protocol = ProtocolModuleState::new();

    assert!(
        crate::protocol_replay_state::a2a_push_webhook_outbox_for_buffers(&protocol.replay_buffers)
            .is_none(),
        "fresh protocol module should carry an outbox but not register its buffers until mounted"
    );

    let state = state.with_protocol(protocol);

    assert!(
        crate::protocol_replay_state::a2a_push_webhook_outbox_for_buffers(
            &state.protocol.replay_buffers
        )
        .is_some(),
        "with_protocol should attach the local A2A push outbox to the replacement buffers"
    );
}

#[test]
fn a2a_push_outbox_can_replace_default_local_outbox() {
    use awaken_server_contract::contract::outbox::OutboxStore;
    use awaken_stores::InMemoryOutboxStore;

    let state = local_component_state();
    let default_outbox = crate::protocol_replay_state::a2a_push_webhook_outbox_for_buffers(
        &state.protocol.replay_buffers,
    )
    .expect("default A2A push outbox should be attached");

    let replacement: Arc<dyn OutboxStore> = Arc::new(InMemoryOutboxStore::new());
    assert!(
        !Arc::ptr_eq(&default_outbox, &replacement),
        "replacement should be a distinct outbox instance"
    );

    let state = crate::protocol_replay_state::with_a2a_push_webhook_relay(
        state,
        replacement.clone(),
        crate::protocol_replay_state::A2aPushWebhookRelayConfig::default(),
    )
    .expect("replace A2A push outbox");

    let attached = crate::protocol_replay_state::a2a_push_webhook_outbox_for_buffers(
        &state.protocol.replay_buffers,
    )
    .expect("replacement A2A push outbox should be attached");
    assert!(
        Arc::ptr_eq(&attached, &replacement),
        "explicit A2A push outbox should replace the local default"
    );
    assert!(
        Arc::ptr_eq(&state.protocol.a2a_push_outbox, &replacement),
        "server state should carry the explicit A2A push outbox as a module dependency"
    );
}

#[test]
fn server_state_method_injects_a2a_push_outbox_dependency() {
    use awaken_server_contract::contract::outbox::OutboxStore;
    use awaken_stores::InMemoryOutboxStore;

    let replacement: Arc<dyn OutboxStore> = Arc::new(InMemoryOutboxStore::new());
    let state = local_component_state()
        .with_a2a_push_webhook_outbox(replacement.clone())
        .expect("inject A2A push outbox");

    let attached = crate::protocol_replay_state::a2a_push_webhook_outbox_for_buffers(
        &state.protocol.replay_buffers,
    )
    .expect("injected A2A push outbox should be registered");
    assert!(Arc::ptr_eq(&attached, &replacement));
    assert!(Arc::ptr_eq(&state.protocol.a2a_push_outbox, &replacement));
}

#[tokio::test(flavor = "current_thread")]
async fn mailbox_routes_remain_available_with_local_mailbox() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let state = local_component_state();
    let app = crate::routes::build_router(&state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/threads/local-thread/mailbox")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
}
