use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime::AgentRuntime;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_server::app::{ConfigModuleState, ServerConfig, ServerState};
use awaken_server::mailbox::{Mailbox, MailboxConfig};
#[cfg(feature = "nats")]
use awaken_server::mailbox::{MailboxLifecycleConfig, MailboxStartupRecoveryConfig};
use awaken_server::routes::build_router;
use awaken_server::services::config_runtime::{
    ConfigRuntimeError, ConfigRuntimeManager, ProviderExecutorFactory,
};
use awaken_server_contract::contract::commit_coordinator::CommitCoordinator;
use awaken_server_contract::contract::config_store::ConfigStore;
use awaken_server_contract::contract::executor::{
    InferenceExecutionError, InferenceRequest, LlmExecutor,
};
use awaken_server_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
#[cfg(feature = "nats")]
use awaken_server_contract::contract::lifecycle::RunStatus;
#[cfg(feature = "nats")]
use awaken_server_contract::contract::message::{Message, MessageMetadata};
#[cfg(feature = "nats")]
use awaken_server_contract::contract::storage::RunRecord;
use awaken_server_contract::contract::storage::{RunStore, ThreadRunStore, ThreadStore};
use awaken_server_contract::{AgentSpec, BuiltinSeedSet, BuiltinSpec, ModelSpec, ProviderSpec};
use awaken_stores::{FileStore, InMemoryMailboxStore, PostgresStore};
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

const ADMIN_TOKEN: &str = "config-backends-admin-token";

#[cfg(feature = "nats")]
use awaken_stores::{
    InMemoryStore, NatsBufferedThreadConfig, NatsBufferedThreadStore, NatsMailboxConfig,
    NatsMailboxStore,
};
#[cfg(feature = "nats")]
use testcontainers::{ContainerAsync, GenericImage, ImageExt, core::WaitFor, runners::AsyncRunner};

struct ImmediateExecutor;

#[async_trait]
impl LlmExecutor for ImmediateExecutor {
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
        "immediate"
    }
}

struct StubProviderFactory;

impl ProviderExecutorFactory for StubProviderFactory {
    fn build(&self, spec: &ProviderSpec) -> Result<Arc<dyn LlmExecutor>, ConfigRuntimeError> {
        if spec.adapter.eq_ignore_ascii_case("stub") {
            return Ok(Arc::new(ImmediateExecutor));
        }

        Err(ConfigRuntimeError::UnsupportedProviderAdapter(
            spec.adapter.clone(),
        ))
    }
}

struct TestApp<S> {
    router: axum::Router,
    runtime: Arc<AgentRuntime>,
    #[allow(dead_code)]
    mailbox: Arc<Mailbox>,
    store: Arc<S>,
}

fn bootstrap_provider() -> ProviderSpec {
    ProviderSpec {
        id: "bootstrap".into(),
        adapter: "stub".into(),
        ..Default::default()
    }
}

fn bootstrap_model() -> ModelSpec {
    ModelSpec::new("bootstrap", "bootstrap", "bootstrap-model")
}

fn bootstrap_agent() -> AgentSpec {
    AgentSpec {
        id: "bootstrap".into(),
        model_id: "bootstrap".into(),
        system_prompt: "bootstrap agent".into(),
        max_rounds: 1,
        ..Default::default()
    }
}

async fn make_app<S, F>(store: Arc<S>, server_name: &str, make_coordinator: F) -> TestApp<S>
where
    S: ConfigStore + ThreadRunStore + Send + Sync + 'static,
    F: FnOnce(Arc<S>) -> Arc<dyn CommitCoordinator>,
{
    let mailbox_store = Arc::new(InMemoryMailboxStore::new());
    make_app_with_mailbox(store, mailbox_store, server_name, make_coordinator).await
}

