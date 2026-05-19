use super::*;

fn state_for_admin_surface_test(address: &str, admin_api_config: AdminApiConfig) -> AppState {
    use crate::mailbox::{Mailbox, MailboxConfig};
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

    let config = ServerConfig {
        address: address.to_string(),
        ..ServerConfig::default()
    };

    AppState::new(
        runtime,
        mailbox,
        store.clone() as Arc<dyn ThreadRunStore>,
        Arc::new(StubResolver),
        config,
    )
    .with_config_store(store as Arc<dyn ConfigStore>)
    .with_admin_api_config(admin_api_config)
}

#[test]
fn admin_surface_has_sensitive_state_includes_eval_run_store() {
    // Regression: eval_run_store carries persisted prompts + tool args
    // /results and must count as sensitive state — otherwise
    // validate_admin_surface short-circuits past the bearer-token
    // requirement on a deployment that exposes /v1/eval/* with only an
    // eval store attached.
    use awaken_eval::FileEvalRunStore;
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_for_admin_surface_test("0.0.0.0:3000", AdminApiConfig::default());
    state = state
        .with_eval_run_store(Arc::new(FileEvalRunStore::new(tmp.path()).unwrap())
            as Arc<dyn awaken_eval::EvalRunStore>);
    assert!(super::admin_surface_has_sensitive_state(&state));
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
    state = state.with_trace_store(trace_store);

    let err = validate_admin_surface(&state).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    let _ = std::fs::remove_dir_all(&dir);
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
    state.config_store = None;
    state.config_runtime_manager = None;
    let state = state.with_runtime_stats(Arc::new(RuntimeStatsRegistry::new()));

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
    state.config_store = None;
    state.config_runtime_manager = None;
    let state = state.with_audit_log(Arc::new(AuditLogger::new(Arc::new(
        awaken_stores::InMemoryStore::new(),
    ))));

    let error = build_service_router(state).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        error.to_string().contains(ADMIN_API_BEARER_TOKEN_ENV),
        "error should name the required env var, got: {error}"
    );
}

#[test]
fn build_service_router_rejects_skill_catalog_admin_surface_without_token() {
    struct EmptySkillCatalog;
    impl SkillCatalogProvider for EmptySkillCatalog {
        fn list_skills(&self) -> Vec<SkillCatalogEntry> {
            Vec::new()
        }
    }

    let mut state = state_for_admin_surface_test("0.0.0.0:3000", AdminApiConfig::default());
    state.config_store = None;
    state.config_runtime_manager = None;
    state.skill_catalog_provider = Some(Arc::new(EmptySkillCatalog));

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

// ── with_audit_log_from_config reuses pre-set logger ───────────────────

#[tokio::test]
async fn with_audit_log_from_config_reuses_preset_logger() {
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

    let preset_logger = Arc::new(AuditLogger::new(store.clone() as Arc<dyn ConfigStore>));
    let preset_ptr = Arc::as_ptr(&preset_logger);

    let state = AppState::new(
        runtime,
        mailbox,
        store.clone() as Arc<dyn awaken_contract::contract::storage::ThreadRunStore>,
        Arc::new(StubResolver),
        ServerConfig::default(),
    )
    .with_config_store(store as Arc<dyn ConfigStore>)
    .with_audit_log(preset_logger)
    .with_audit_log_from_config();

    let stored = state
        .audit_log()
        .expect("audit_log must be Some after with_audit_log_from_config");
    assert_eq!(
        Arc::as_ptr(&stored),
        preset_ptr,
        "with_audit_log_from_config must reuse the pre-set AuditLogger instance"
    );
}
