use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::config_store::ConfigStore;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
#[cfg(feature = "nats")]
use awaken_contract::contract::lifecycle::RunStatus;
#[cfg(feature = "nats")]
use awaken_contract::contract::message::{Message, MessageMetadata};
#[cfg(feature = "nats")]
use awaken_contract::contract::storage::RunRecord;
use awaken_contract::contract::storage::{RunStore, ThreadRunStore, ThreadStore};
use awaken_contract::{AgentSpec, BuiltinSeedSet, BuiltinSpec, ModelBindingSpec, ProviderSpec};
use awaken_runtime::AgentRuntime;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::registry::traits::ModelBinding;
use awaken_server::app::{AppState, ServerConfig};
use awaken_server::mailbox::{Mailbox, MailboxConfig};
use awaken_server::routes::build_router;
use awaken_server::services::config_runtime::{
    ConfigRuntimeError, ConfigRuntimeManager, ProviderExecutorFactory,
};
use awaken_stores::{FileStore, InMemoryMailboxStore, PostgresStore};
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

#[cfg(feature = "nats")]
use awaken_stores::{InMemoryStore, NatsBufferedThreadConfig, NatsBufferedThreadStore};
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
    store: Arc<S>,
}

fn bootstrap_provider() -> ProviderSpec {
    ProviderSpec {
        id: "bootstrap".into(),
        adapter: "stub".into(),
        ..Default::default()
    }
}

fn bootstrap_model() -> ModelBindingSpec {
    ModelBindingSpec {
        id: "bootstrap".into(),
        provider_id: "bootstrap".into(),
        upstream_model: "bootstrap-model".into(),
        input_token_price_per_million_usd: None,
        output_token_price_per_million_usd: None,
    }
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

async fn make_app<S>(store: Arc<S>, server_name: &str) -> TestApp<S>
where
    S: ConfigStore + ThreadRunStore + Send + Sync + 'static,
{
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(ImmediateExecutor))
            .with_thread_run_store(store.clone() as Arc<dyn ThreadRunStore>)
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
        Arc::new(InMemoryMailboxStore::new()),
        store.clone(),
        server_name.to_string(),
        MailboxConfig::default(),
    ));
    let state = AppState::new(
        runtime.clone(),
        mailbox,
        store.clone() as Arc<dyn ThreadRunStore>,
        runtime.resolver_arc(),
        ServerConfig::default(),
    )
    .with_config_store(config_store)
    .with_config_runtime_manager(manager);

    TestApp {
        router: build_router(&state).with_state(state),
        runtime,
        store,
    }
}

async fn make_thread_app<S>(store: Arc<S>, server_name: &str) -> axum::Router
where
    S: ThreadRunStore + Send + Sync + 'static,
{
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("mock", Arc::new(ImmediateExecutor))
            .with_model_binding(
                "test-model",
                ModelBinding {
                    provider_id: "mock".into(),
                    upstream_model: "mock-model".into(),
                },
            )
            .with_agent_spec(AgentSpec {
                id: "test-agent".into(),
                model_id: "test-model".into(),
                system_prompt: "test".into(),
                max_rounds: 0,
                ..Default::default()
            })
            .with_thread_run_store(store.clone() as Arc<dyn ThreadRunStore>)
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
    let state = AppState::new(
        runtime.clone(),
        mailbox,
        store as Arc<dyn ThreadRunStore>,
        runtime.resolver_arc(),
        ServerConfig::default(),
    );
    build_router(&state).with_state(state)
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

async fn send_request(
    router: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
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
        .save_thread(&awaken_contract::thread::Thread::with_id(parent_id))
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
    let app = make_app(Arc::new(FileStore::new(dir.path())), "file-config-test").await;

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
    let stored_agent = awaken_contract::ConfigRecord::<Value>::from_value(stored_agent)
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
    let app = make_app(Arc::new(FileStore::new(dir.path())), "file-lineage-test").await;

    app.store
        .save_thread(
            &awaken_contract::thread::Thread::with_id("file-lineage-match")
                .with_resource_id("resource-a")
                .with_parent_thread_id("parent-1"),
        )
        .await
        .expect("save matching thread");
    app.store
        .save_thread(
            &awaken_contract::thread::Thread::with_id("file-lineage-other-resource")
                .with_resource_id("resource-b")
                .with_parent_thread_id("parent-1"),
        )
        .await
        .expect("save other thread");
    app.store
        .save_thread(
            &awaken_contract::thread::Thread::with_id("file-lineage-other-parent")
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
    let router = make_thread_app(store.clone(), "file-hierarchy-test").await;

    assert_thread_hierarchy_management_round_trip(&router, &store, "file-parent").await;
    assert_thread_hierarchy_rejects_missing_parent(&router).await;
}

fn unique_postgres_prefix(seed: &str) -> String {
    format!(
        "{}_{}_{}",
        seed,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time before epoch")
            .as_millis()
    )
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
    let app = make_app(store.clone(), "postgres-config-test").await;

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
    let app = make_app(store.clone(), "postgres-lineage-test").await;

    store
        .save_thread(
            &awaken_contract::thread::Thread::with_id("pg-lineage-match")
                .with_resource_id("resource-a")
                .with_parent_thread_id("parent-1"),
        )
        .await
        .expect("save matching thread");
    store
        .save_thread(
            &awaken_contract::thread::Thread::with_id("pg-lineage-other-resource")
                .with_resource_id("resource-b")
                .with_parent_thread_id("parent-1"),
        )
        .await
        .expect("save other thread");
    store
        .save_thread(
            &awaken_contract::thread::Thread::with_id("pg-lineage-other-parent")
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
    let router = make_thread_app(store.clone(), "postgres-hierarchy-test").await;

    assert_thread_hierarchy_management_round_trip(&router, &store, "pg-parent").await;
    assert_thread_hierarchy_rejects_missing_parent(&router).await;
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
    let router = make_thread_app(store.clone(), "nats-lineage-test").await;

    store
        .save_thread(
            &awaken_contract::thread::Thread::with_id("nats-lineage-match")
                .with_title("NATS Match")
                .with_resource_id("resource-a")
                .with_parent_thread_id("parent-1"),
        )
        .await
        .expect("save matching thread");
    store
        .save_thread(
            &awaken_contract::thread::Thread::with_id("nats-lineage-other-resource")
                .with_title("Other Resource")
                .with_resource_id("resource-b")
                .with_parent_thread_id("parent-1"),
        )
        .await
        .expect("save other thread");
    store
        .save_thread(
            &awaken_contract::thread::Thread::with_id("nats-lineage-other-parent")
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