async fn make_app_with_mailbox<S, M, F>(
    store: Arc<S>,
    mailbox_store: Arc<M>,
    server_name: &str,
    make_coordinator: F,
) -> TestApp<S>
where
    S: ConfigStore + ThreadRunStore + Send + Sync + 'static,
    M: awaken_server_contract::contract::mailbox::MailboxStore + Send + Sync + 'static,
    F: FnOnce(Arc<S>) -> Arc<dyn CommitCoordinator>,
{
    // ADR-0038 D7: the runtime's CommitCoordinator must wrap the same
    // ThreadRunStore handle as the mailbox uses. The backend-specific
    // coordinator factory is supplied by the caller so FileStore tests can
    // pair with FileCommitCoordinator, etc.
    let coordinator = make_coordinator(Arc::clone(&store));
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(ImmediateExecutor))
            .with_thread_run_store(store.clone() as Arc<dyn ThreadRunStore>)
            .with_commit_coordinator(coordinator)
            .build()
            .expect("build runtime"),
    );

    let config_store = store.clone() as Arc<dyn ConfigStore>;
    let manager = Arc::new(
        ConfigRuntimeManager::new(runtime.clone(), config_store.clone())
            .expect("config runtime manager")
            .with_provider_factory(Arc::new(StubProviderFactory)),
    );
    let seed = BuiltinSeedSet {
        binary_version: "test".to_string(),
        specs: vec![
            BuiltinSpec::provider(bootstrap_provider()),
            BuiltinSpec::model(bootstrap_model()),
            BuiltinSpec::agent(bootstrap_agent()),
        ],
    };
    manager.apply_seed(&seed).await.expect("apply_seed");
    manager.apply().await.expect("publish config snapshot");

    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        mailbox_store,
        store.clone(),
        server_name.to_string(),
        MailboxConfig::default(),
    ));
    let mut state = ServerState::new(
        runtime.clone(),
        mailbox.clone(),
        store.clone() as Arc<dyn ThreadRunStore>,
        runtime.resolver_arc(),
        ServerConfig::default(),
    );
    state.admin.admin_api_config.bearer_token = Some(ADMIN_TOKEN.into());
    state.config = Some(ConfigModuleState::new(config_store, manager));

    TestApp {
        router: build_router(&state),
        runtime,
        mailbox,
        store,
    }
}

fn file_coordinator(store: Arc<FileStore>) -> Arc<dyn CommitCoordinator> {
    awaken_stores::FileCommitCoordinator::wrap(store).expect("file coordinator constructs")
        as Arc<dyn CommitCoordinator>
}

fn postgres_coordinator(store: Arc<PostgresStore>) -> Arc<dyn CommitCoordinator> {
    Arc::new(
        awaken_stores::PgCommitCoordinator::new(store).expect("postgres coordinator constructs"),
    ) as Arc<dyn CommitCoordinator>
}

async fn make_thread_app<S, F>(
    store: Arc<S>,
    server_name: &str,
    make_coordinator: F,
) -> axum::Router
where
    S: ThreadRunStore + Send + Sync + 'static,
    F: FnOnce(Arc<S>) -> Arc<dyn CommitCoordinator>,
{
    let coordinator = make_coordinator(Arc::clone(&store));
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("mock", Arc::new(ImmediateExecutor))
            .with_model(ModelSpec::new("test-model", "mock", "mock-model"))
            .with_agent_spec(AgentSpec {
                id: "test-agent".into(),
                model_id: "test-model".into(),
                system_prompt: "test".into(),
                max_rounds: 0,
                ..Default::default()
            })
            .with_thread_run_store(store.clone() as Arc<dyn ThreadRunStore>)
            .with_commit_coordinator(coordinator)
            .build()
            .expect("build runtime"),
    );

    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(InMemoryMailboxStore::new()),
        store.clone() as Arc<dyn ThreadRunStore>,
        server_name.to_string(),
        MailboxConfig::default(),
    ));
    let state = ServerState::new(
        runtime.clone(),
        mailbox,
        store as Arc<dyn ThreadRunStore>,
        runtime.resolver_arc(),
        ServerConfig::default(),
    );
    build_router(&state)
}

