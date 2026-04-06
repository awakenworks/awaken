use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::config_store::ConfigStore;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::storage::{RunStore, ThreadRunStore, ThreadStore};
use awaken_contract::{AgentSpec, ModelSpec, ProviderSpec};
use awaken_runtime::AgentRuntime;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::registry::traits::ModelEntry;
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

fn bootstrap_model() -> ModelSpec {
    ModelSpec {
        id: "bootstrap".into(),
        provider: "bootstrap".into(),
        model: "bootstrap-model".into(),
    }
}

fn bootstrap_agent() -> AgentSpec {
    AgentSpec {
        id: "bootstrap".into(),
        model: "bootstrap".into(),
        system_prompt: "bootstrap agent".into(),
        max_rounds: 1,
        ..Default::default()
    }
}

async fn make_app<S>(store: Arc<S>, server_name: &str) -> TestApp<S>
where
    S: ConfigStore + ThreadRunStore + Send + Sync + 'static,
{
    let bootstrap_provider = bootstrap_provider();
    let bootstrap_model = bootstrap_model();
    let bootstrap_agent = bootstrap_agent();

    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(ImmediateExecutor))
            .with_model(
                "bootstrap",
                ModelEntry {
                    provider: "bootstrap".into(),
                    model_name: "bootstrap-model".into(),
                },
            )
            .with_agent_spec(bootstrap_agent.clone())
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
    manager
        .bootstrap_if_empty(
            std::slice::from_ref(&bootstrap_provider),
            std::slice::from_ref(&bootstrap_model),
            std::slice::from_ref(&bootstrap_agent),
            &[],
        )
        .await
        .expect("bootstrap config store");
    manager.apply().await.expect("publish config snapshot");

    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(InMemoryMailboxStore::new()),
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
        router: build_router().with_state(state),
        runtime,
        store,
    }
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
            "provider": format!("{prefix}-provider"),
            "model": format!("{prefix}-model-upstream")
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
            "model": format!("{prefix}-model"),
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