#[cfg(feature = "nats")]
fn test_run_record(run_id: &str, thread_id: &str, updated_at: u64) -> RunRecord {
    RunRecord {
        run_id: run_id.to_string(),
        thread_id: thread_id.to_string(),
        agent_id: "test-agent".to_string(),
        parent_run_id: None,
        registry_manifest: None,
        activation: None,
        request: None,
        input: None,
        output: None,
        status: RunStatus::Done,
        termination_reason: None,
        final_output: None,
        error_payload: None,
        dispatch_id: None,
        session_id: None,
        transport_request_id: None,
        waiting: None,
        outcome: None,
        created_at: updated_at,
        started_at: None,
        finished_at: Some(updated_at),
        updated_at,
        steps: 0,
        input_tokens: 0,
        output_tokens: 0,
        state: None,
    }
}

#[cfg(feature = "nats")]
struct NatsFixture {
    _container: ContainerAsync<GenericImage>,
    url: String,
}

#[cfg(feature = "nats")]
impl NatsFixture {
    async fn start() -> Self {
        let image = GenericImage::new("nats", "2.10-alpine")
            .with_wait_for(WaitFor::message_on_stderr("Server is ready"))
            .with_cmd(vec!["-js"]);
        let container = image.start().await.expect("failed to start nats container");
        let host_port = container.get_host_port_ipv4(4222).await.expect("nats port");
        let url = format!("nats://127.0.0.1:{host_port}");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        Self {
            _container: container,
            url,
        }
    }
}

#[cfg(feature = "nats")]
fn unique_nats_config(fixture: &NatsFixture) -> NatsBufferedThreadConfig {
    let mut config = NatsBufferedThreadConfig::new(fixture.url.clone());
    config.stream_name = format!("THREADLOG_{}", uuid::Uuid::now_v7().simple());
    config.consumer_name = format!("c_{}", uuid::Uuid::now_v7().simple());
    config.hot_bucket = format!("hot_{}", uuid::Uuid::now_v7().simple());
    config
}

#[cfg(feature = "nats")]
fn unique_nats_mailbox_config(fixture: &NatsFixture) -> NatsMailboxConfig {
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    let mut config = NatsMailboxConfig::new(fixture.url.clone());
    config.stream_name = format!("DISPATCH_{suffix}");
    config.consumer_name = format!("c_{suffix}");
    config.dispatch_bucket = format!("d_{suffix}");
    config.epoch_bucket = format!("e_{suffix}");
    config.thread_index_bucket = format!("ti_{suffix}");
    config.sweeper_interval = std::time::Duration::from_millis(100);
    config.sweeper_republish_after = std::time::Duration::from_millis(200);
    config.watcher_initial_scan_timeout = std::time::Duration::from_secs(5);
    config.authoritative_scan_timeout = std::time::Duration::from_secs(5);
    config.nats_request_timeout = std::time::Duration::from_millis(300);
    config
}

async fn send_request(
    router: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {ADMIN_TOKEN}"));
    let request = if let Some(body) = body {
        builder = builder.header("content-type", "application/json");
        builder
            .body(Body::from(body.to_string()))
            .expect("request build")
    } else {
        builder.body(Body::empty()).expect("request build")
    };

    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("router should handle request");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    (
        status,
        String::from_utf8(bytes.to_vec()).expect("utf-8 body"),
    )
}

async fn assert_thread_hierarchy_management_round_trip<S>(
    router: &axum::Router,
    store: &Arc<S>,
    parent_id: &str,
) where
    S: ThreadRunStore + Send + Sync + 'static,
{
    store
        .save_thread(&awaken_server_contract::thread::Thread::with_id(parent_id))
        .await
        .expect("save parent thread");

    let (status, body) = send_request(
        router,
        Method::POST,
        "/v1/threads",
        Some(json!({
            "title": "Managed Child",
            "parentThreadId": parent_id,
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "unexpected create body: {body}"
    );
    let body: Value = serde_json::from_str(&body).expect("create thread json");
    let child_id = body["id"].as_str().expect("child thread id").to_string();
    assert_eq!(body["parent_thread_id"].as_str(), Some(parent_id));

    let (status, body) = send_request(
        router,
        Method::DELETE,
        &format!("/v1/threads/{parent_id}"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "unexpected delete body: {body}"
    );

    assert!(
        store
            .load_thread(parent_id)
            .await
            .expect("load parent")
            .is_none()
    );
    let child = store
        .load_thread(&child_id)
        .await
        .expect("load child")
        .expect("child should still exist");
    assert_eq!(child.parent_thread_id, None);
}

async fn assert_thread_hierarchy_rejects_missing_parent(router: &axum::Router) {
    let (status, body) = send_request(
        router,
        Method::POST,
        "/v1/threads",
        Some(json!({
            "title": "Broken Child",
            "parentThreadId": "missing-parent",
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "unexpected error body: {body}"
    );
    let body: Value = serde_json::from_str(&body).expect("error json");
    assert_eq!(
        body["error"].as_str(),
        Some("parent thread not found: missing-parent")
    );
}

fn extract_sse_events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

fn find_event<'a>(events: &'a [Value], event_type: &str) -> Option<&'a Value> {
    events.iter().find(|event| {
        event
            .get("event_type")
            .and_then(Value::as_str)
            .or_else(|| event.get("type").and_then(Value::as_str))
            == Some(event_type)
    })
}

async fn seed_managed_agent(router: &axum::Router, prefix: &str) {
    let provider_id = format!("{prefix}-provider");
    let model_id = format!("{prefix}-model");
    let agent_id = format!("{prefix}-agent");

    let (status, body) = send_request(
        router,
        Method::POST,
        "/v1/config/providers",
        Some(json!({
            "id": provider_id,
            "adapter": "stub"
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "unexpected provider body: {body}"
    );

    let (status, body) = send_request(
        router,
        Method::POST,
        "/v1/config/models",
        Some(json!({
            "id": model_id,
            "provider_id": format!("{prefix}-provider"),
            "upstream_model": format!("{prefix}-model-upstream")
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "unexpected model body: {body}");

    let (status, body) = send_request(
        router,
        Method::POST,
        "/v1/config/agents",
        Some(json!({
            "id": agent_id,
            "model_id": format!("{prefix}-model"),
            "system_prompt": "configured agent",
            "max_rounds": 1
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "unexpected agent body: {body}");
}

#[tokio::test]
async fn file_store_config_api_persists_and_publishes_runtime() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = make_app(
        Arc::new(FileStore::new(dir.path())),
        "file-config-test",
        file_coordinator,
    )
    .await;

    seed_managed_agent(&app.router, "file").await;

    let (status, body) = send_request(
        &app.router,
        Method::POST,
        "/v1/runs",
        Some(json!({
            "agentId": "file-agent",
            "threadId": "file-thread",
            "messages": [{"role": "user", "content": "hello file store"}]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected SSE body: {body}");

    let events = extract_sse_events(&body);
    let run_start = find_event(&events, "run_start").expect("run_start missing");
    let run_id = run_start["run_id"]
        .as_str()
        .expect("run_start should contain run_id");

    let agent_path = dir.path().join("config/agents/file-agent.json");
    let stored_agent = tokio::fs::read_to_string(&agent_path)
        .await
        .expect("read persisted agent config");
    let stored_agent: Value = serde_json::from_str(&stored_agent).expect("agent config json");
    let stored_agent = awaken_server_contract::ConfigRecord::<Value>::from_value(stored_agent)
        .expect("decode envelope")
        .spec;
    assert_eq!(stored_agent["id"], "file-agent");

    let resolved = app
        .runtime
        .resolver()
        .resolve("file-agent")
        .expect("file-backed runtime should resolve managed agent");
    assert_eq!(resolved.model_id(), "file-model");

    let thread = ThreadStore::load_thread(app.store.as_ref(), "file-thread")
        .await
        .expect("load persisted thread")
        .expect("thread should exist");
    assert_eq!(thread.id, "file-thread");

    let messages = ThreadStore::load_messages(app.store.as_ref(), "file-thread")
        .await
        .expect("load persisted messages")
        .expect("messages should exist");
    assert!(
        !messages.is_empty(),
        "file-backed thread should persist conversation messages"
    );

    let latest_run = RunStore::latest_run(app.store.as_ref(), "file-thread")
        .await
        .expect("load latest run")
        .expect("run should exist");
    assert_eq!(latest_run.run_id, run_id);
}

#[tokio::test]
async fn file_store_thread_lineage_filters_round_trip_via_http() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = make_app(
        Arc::new(FileStore::new(dir.path())),
        "file-lineage-test",
        file_coordinator,
    )
    .await;

    app.store
        .save_thread(
            &awaken_server_contract::thread::Thread::with_id("file-lineage-match")
                .with_resource_id("resource-a")
                .with_parent_thread_id("parent-1"),
        )
        .await
        .expect("save matching thread");
    app.store
        .save_thread(
            &awaken_server_contract::thread::Thread::with_id("file-lineage-other-resource")
                .with_resource_id("resource-b")
                .with_parent_thread_id("parent-1"),
        )
        .await
        .expect("save other thread");
    app.store
        .save_thread(
            &awaken_server_contract::thread::Thread::with_id("file-lineage-other-parent")
                .with_resource_id("resource-a")
                .with_parent_thread_id("parent-2"),
        )
        .await
        .expect("save other thread");

    let (status, body) = send_request(
        &app.router,
        Method::GET,
        "/v1/threads?resourceId=resource-a&parentThreadId=parent-1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected list body: {body}");
    let body: Value = serde_json::from_str(&body).expect("threads list json");
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].as_str(), Some("file-lineage-match"));
    assert_eq!(body["total"].as_u64(), Some(1));
    assert_eq!(body["has_more"].as_bool(), Some(false));
}

#[tokio::test]
async fn file_store_thread_hierarchy_management_round_trip_via_http() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(FileStore::new(dir.path()));
    let router = make_thread_app(store.clone(), "file-hierarchy-test", file_coordinator).await;

    assert_thread_hierarchy_management_round_trip(&router, &store, "file-parent").await;
    assert_thread_hierarchy_rejects_missing_parent(&router).await;
}

fn unique_postgres_prefix(seed: &str) -> String {
    let uuid_short = uuid::Uuid::now_v7().simple().to_string();
    format!("pgs_{}_{}", &uuid_short[..12], &seed[..seed.len().min(8)])
}

async fn make_postgres_store(seed: &str) -> (Arc<PostgresStore>, PgPool, String) {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for ignored test");
    let pool = PgPool::connect(&url).await.expect("connect postgres");
    let prefix = unique_postgres_prefix(seed);
    (
        Arc::new(PostgresStore::with_prefix(pool.clone(), &prefix)),
        pool,
        prefix,
    )
}

async fn table_exists(pool: &PgPool, table_name: &str) -> bool {
    let qualified = format!("public.{table_name}");
    let name: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
        .bind(qualified)
        .fetch_one(pool)
        .await
        .expect("query table existence");
    name.is_some()
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn postgres_store_auto_creates_schema_and_supports_end_to_end_runtime() {
    let (store, pool, prefix) = make_postgres_store("cfg_runtime").await;
    let app = make_app(store.clone(), "postgres-config-test", postgres_coordinator).await;

    seed_managed_agent(&app.router, "pg").await;

    let (status, body) = send_request(
        &app.router,
        Method::POST,
        "/v1/runs",
        Some(json!({
            "agentId": "pg-agent",
            "threadId": "pg-thread",
            "messages": [{"role": "user", "content": "hello postgres"}]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected SSE body: {body}");

    let events = extract_sse_events(&body);
    let run_finish = find_event(&events, "run_finish").expect("run_finish missing");
    assert_eq!(run_finish["thread_id"].as_str(), Some("pg-thread"));

    let resolved = app
        .runtime
        .resolver()
        .resolve("pg-agent")
        .expect("postgres-backed runtime should resolve managed agent");
    assert_eq!(resolved.model_id(), "pg-model");

    let thread = ThreadStore::load_thread(store.as_ref(), "pg-thread")
        .await
        .expect("load postgres thread")
        .expect("thread should exist");
    assert_eq!(thread.id, "pg-thread");

    let messages = ThreadStore::load_messages(store.as_ref(), "pg-thread")
        .await
        .expect("load postgres messages")
        .expect("messages should exist");
    assert!(
        !messages.is_empty(),
        "postgres-backed thread should persist conversation messages"
    );

    let latest_run = RunStore::latest_run(store.as_ref(), "pg-thread")
        .await
        .expect("load postgres run")
        .expect("run should exist");
    assert_eq!(latest_run.thread_id, "pg-thread");

    assert!(table_exists(&pool, &format!("{prefix}_configs")).await);
    assert!(table_exists(&pool, &format!("{prefix}_threads")).await);
    assert!(table_exists(&pool, &format!("{prefix}_runs")).await);
    assert!(table_exists(&pool, &format!("{prefix}_messages")).await);
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn postgres_store_thread_lineage_filters_round_trip_via_http() {
    let (store, _pool, _prefix) = make_postgres_store("cfg_lineage").await;
    let app = make_app(store.clone(), "postgres-lineage-test", postgres_coordinator).await;

    store
        .save_thread(
            &awaken_server_contract::thread::Thread::with_id("pg-lineage-match")
                .with_resource_id("resource-a")
                .with_parent_thread_id("parent-1"),
        )
        .await
        .expect("save matching thread");
    store
        .save_thread(
            &awaken_server_contract::thread::Thread::with_id("pg-lineage-other-resource")
                .with_resource_id("resource-b")
                .with_parent_thread_id("parent-1"),
        )
        .await
        .expect("save other thread");
    store
        .save_thread(
            &awaken_server_contract::thread::Thread::with_id("pg-lineage-other-parent")
                .with_resource_id("resource-a")
                .with_parent_thread_id("parent-2"),
        )
        .await
        .expect("save other thread");

    let (status, body) = send_request(
        &app.router,
        Method::GET,
        "/v1/threads?resourceId=resource-a&parentThreadId=parent-1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected list body: {body}");
    let body: Value = serde_json::from_str(&body).expect("threads list json");
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].as_str(), Some("pg-lineage-match"));
    assert_eq!(body["total"].as_u64(), Some(1));
    assert_eq!(body["has_more"].as_bool(), Some(false));
}

#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL"]
async fn postgres_store_thread_hierarchy_management_round_trip_via_http() {
    let (store, _pool, _prefix) = make_postgres_store("cfg_hierarchy").await;
    let router = make_thread_app(
        store.clone(),
        "postgres-hierarchy-test",
        postgres_coordinator,
    )
    .await;

    assert_thread_hierarchy_management_round_trip(&router, &store, "pg-parent").await;
    assert_thread_hierarchy_rejects_missing_parent(&router).await;
}

#[cfg(feature = "nats")]
#[tokio::test]
#[ignore = "requires PostgreSQL via DATABASE_URL and Docker for NATS testcontainers"]
async fn postgres_nats_two_server_instances_share_runtime_mailbox_without_duplicate_execution() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for ignored test");
    let pool = PgPool::connect(&url).await.expect("connect postgres");
    let prefix = unique_postgres_prefix("pg_nats_two_server");
    let store_a = Arc::new(PostgresStore::with_prefix(pool.clone(), &prefix));
    let store_b = Arc::new(PostgresStore::with_prefix(pool, &prefix));

    let fixture = NatsFixture::start().await;
    let mailbox_config = unique_nats_mailbox_config(&fixture);
    let mailbox_store_a = Arc::new(
        NatsMailboxStore::connect(mailbox_config.clone())
            .await
            .expect("connect nats mailbox a"),
    );
    let mailbox_store_b = Arc::new(
        NatsMailboxStore::connect(mailbox_config)
            .await
            .expect("connect nats mailbox b"),
    );

    let app_a = make_app_with_mailbox(
        store_a.clone(),
        mailbox_store_a.clone(),
        "server-a",
        postgres_coordinator,
    )
    .await;
    let app_b = make_app_with_mailbox(
        store_b.clone(),
        mailbox_store_b.clone(),
        "server-b",
        postgres_coordinator,
    )
    .await;

    let lifecycle = app_b
        .mailbox
        .start_lifecycle_ready(MailboxLifecycleConfig {
            startup_delay: std::time::Duration::ZERO,
            startup_recovery: MailboxStartupRecoveryConfig {
                max_attempts: 3,
                retry_delay: std::time::Duration::from_millis(50),
            },
            maintenance_callback: None,
        })
        .await
        .expect("server b lifecycle starts");

    let (status, body) = send_request(
        &app_a.router,
        Method::POST,
        "/v1/runs",
        Some(json!({
            "agentId": "bootstrap",
            "threadId": "pg-nats-inline-thread",
            "messages": [{"role": "user", "content": "inline from server a"}]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected inline body: {body}");
    let events = extract_sse_events(&body);
    let run_start = find_event(&events, "run_start").expect("run_start missing");
    let inline_run_id = run_start["run_id"]
        .as_str()
        .expect("run_start should contain run_id")
        .to_string();

    let (status, body) = send_request(
        &app_b.router,
        Method::GET,
        &format!("/v1/runs/{inline_run_id}"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "server b must read server a run from shared Postgres: {body}"
    );
    let body: Value = serde_json::from_str(&body).expect("run json");
    assert_eq!(body["thread_id"].as_str(), Some("pg-nats-inline-thread"));

    let background = app_a
        .mailbox
        .submit_background(
            awaken_runtime::RunActivation::new(
                "pg-nats-background-thread",
                vec![awaken_server_contract::contract::message::Message::user(
                    "background through shared nats",
                )],
            )
            .with_agent_id("bootstrap"),
        )
        .await
        .expect("background submit");

    let mut final_run = None;
    for _ in 0..50 {
        if let Some(run) = RunStore::load_run(store_b.as_ref(), &background.run_id)
            .await
            .expect("load background run")
            && run.status == RunStatus::Done
        {
            final_run = Some(run);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let final_run = final_run.expect("background run should complete once");
    assert_eq!(final_run.thread_id, "pg-nats-background-thread");

    let page = RunStore::list_runs(
        store_b.as_ref(),
        &awaken_server_contract::contract::storage::RunQuery {
            offset: 0,
            limit: 20,
            thread_id: Some("pg-nats-background-thread".to_string()),
            status: Some(RunStatus::Done),
        },
    )
    .await
    .expect("list done runs");
    assert_eq!(
        page.items.len(),
        1,
        "two active server instances must not duplicate a shared NATS dispatch"
    );
    assert_eq!(page.items[0].run_id, background.run_id);

    lifecycle.shutdown().await.expect("shutdown lifecycle");
    mailbox_store_a.shutdown().await.expect("shutdown nats a");
    mailbox_store_b.shutdown().await.expect("shutdown nats b");
}

#[cfg(feature = "nats")]
#[tokio::test]
async fn nats_buffered_store_thread_routes_round_trip_via_http() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let store = Arc::new(
        NatsBufferedThreadStore::connect(inner, unique_nats_config(&fixture))
            .await
            .expect("connect buffered nats store"),
    );
    let router = make_thread_app(store.clone(), "nats-lineage-test", |_| {
        // NATS-buffered thread store fronts an InMemoryStore inner; tests
        // rely on the runtime/mailbox coordinator path matching that buffer,
        // not the inner store directly.
        awaken_stores::MemoryCommitCoordinator::wrap(Arc::new(awaken_stores::InMemoryStore::new()))
            as Arc<dyn awaken_server_contract::contract::commit_coordinator::CommitCoordinator>
    })
    .await;

    store
        .save_thread(
            &awaken_server_contract::thread::Thread::with_id("nats-lineage-match")
                .with_title("NATS Match")
                .with_resource_id("resource-a")
                .with_parent_thread_id("parent-1"),
        )
        .await
        .expect("save matching thread");
    store
        .save_thread(
            &awaken_server_contract::thread::Thread::with_id("nats-lineage-other-resource")
                .with_title("Other Resource")
                .with_resource_id("resource-b")
                .with_parent_thread_id("parent-1"),
        )
        .await
        .expect("save other thread");
    store
        .save_thread(
            &awaken_server_contract::thread::Thread::with_id("nats-lineage-other-parent")
                .with_title("Other Parent")
                .with_resource_id("resource-a")
                .with_parent_thread_id("parent-2"),
        )
        .await
        .expect("save other thread");

    store
        .create_run(&test_run_record("nats-run-1", "nats-lineage-match", 100))
        .await
        .expect("create latest run");
    let run_metadata = MessageMetadata {
        run_id: Some("nats-run-1".to_string()),
        step_index: Some(0),
    };
    store
        .save_messages(
            "nats-lineage-match",
            &[
                Message::user("input"),
                Message::assistant("first").with_metadata(run_metadata.clone()),
                Message::internal_system("hidden").with_metadata(run_metadata.clone()),
                Message::assistant("second").with_metadata(run_metadata),
            ],
        )
        .await
        .expect("save messages");

    let (status, body) = send_request(
        &router,
        Method::GET,
        "/v1/threads?resourceId=resource-a&parentThreadId=parent-1",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "unexpected thread list body: {body}"
    );
    let body: Value = serde_json::from_str(&body).expect("threads list json");
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].as_str(), Some("nats-lineage-match"));
    assert_eq!(body["total"].as_u64(), Some(1));
    assert_eq!(body["has_more"].as_bool(), Some(false));

    let (status, body) = send_request(
        &router,
        Method::GET,
        "/v1/threads/summaries?resourceId=resource-a&parentThreadId=parent-1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected summary body: {body}");
    let body: Value = serde_json::from_str(&body).expect("threads summaries json");
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"].as_str(), Some("nats-lineage-match"));
    assert_eq!(items[0]["title"].as_str(), Some("NATS Match"));
    assert_eq!(items[0]["resource_id"].as_str(), Some("resource-a"));
    assert_eq!(items[0]["parent_thread_id"].as_str(), Some("parent-1"));
    assert_eq!(items[0]["agent_id"].as_str(), Some("test-agent"));

    let (status, body) = send_request(
        &router,
        Method::GET,
        "/v1/threads/nats-lineage-match/messages?runId=nats-run-1&after=1&order=desc",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected messages body: {body}");
    let body: Value = serde_json::from_str(&body).expect("messages json");
    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["content"][0]["text"].as_str(), Some("second"));
    assert_eq!(messages[1]["content"][0]["text"].as_str(), Some("first"));
    assert_eq!(body["total"].as_u64(), Some(2));
    assert_eq!(body["has_more"].as_bool(), Some(false));

    assert_thread_hierarchy_management_round_trip(&router, &store, "nats-parent").await;
    assert_thread_hierarchy_rejects_missing_parent(&router).await;
}
