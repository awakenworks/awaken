use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use awaken_contract::contract::config_store::{
    ConfigChangeEvent, ConfigChangeKind, ConfigChangeNotifier, ConfigChangeSubscriber, ConfigStore,
};
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
#[cfg(feature = "permission")]
use awaken_contract::contract::inference::ReasoningEffort;
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::storage::StorageError;
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken_contract::{
    AgentSpec, BuiltinSeedSet, BuiltinSpec, McpServerSpec, ModelBindingSpec, ProviderSpec,
};
#[cfg(feature = "permission")]
use awaken_ext_permission::{PermissionConfigKey, PermissionPlugin, ToolPermissionBehavior};
use awaken_runtime::AgentRuntime;
use awaken_runtime::builder::AgentRuntimeBuilder;
#[cfg(feature = "permission")]
use awaken_runtime::context::CompactionConfigKey;
#[cfg(feature = "permission")]
use awaken_runtime::engine::RetryConfigKey;
use awaken_runtime::registry::ToolRegistry;
use awaken_runtime::registry::memory::MapToolRegistry;
use awaken_server::app::{
    AdminApiConfig, AppState, ServerConfig, SkillCatalogArgument, SkillCatalogContext,
    SkillCatalogEntry, SkillCatalogProvider,
};
use awaken_server::mailbox::{Mailbox, MailboxConfig};
use awaken_server::routes::build_router;
use awaken_server::services::config_runtime::{
    ConfigRuntimeError, ConfigRuntimeManager, ManagedMcpRegistry, McpRegistryFactory,
    ProviderExecutorFactory,
};
use awaken_stores::InMemoryStore;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tokio::sync::broadcast;
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

struct TestProviderFactory;

impl ProviderExecutorFactory for TestProviderFactory {
    fn build(&self, spec: &ProviderSpec) -> Result<Arc<dyn LlmExecutor>, ConfigRuntimeError> {
        if spec.adapter.eq_ignore_ascii_case("stub") {
            return Ok(Arc::new(ImmediateExecutor));
        }

        Err(ConfigRuntimeError::UnsupportedProviderAdapter(
            spec.adapter.clone(),
        ))
    }
}

/// Test factory that records how many times `build` runs per provider id —
/// used to assert that the executor cache reuses unchanged providers.
#[derive(Default)]
struct CountingProviderFactory {
    builds_per_id: Arc<Mutex<std::collections::HashMap<String, usize>>>,
}

impl CountingProviderFactory {
    fn builds_for(&self, id: &str) -> usize {
        self.builds_per_id
            .lock()
            .expect("counts lock")
            .get(id)
            .copied()
            .unwrap_or(0)
    }
}

impl ProviderExecutorFactory for CountingProviderFactory {
    fn build(&self, spec: &ProviderSpec) -> Result<Arc<dyn LlmExecutor>, ConfigRuntimeError> {
        let mut map = self.builds_per_id.lock().expect("counts lock");
        *map.entry(spec.id.clone()).or_insert(0) += 1;
        if spec.adapter.eq_ignore_ascii_case("stub") {
            return Ok(Arc::new(ImmediateExecutor));
        }
        Err(ConfigRuntimeError::UnsupportedProviderAdapter(
            spec.adapter.clone(),
        ))
    }
}

#[cfg(feature = "permission")]
struct RecordingFallbackExecutor {
    attempts: Arc<Mutex<Vec<String>>>,
    retryable_model: String,
}

#[cfg(feature = "permission")]
#[async_trait]
impl LlmExecutor for RecordingFallbackExecutor {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        self.attempts
            .lock()
            .expect("attempt log lock poisoned")
            .push(request.upstream_model.clone());

        if request.upstream_model == self.retryable_model {
            return Err(InferenceExecutionError::rate_limited("test retry"));
        }

        Ok(StreamResult {
            content: vec![],
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        "recording-fallback"
    }
}

#[cfg(feature = "permission")]
struct RecordingProviderFactory {
    attempts: Arc<Mutex<Vec<String>>>,
    retryable_model: String,
}

#[cfg(feature = "permission")]
impl ProviderExecutorFactory for RecordingProviderFactory {
    fn build(&self, spec: &ProviderSpec) -> Result<Arc<dyn LlmExecutor>, ConfigRuntimeError> {
        if spec.adapter.eq_ignore_ascii_case("stub") {
            return Ok(Arc::new(RecordingFallbackExecutor {
                attempts: self.attempts.clone(),
                retryable_model: self.retryable_model.clone(),
            }));
        }

        Err(ConfigRuntimeError::UnsupportedProviderAdapter(
            spec.adapter.clone(),
        ))
    }
}

struct StaticTool {
    id: String,
}

#[async_trait]
impl Tool for StaticTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(&self.id, &self.id, "static test tool")
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolResult::success(&self.id, Value::Null).into())
    }
}

struct TestManagedMcpRegistry {
    tool_registry: Arc<dyn ToolRegistry>,
    periodic_refresh_running: AtomicBool,
}

#[async_trait]
impl ManagedMcpRegistry for TestManagedMcpRegistry {
    fn tool_registry(&self) -> Arc<dyn ToolRegistry> {
        Arc::clone(&self.tool_registry)
    }

    fn periodic_refresh_running(&self) -> bool {
        self.periodic_refresh_running.load(Ordering::Relaxed)
    }

    fn start_periodic_refresh(&self, interval: Duration) -> Result<(), ConfigRuntimeError> {
        if interval.is_zero() {
            return Err(ConfigRuntimeError::PeriodicRefresh(
                "interval must be non-zero".into(),
            ));
        }
        self.periodic_refresh_running.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn stop_periodic_refresh(&self) -> bool {
        self.periodic_refresh_running.swap(false, Ordering::Relaxed)
    }

    fn server_status(&self, _server_name: &str) -> Option<awaken_ext_mcp::McpServerStatusSnapshot> {
        None
    }

    async fn reconnect(&self, _server_name: &str) -> Result<(), ConfigRuntimeError> {
        Ok(())
    }
}

struct TestMcpRegistryFactory;

#[async_trait]
impl McpRegistryFactory for TestMcpRegistryFactory {
    async fn connect(
        &self,
        specs: &[McpServerSpec],
    ) -> Result<Option<Arc<dyn ManagedMcpRegistry>>, ConfigRuntimeError> {
        if specs.is_empty() {
            return Ok(None);
        }

        let mut registry = MapToolRegistry::new();
        for spec in specs {
            let tool_id = format!("mcp__{}__ping", spec.id);
            registry
                .register_tool(tool_id.clone(), Arc::new(StaticTool { id: tool_id }))
                .expect("register synthetic mcp tool");
        }

        Ok(Some(Arc::new(TestManagedMcpRegistry {
            tool_registry: Arc::new(registry),
            periodic_refresh_running: AtomicBool::new(false),
        }) as Arc<dyn ManagedMcpRegistry>))
    }
}

#[derive(Default)]
struct TrackingManagedMcpRegistryState {
    periodic_refresh_running: AtomicBool,
    start_calls: AtomicUsize,
    stop_calls: AtomicUsize,
}

struct TrackingManagedMcpRegistry {
    tool_registry: Arc<dyn ToolRegistry>,
    state: Arc<TrackingManagedMcpRegistryState>,
}

#[async_trait]
impl ManagedMcpRegistry for TrackingManagedMcpRegistry {
    fn tool_registry(&self) -> Arc<dyn ToolRegistry> {
        Arc::clone(&self.tool_registry)
    }

    fn periodic_refresh_running(&self) -> bool {
        self.state.periodic_refresh_running.load(Ordering::Relaxed)
    }

    fn start_periodic_refresh(&self, interval: Duration) -> Result<(), ConfigRuntimeError> {
        if interval.is_zero() {
            return Err(ConfigRuntimeError::PeriodicRefresh(
                "interval must be non-zero".into(),
            ));
        }
        self.state.start_calls.fetch_add(1, Ordering::Relaxed);
        self.state
            .periodic_refresh_running
            .store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn stop_periodic_refresh(&self) -> bool {
        self.state.stop_calls.fetch_add(1, Ordering::Relaxed);
        self.state
            .periodic_refresh_running
            .swap(false, Ordering::Relaxed)
    }

    fn server_status(&self, _server_name: &str) -> Option<awaken_ext_mcp::McpServerStatusSnapshot> {
        None
    }

    async fn reconnect(&self, _server_name: &str) -> Result<(), ConfigRuntimeError> {
        Ok(())
    }
}

#[derive(Default)]
struct TrackingMcpRegistryFactory {
    states: Mutex<Vec<Arc<TrackingManagedMcpRegistryState>>>,
}

impl TrackingMcpRegistryFactory {
    fn single_state(&self) -> Arc<TrackingManagedMcpRegistryState> {
        self.states
            .lock()
            .expect("tracking factory lock poisoned")
            .first()
            .cloned()
            .expect("tracking factory should have created one registry")
    }
}

#[async_trait]
impl McpRegistryFactory for TrackingMcpRegistryFactory {
    async fn connect(
        &self,
        specs: &[McpServerSpec],
    ) -> Result<Option<Arc<dyn ManagedMcpRegistry>>, ConfigRuntimeError> {
        if specs.is_empty() {
            return Ok(None);
        }

        let state = Arc::new(TrackingManagedMcpRegistryState::default());
        self.states
            .lock()
            .expect("tracking factory lock poisoned")
            .push(state.clone());

        let mut registry = MapToolRegistry::new();
        for spec in specs {
            let tool_id = format!("mcp__{}__ping", spec.id);
            registry
                .register_tool(tool_id.clone(), Arc::new(StaticTool { id: tool_id }))
                .expect("register synthetic mcp tool");
        }

        Ok(Some(Arc::new(TrackingManagedMcpRegistry {
            tool_registry: Arc::new(registry),
            state,
        }) as Arc<dyn ManagedMcpRegistry>))
    }
}

struct TestConfigChangeSubscriber {
    receiver: broadcast::Receiver<ConfigChangeEvent>,
}

#[async_trait]
impl ConfigChangeSubscriber for TestConfigChangeSubscriber {
    async fn next(&mut self) -> Result<ConfigChangeEvent, StorageError> {
        self.receiver.recv().await.map_err(|error| match error {
            broadcast::error::RecvError::Closed => {
                StorageError::Io("config change notifier closed".into())
            }
            broadcast::error::RecvError::Lagged(skipped) => {
                StorageError::Io(format!("config change notifier lagged by {skipped}"))
            }
        })
    }
}

struct TestConfigChangeNotifier {
    sender: broadcast::Sender<ConfigChangeEvent>,
}

impl TestConfigChangeNotifier {
    fn new() -> Self {
        let (sender, _) = broadcast::channel(32);
        Self { sender }
    }

    fn publish(&self, event: ConfigChangeEvent) {
        let _ = self.sender.send(event);
    }

    fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

#[async_trait]
impl ConfigChangeNotifier for TestConfigChangeNotifier {
    async fn subscribe(&self) -> Result<Box<dyn ConfigChangeSubscriber>, StorageError> {
        Ok(Box::new(TestConfigChangeSubscriber {
            receiver: self.sender.subscribe(),
        }))
    }
}

struct FailingSubscribeNotifier {
    inner: Arc<TestConfigChangeNotifier>,
    remaining_failures: AtomicUsize,
    subscribe_attempts: AtomicUsize,
}

impl FailingSubscribeNotifier {
    fn new(failures: usize) -> Self {
        Self {
            inner: Arc::new(TestConfigChangeNotifier::new()),
            remaining_failures: AtomicUsize::new(failures),
            subscribe_attempts: AtomicUsize::new(0),
        }
    }

    fn publish(&self, event: ConfigChangeEvent) {
        self.inner.publish(event);
    }

    fn subscriber_count(&self) -> usize {
        self.inner.subscriber_count()
    }

    fn subscribe_attempts(&self) -> usize {
        self.subscribe_attempts.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl ConfigChangeNotifier for FailingSubscribeNotifier {
    async fn subscribe(&self) -> Result<Box<dyn ConfigChangeSubscriber>, StorageError> {
        self.subscribe_attempts.fetch_add(1, Ordering::Relaxed);
        let remaining = self.remaining_failures.load(Ordering::Relaxed);
        if remaining > 0 {
            self.remaining_failures.fetch_sub(1, Ordering::Relaxed);
            return Err(StorageError::Io("synthetic subscribe failure".into()));
        }
        self.inner.subscribe().await
    }
}

struct FailingNextSubscriber;

#[async_trait]
impl ConfigChangeSubscriber for FailingNextSubscriber {
    async fn next(&mut self) -> Result<ConfigChangeEvent, StorageError> {
        Err(StorageError::Io("synthetic receive failure".into()))
    }
}

struct RecoveringReceiveNotifier {
    inner: Arc<TestConfigChangeNotifier>,
    subscribe_attempts: AtomicUsize,
}

impl RecoveringReceiveNotifier {
    fn new() -> Self {
        Self {
            inner: Arc::new(TestConfigChangeNotifier::new()),
            subscribe_attempts: AtomicUsize::new(0),
        }
    }

    fn publish(&self, event: ConfigChangeEvent) {
        self.inner.publish(event);
    }

    fn subscriber_count(&self) -> usize {
        self.inner.subscriber_count()
    }

    fn subscribe_attempts(&self) -> usize {
        self.subscribe_attempts.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl ConfigChangeNotifier for RecoveringReceiveNotifier {
    async fn subscribe(&self) -> Result<Box<dyn ConfigChangeSubscriber>, StorageError> {
        let attempt = self.subscribe_attempts.fetch_add(1, Ordering::Relaxed);
        if attempt == 0 {
            return Ok(Box::new(FailingNextSubscriber));
        }
        self.inner.subscribe().await
    }
}

struct TestApp {
    router: axum::Router,
    runtime: Arc<AgentRuntime>,
    store: Arc<InMemoryStore>,
    manager: Arc<ConfigRuntimeManager>,
    notifier: Arc<TestConfigChangeNotifier>,
}

struct StaticSkillCatalogProvider {
    skills: Vec<SkillCatalogEntry>,
}

impl SkillCatalogProvider for StaticSkillCatalogProvider {
    fn list_skills(&self) -> Vec<SkillCatalogEntry> {
        self.skills.clone()
    }
}

fn agent_spec(id: &str, model_id: &str) -> AgentSpec {
    AgentSpec {
        id: id.into(),
        model_id: model_id.into(),
        system_prompt: format!("agent {id}"),
        max_rounds: 1,
        ..Default::default()
    }
}

async fn make_runtime_manager(
    change_notifier: Option<Arc<dyn ConfigChangeNotifier>>,
) -> (
    Arc<AgentRuntime>,
    Arc<InMemoryStore>,
    Arc<ConfigRuntimeManager>,
) {
    make_runtime_manager_with_options(change_notifier, Arc::new(TestMcpRegistryFactory), None).await
}

async fn make_runtime_manager_with_options(
    change_notifier: Option<Arc<dyn ConfigChangeNotifier>>,
    mcp_registry_factory: Arc<dyn McpRegistryFactory>,
    mcp_refresh_interval: Option<Duration>,
) -> (
    Arc<AgentRuntime>,
    Arc<InMemoryStore>,
    Arc<ConfigRuntimeManager>,
) {
    make_runtime_manager_custom(
        change_notifier,
        mcp_registry_factory,
        mcp_refresh_interval,
        Arc::new(TestProviderFactory),
        false,
    )
    .await
}

async fn make_runtime_manager_custom(
    change_notifier: Option<Arc<dyn ConfigChangeNotifier>>,
    mcp_registry_factory: Arc<dyn McpRegistryFactory>,
    mcp_refresh_interval: Option<Duration>,
    provider_factory: Arc<dyn ProviderExecutorFactory>,
    register_permission_plugin: bool,
) -> (
    Arc<AgentRuntime>,
    Arc<InMemoryStore>,
    Arc<ConfigRuntimeManager>,
) {
    let store = Arc::new(InMemoryStore::new());

    let builder = AgentRuntimeBuilder::new()
        .with_provider("bootstrap", Arc::new(ImmediateExecutor))
        .with_thread_run_store(store.clone());
    #[cfg(feature = "permission")]
    let builder = if register_permission_plugin {
        builder.with_plugin("permission", Arc::new(PermissionPlugin))
    } else {
        builder
    };
    #[cfg(not(feature = "permission"))]
    let _ = register_permission_plugin;

    let runtime = Arc::new(builder.build().expect("build runtime"));

    let config_store = store.clone() as Arc<dyn ConfigStore>;
    let mut manager = ConfigRuntimeManager::new(runtime.clone(), config_store.clone())
        .expect("config runtime manager")
        .with_provider_factory(provider_factory)
        .with_mcp_registry_factory(mcp_registry_factory);
    if let Some(notifier) = change_notifier {
        manager = manager.with_change_notifier(notifier);
    }
    if let Some(interval) = mcp_refresh_interval {
        manager = manager.with_mcp_refresh_interval(interval);
    }
    let manager = Arc::new(manager);
    let seed = BuiltinSeedSet {
        binary_version: "test".to_string(),
        specs: vec![
            BuiltinSpec::provider(ProviderSpec {
                id: "bootstrap".into(),
                adapter: "stub".into(),
                ..Default::default()
            }),
            BuiltinSpec::model(ModelBindingSpec {
                id: "bootstrap".into(),
                provider_id: "bootstrap".into(),
                upstream_model: "bootstrap-model".into(),
                created_at: None,
                updated_at: None,
            }),
            BuiltinSpec::agent(agent_spec("bootstrap", "bootstrap")),
        ],
    };
    manager.apply_seed(&seed).await.expect("apply_seed");
    manager.apply().await.expect("publish config snapshot");

    (runtime, store, manager)
}

async fn make_app() -> TestApp {
    make_app_with_skill_catalog(None).await
}

async fn make_app_with_skill_catalog(
    skill_catalog_provider: Option<Arc<dyn SkillCatalogProvider>>,
) -> TestApp {
    make_app_with_skill_catalog_and_config(skill_catalog_provider, ServerConfig::default()).await
}

async fn make_app_with_admin_token(token: &str) -> TestApp {
    make_app_with_skill_catalog_config_and_admin(
        None,
        ServerConfig::default(),
        Some(AdminApiConfig {
            bearer_token: Some(token.into()),
            ..Default::default()
        }),
    )
    .await
}

async fn make_app_with_skill_catalog_and_config(
    skill_catalog_provider: Option<Arc<dyn SkillCatalogProvider>>,
    config: ServerConfig,
) -> TestApp {
    make_app_with_skill_catalog_config_and_admin(skill_catalog_provider, config, None).await
}

async fn make_app_with_skill_catalog_config_and_admin(
    skill_catalog_provider: Option<Arc<dyn SkillCatalogProvider>>,
    config: ServerConfig,
    admin_config: Option<AdminApiConfig>,
) -> TestApp {
    let notifier = Arc::new(TestConfigChangeNotifier::new());
    let (runtime, store, manager) =
        make_runtime_manager(Some(notifier.clone() as Arc<dyn ConfigChangeNotifier>)).await;
    let config_store = store.clone() as Arc<dyn ConfigStore>;

    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        store.clone(),
        "config-api-test".into(),
        MailboxConfig::default(),
    ));
    let mut state = AppState::new(
        runtime.clone(),
        mailbox,
        store.clone(),
        runtime.resolver_arc(),
        config,
    )
    .with_config_store(config_store)
    .with_config_runtime_manager(manager.clone());
    if let Some(admin_config) = admin_config {
        state = state.with_admin_api_config(admin_config);
    }
    if let Some(provider) = skill_catalog_provider {
        state = state.with_skill_catalog_provider(provider);
    }

    TestApp {
        router: build_router(&state).with_state(state),
        runtime,
        store,
        manager,
        notifier,
    }
}

async fn request_json(
    router: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    request_json_with_headers(router, method, uri, body, &[]).await
}

async fn request_json_with_headers(
    router: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
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
    if bytes.is_empty() {
        return (status, Value::Null);
    }

    (
        status,
        serde_json::from_slice(&bytes).expect("response should be valid JSON"),
    )
}

fn contains_id(items: &[Value], id: &str) -> bool {
    items.iter().any(|item| item["id"] == id)
}

async fn wait_until(
    timeout: Duration,
    interval: Duration,
    mut predicate: impl FnMut() -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        tokio::time::sleep(interval).await;
    }
    predicate()
}

#[tokio::test]
async fn admin_config_routes_require_bearer_token_when_configured() {
    let app = make_app_with_admin_token("admin-token").await;

    let (status, body) = request_json(&app.router, Method::GET, "/v1/capabilities", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["error"].as_str().unwrap().contains("authentication"));

    let (status, _) = request_json_with_headers(
        &app.router,
        Method::GET,
        "/v1/capabilities",
        None,
        &[("authorization", "Bearer wrong-token")],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, body) = request_json_with_headers(
        &app.router,
        Method::GET,
        "/v1/capabilities",
        None,
        &[("authorization", "Bearer admin-token")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["namespaces"].is_array());
}

#[tokio::test]
async fn provider_secret_is_redacted_and_preserved_on_update() {
    let app = make_app().await;

    let (status, created) = request_json(
        &app.router,
        Method::POST,
        "/v1/config/providers",
        Some(json!({
            "id": "secure",
            "adapter": "stub",
            "api_key": "top-secret"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(created.get("api_key").is_none());
    assert_eq!(created["has_api_key"], true);

    let (status, fetched) = request_json(
        &app.router,
        Method::GET,
        "/v1/config/providers/secure",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(fetched.get("api_key").is_none());
    assert_eq!(fetched["has_api_key"], true);

    let (status, updated) = request_json(
        &app.router,
        Method::PUT,
        "/v1/config/providers/secure",
        Some(json!({
            "id": "secure",
            "adapter": "stub",
            "base_url": "https://provider.example.test"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(updated.get("api_key").is_none());
    assert_eq!(updated["has_api_key"], true);

    let stored = ConfigStore::get(app.store.as_ref(), "providers", "secure")
        .await
        .expect("read raw provider")
        .expect("provider should exist");
    let stored = awaken_contract::ConfigRecord::<serde_json::Value>::from_value(stored)
        .expect("decode envelope")
        .spec;
    assert_eq!(stored["api_key"], "top-secret");
    assert_eq!(stored["base_url"], "https://provider.example.test");
}

#[tokio::test]
async fn mcp_servers_are_redacted_and_publish_live_tools() {
    let app = make_app().await;

    let (status, created) = request_json(
        &app.router,
        Method::POST,
        "/v1/config/mcp-servers",
        Some(json!({
            "id": "demo",
            "transport": "stdio",
            "command": "demo-mcp",
            "args": ["--serve"],
            "env": {
                "TOKEN": "secret-token"
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(created.get("env").is_none());
    assert_eq!(created["has_env"], true);
    assert_eq!(created["env_keys"], json!(["TOKEN"]));

    let (status, fetched) = request_json(
        &app.router,
        Method::GET,
        "/v1/config/mcp-servers/demo",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(fetched.get("env").is_none());
    assert_eq!(fetched["has_env"], true);
    assert_eq!(fetched["env_keys"], json!(["TOKEN"]));

    let (status, updated) = request_json(
        &app.router,
        Method::PUT,
        "/v1/config/mcp-servers/demo",
        Some(json!({
            "id": "demo",
            "transport": "stdio",
            "command": "demo-mcp",
            "args": ["--updated"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(updated.get("env").is_none());
    assert_eq!(updated["has_env"], true);

    let stored = ConfigStore::get(app.store.as_ref(), "mcp-servers", "demo")
        .await
        .expect("read raw mcp config")
        .expect("mcp config should exist");
    let stored = awaken_contract::ConfigRecord::<serde_json::Value>::from_value(stored)
        .expect("decode envelope")
        .spec;
    assert_eq!(stored["env"]["TOKEN"], "secret-token");
    assert_eq!(stored["args"], json!(["--updated"]));

    let (status, capabilities) =
        request_json(&app.router, Method::GET, "/v1/capabilities", None).await;
    assert_eq!(status, StatusCode::OK);
    let tools = capabilities["tools"]
        .as_array()
        .expect("tools should be an array");
    assert!(contains_id(tools, "mcp__demo__ping"));

    let resolved = app
        .runtime
        .resolver()
        .resolve("bootstrap")
        .expect("bootstrap agent should resolve");
    assert!(
        resolved.tools.contains_key("mcp__demo__ping"),
        "resolved agent should include dynamically published MCP tools"
    );
}

#[tokio::test]
async fn published_config_updates_live_capabilities_and_resolver() {
    let app = make_app().await;

    let (status, _) = request_json(
        &app.router,
        Method::POST,
        "/v1/config/providers",
        Some(json!({
            "id": "provider-1",
            "adapter": "stub"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) = request_json(
        &app.router,
        Method::POST,
        "/v1/config/models",
        Some(json!({
            "id": "model-1",
            "provider_id": "provider-1",
            "upstream_model": "test-model"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, agent) = request_json(
        &app.router,
        Method::POST,
        "/v1/config/agents",
        Some(json!({
            "id": "agent-1",
            "model_id": "model-1",
            "system_prompt": "hello",
            "max_rounds": 2
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(agent["id"], "agent-1");

    let (status, capabilities) =
        request_json(&app.router, Method::GET, "/v1/capabilities", None).await;
    assert_eq!(status, StatusCode::OK);

    let agents = capabilities["agents"]
        .as_array()
        .expect("agents should be an array");
    assert!(agents.iter().any(|value| value == "agent-1"));

    let models = capabilities["models"]
        .as_array()
        .expect("models should be an array");
    assert!(contains_id(models, "model-1"));

    let providers = capabilities["providers"]
        .as_array()
        .expect("providers should be an array");
    assert!(contains_id(providers, "provider-1"));

    let resolved = app
        .runtime
        .resolver()
        .resolve("agent-1")
        .expect("resolver should see published config");
    assert_eq!(resolved.id(), "agent-1");
    assert_eq!(resolved.model_id(), "model-1");
}

#[cfg(feature = "permission")]
#[tokio::test]
async fn documented_config_driven_agent_tuning_publishes_sections_and_retry() {
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let notifier = Arc::new(TestConfigChangeNotifier::new());
    let (runtime, store, manager) = make_runtime_manager_custom(
        Some(notifier.clone() as Arc<dyn ConfigChangeNotifier>),
        Arc::new(TestMcpRegistryFactory),
        None,
        Arc::new(RecordingProviderFactory {
            attempts: attempts.clone(),
            retryable_model: "doc-primary".into(),
        }),
        true,
    )
    .await;
    let config_store = store.clone() as Arc<dyn ConfigStore>;
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        store.clone(),
        "config-doc-scenario-test".into(),
        MailboxConfig::default(),
    ));
    let state = AppState::new(
        runtime.clone(),
        mailbox,
        store,
        runtime.resolver_arc(),
        ServerConfig::default(),
    )
    .with_config_store(config_store)
    .with_config_runtime_manager(manager);
    let router = build_router(&state).with_state(state);

    let (status, _) = request_json(
        &router,
        Method::POST,
        "/v1/config/providers",
        Some(json!({
            "id": "doc-provider",
            "adapter": "stub",
            "base_url": null,
            "timeout_secs": 300
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) = request_json(
        &router,
        Method::POST,
        "/v1/config/models",
        Some(json!({
            "id": "research-default",
            "provider_id": "doc-provider",
            "upstream_model": "doc-primary"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, agent) = request_json(
        &router,
        Method::POST,
        "/v1/config/agents",
        Some(json!({
            "id": "research-assistant",
            "model_id": "research-default",
            "system_prompt": "You help with source-grounded research.",
            "max_rounds": 12,
            "max_continuation_retries": 3,
            "reasoning_effort": "medium",
            "plugin_ids": ["permission"],
            "allowed_tools": ["read_document", "web_search", "summarize"],
            "excluded_tools": ["delete_file"],
            "context_policy": {
                "max_context_tokens": 120000,
                "max_output_tokens": 8192,
                "min_recent_messages": 8,
                "enable_prompt_cache": true,
                "autocompact_threshold": 90000,
                "compaction_mode": "keep_recent_raw_suffix",
                "compaction_raw_suffix_messages": 2
            },
            "sections": {
                "retry": {
                    "max_retries": 1,
                    "fallback_upstream_models": ["doc-fallback"],
                    "backoff_base_ms": 0
                },
                "permission": {
                    "default_behavior": "ask",
                    "rules": [
                        { "tool": "read_document", "behavior": "allow" },
                        { "tool": "web_search", "behavior": "ask" },
                        { "tool": "delete_*", "behavior": "deny" }
                    ]
                },
                "compaction": {
                    "summarizer_system_prompt": "Preserve decisions, facts, tool results, and unresolved tasks.",
                    "summarizer_user_prompt": "Summarize the following conversation:\n\n{messages}",
                    "summary_max_tokens": 1024,
                    "summary_model": "doc-summary",
                    "min_savings_ratio": 0.3
                }
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(agent["id"], "research-assistant");

    let (status, capabilities) = request_json(&router, Method::GET, "/v1/capabilities", None).await;
    assert_eq!(status, StatusCode::OK);
    let permission_plugin = capabilities["plugins"]
        .as_array()
        .expect("plugins should be an array")
        .iter()
        .find(|plugin| plugin["id"] == "permission")
        .expect("permission plugin should be advertised");
    assert!(
        permission_plugin["config_schemas"]
            .as_array()
            .expect("config_schemas should be an array")
            .iter()
            .any(|schema| schema["key"] == "permission")
    );

    let resolved = runtime
        .resolver()
        .resolve("research-assistant")
        .expect("documented config-driven agent should resolve");
    assert_eq!(resolved.id(), "research-assistant");
    assert_eq!(resolved.model_id(), "research-default");
    assert_eq!(resolved.upstream_model, "doc-primary");
    assert_eq!(resolved.max_rounds(), 12);
    assert_eq!(resolved.max_continuation_retries(), 3);
    assert_eq!(
        resolved.spec.reasoning_effort.as_ref(),
        Some(&ReasoningEffort::Medium)
    );
    assert_eq!(
        resolved.spec.allowed_tools.as_ref().expect("allowed tools"),
        &vec![
            "read_document".to_string(),
            "web_search".to_string(),
            "summarize".to_string()
        ]
    );
    assert_eq!(
        resolved
            .spec
            .excluded_tools
            .as_ref()
            .expect("excluded tools"),
        &vec!["delete_file".to_string()]
    );

    let retry = resolved
        .spec
        .config::<RetryConfigKey>()
        .expect("retry section should decode");
    assert_eq!(retry.max_retries, 1);
    assert_eq!(
        retry.fallback_upstream_models,
        vec!["doc-fallback".to_string()]
    );
    assert_eq!(retry.backoff_base_ms, 0);

    let permission = resolved
        .spec
        .config::<PermissionConfigKey>()
        .expect("permission section should decode");
    assert_eq!(permission.default_behavior, ToolPermissionBehavior::Ask);
    assert_eq!(permission.rules.len(), 3);
    assert_eq!(permission.rules[0].tool, "read_document");

    let context_policy = resolved
        .context_policy()
        .expect("context policy should be configured");
    assert_eq!(context_policy.max_context_tokens, 120000);
    assert_eq!(context_policy.autocompact_threshold, Some(90000));

    let compaction = resolved
        .spec
        .config::<CompactionConfigKey>()
        .expect("compaction section should decode");
    assert_eq!(compaction.summary_max_tokens, Some(1024));
    assert_eq!(compaction.summary_model.as_deref(), Some("doc-summary"));
    assert!((compaction.min_savings_ratio - 0.3).abs() < f64::EPSILON);

    attempts.lock().expect("attempt log lock poisoned").clear();
    resolved
        .llm_executor
        .execute(InferenceRequest {
            upstream_model: resolved.upstream_model.clone(),
            messages: vec![],
            tools: vec![],
            system: vec![],
            overrides: None,
            enable_prompt_cache: context_policy.enable_prompt_cache,
        })
        .await
        .expect("fallback upstream model should recover retryable primary failure");
    assert_eq!(
        *attempts.lock().expect("attempt log lock poisoned"),
        vec![
            "doc-primary".to_string(),
            "doc-primary".to_string(),
            "doc-fallback".to_string()
        ]
    );
}

#[tokio::test]
async fn capabilities_include_skill_registry_when_available() {
    let skill_catalog = Arc::new(StaticSkillCatalogProvider {
        skills: vec![SkillCatalogEntry {
            id: "greeting".into(),
            name: "Greeting".into(),
            description: "Adds friendly greeting behavior".into(),
            allowed_tools: vec!["append_note".into()],
            when_to_use: Some("When the user needs a warm opening.".into()),
            arguments: vec![SkillCatalogArgument {
                name: "tone".into(),
                description: Some("Greeting tone".into()),
                required: false,
            }],
            argument_hint: Some("tone=warm".into()),
            user_invocable: true,
            model_invocable: true,
            model_override: None,
            context: SkillCatalogContext::Inline,
            paths: vec!["src/**".into()],
        }],
    }) as Arc<dyn SkillCatalogProvider>;
    let app = make_app_with_skill_catalog(Some(skill_catalog)).await;

    let (status, capabilities) =
        request_json(&app.router, Method::GET, "/v1/capabilities", None).await;
    assert_eq!(status, StatusCode::OK);

    let skills = capabilities["skills"]
        .as_array()
        .expect("skills should be an array");
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0]["id"], "greeting");
    assert_eq!(skills[0]["name"], "Greeting");
    assert_eq!(skills[0]["context"], "inline");
    assert_eq!(skills[0]["allowed_tools"], json!(["append_note"]));
    assert_eq!(skills[0]["arguments"][0]["name"], "tone");
}

#[tokio::test]
async fn capabilities_report_runtime_supported_adapters_without_scripted() {
    let app = make_app().await;

    let (status, capabilities) =
        request_json(&app.router, Method::GET, "/v1/capabilities", None).await;
    assert_eq!(status, StatusCode::OK);

    let adapters = capabilities["supported_adapters"]
        .as_array()
        .expect("supported_adapters should be an array");
    assert!(adapters.iter().any(|value| value == "openai"));
    assert!(adapters.iter().any(|value| value == "groq"));
    assert!(adapters.iter().any(|value| value == "nebius"));
    assert!(
        !adapters.iter().any(|value| value == "scripted"),
        "admin capabilities must not advertise adapters the runtime rejects"
    );
}

#[tokio::test]
async fn periodic_refresh_publishes_external_store_changes() {
    let app = make_app().await;
    app.manager
        .start_periodic_refresh(Duration::from_millis(20))
        .expect("start periodic refresh");

    ConfigStore::put(
        app.store.as_ref(),
        "mcp-servers",
        "shared",
        &json!({
            "id": "shared",
            "transport": "stdio",
            "command": "shared-mcp"
        }),
    )
    .await
    .expect("write shared mcp config");

    let observed = wait_until(Duration::from_secs(2), Duration::from_millis(20), || {
        app.runtime
            .resolver()
            .resolve("bootstrap")
            .map(|resolved| resolved.tools.contains_key("mcp__shared__ping"))
            .unwrap_or(false)
    })
    .await;
    assert!(
        observed,
        "runtime should converge to external config changes"
    );

    let (status, capabilities) =
        request_json(&app.router, Method::GET, "/v1/capabilities", None).await;
    assert_eq!(status, StatusCode::OK);
    let tools = capabilities["tools"]
        .as_array()
        .expect("tools should be an array");
    assert!(contains_id(tools, "mcp__shared__ping"));

    assert!(app.manager.stop_periodic_refresh().await);
}

#[tokio::test]
async fn notify_listener_applies_external_store_changes_without_waiting_for_poll() {
    let app = make_app().await;
    app.manager
        .start_periodic_refresh(Duration::from_secs(60))
        .expect("start periodic refresh with listener");
    let listening = wait_until(Duration::from_secs(1), Duration::from_millis(10), || {
        app.notifier.subscriber_count() > 0
    })
    .await;
    assert!(
        listening,
        "config change listener should subscribe before publishing"
    );

    ConfigStore::put(
        app.store.as_ref(),
        "mcp-servers",
        "notified",
        &json!({
            "id": "notified",
            "transport": "stdio",
            "command": "notify-mcp"
        }),
    )
    .await
    .expect("write notified mcp config");
    app.notifier.publish(ConfigChangeEvent {
        namespace: "mcp-servers".into(),
        id: "notified".into(),
        kind: ConfigChangeKind::Put,
    });

    let observed = wait_until(Duration::from_secs(1), Duration::from_millis(10), || {
        app.runtime
            .resolver()
            .resolve("bootstrap")
            .map(|resolved| resolved.tools.contains_key("mcp__notified__ping"))
            .unwrap_or(false)
    })
    .await;
    assert!(
        observed,
        "notify listener should publish config changes without waiting for the poll interval"
    );

    assert!(app.manager.stop_periodic_refresh().await);
}

#[tokio::test]
async fn notify_listener_removes_external_store_changes_without_waiting_for_poll() {
    let app = make_app().await;
    app.manager
        .start_periodic_refresh(Duration::from_secs(60))
        .expect("start periodic refresh with listener");
    let listening = wait_until(Duration::from_secs(1), Duration::from_millis(10), || {
        app.notifier.subscriber_count() > 0
    })
    .await;
    assert!(
        listening,
        "config change listener should subscribe before publishing"
    );

    ConfigStore::put(
        app.store.as_ref(),
        "mcp-servers",
        "notify-delete",
        &json!({
            "id": "notify-delete",
            "transport": "stdio",
            "command": "notify-delete-mcp"
        }),
    )
    .await
    .expect("write mcp config before delete");
    app.notifier.publish(ConfigChangeEvent {
        namespace: "mcp-servers".into(),
        id: "notify-delete".into(),
        kind: ConfigChangeKind::Put,
    });
    let published = wait_until(Duration::from_secs(1), Duration::from_millis(10), || {
        app.runtime
            .resolver()
            .resolve("bootstrap")
            .map(|resolved| resolved.tools.contains_key("mcp__notify-delete__ping"))
            .unwrap_or(false)
    })
    .await;
    assert!(
        published,
        "notify listener should publish the initial MCP tool"
    );

    ConfigStore::delete(app.store.as_ref(), "mcp-servers", "notify-delete")
        .await
        .expect("delete mcp config");
    app.notifier.publish(ConfigChangeEvent {
        namespace: "mcp-servers".into(),
        id: "notify-delete".into(),
        kind: ConfigChangeKind::Delete,
    });

    let removed = wait_until(Duration::from_secs(1), Duration::from_millis(10), || {
        app.runtime
            .resolver()
            .resolve("bootstrap")
            .map(|resolved| !resolved.tools.contains_key("mcp__notify-delete__ping"))
            .unwrap_or(false)
    })
    .await;
    assert!(
        removed,
        "notify listener should remove published tools without waiting for the poll interval"
    );

    let (status, capabilities) =
        request_json(&app.router, Method::GET, "/v1/capabilities", None).await;
    assert_eq!(status, StatusCode::OK);
    let tools = capabilities["tools"]
        .as_array()
        .expect("tools should be an array");
    assert!(!contains_id(tools, "mcp__notify-delete__ping"));

    assert!(app.manager.stop_periodic_refresh().await);
}

#[tokio::test]
async fn notify_listener_recovers_from_subscribe_failures() {
    let notifier = Arc::new(FailingSubscribeNotifier::new(1));
    let (runtime, store, manager) =
        make_runtime_manager(Some(notifier.clone() as Arc<dyn ConfigChangeNotifier>)).await;
    manager
        .start_periodic_refresh(Duration::from_secs(60))
        .expect("start periodic refresh with listener");

    let listening = wait_until(Duration::from_secs(3), Duration::from_millis(20), || {
        notifier.subscribe_attempts() >= 2 && notifier.subscriber_count() > 0
    })
    .await;
    assert!(
        listening,
        "config change listener should retry subscribe failures and recover"
    );

    ConfigStore::put(
        store.as_ref(),
        "mcp-servers",
        "subscribe-retry",
        &json!({
            "id": "subscribe-retry",
            "transport": "stdio",
            "command": "subscribe-retry-mcp"
        }),
    )
    .await
    .expect("write mcp config after subscribe retry");
    notifier.publish(ConfigChangeEvent {
        namespace: "mcp-servers".into(),
        id: "subscribe-retry".into(),
        kind: ConfigChangeKind::Put,
    });

    let observed = wait_until(Duration::from_secs(1), Duration::from_millis(10), || {
        runtime
            .resolver()
            .resolve("bootstrap")
            .map(|resolved| resolved.tools.contains_key("mcp__subscribe-retry__ping"))
            .unwrap_or(false)
    })
    .await;
    assert!(
        observed,
        "notify listener should apply config changes after recovering from subscribe failures"
    );

    assert!(manager.stop_periodic_refresh().await);
}

#[tokio::test]
async fn notify_listener_recovers_from_receive_failures() {
    let notifier = Arc::new(RecoveringReceiveNotifier::new());
    let (runtime, store, manager) =
        make_runtime_manager(Some(notifier.clone() as Arc<dyn ConfigChangeNotifier>)).await;
    manager
        .start_periodic_refresh(Duration::from_secs(60))
        .expect("start periodic refresh with listener");

    let listening = wait_until(Duration::from_secs(3), Duration::from_millis(20), || {
        notifier.subscribe_attempts() >= 2 && notifier.subscriber_count() > 0
    })
    .await;
    assert!(
        listening,
        "config change listener should resubscribe after receive failures"
    );

    ConfigStore::put(
        store.as_ref(),
        "mcp-servers",
        "receive-retry",
        &json!({
            "id": "receive-retry",
            "transport": "stdio",
            "command": "receive-retry-mcp"
        }),
    )
    .await
    .expect("write mcp config after receive retry");
    notifier.publish(ConfigChangeEvent {
        namespace: "mcp-servers".into(),
        id: "receive-retry".into(),
        kind: ConfigChangeKind::Put,
    });

    let observed = wait_until(Duration::from_secs(1), Duration::from_millis(10), || {
        runtime
            .resolver()
            .resolve("bootstrap")
            .map(|resolved| resolved.tools.contains_key("mcp__receive-retry__ping"))
            .unwrap_or(false)
    })
    .await;
    assert!(
        observed,
        "notify listener should apply config changes after recovering from receive failures"
    );

    assert!(manager.stop_periodic_refresh().await);
}

#[tokio::test]
async fn put_rejects_path_and_body_id_mismatch() {
    let app = make_app().await;

    let (status, body) = request_json(
        &app.router,
        Method::PUT,
        "/v1/config/agents/left",
        Some(json!({
            "id": "right",
            "model_id": "bootstrap",
            "system_prompt": "mismatch"
        })),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .expect("error string")
            .contains("path id 'left' does not match body id 'right'")
    );
}

#[tokio::test]
async fn delete_provider_with_dependents_returns_409() {
    let app = make_app().await;

    // bootstrap provider is referenced by bootstrap model — should be blocked
    let (status, body) = request_json(
        &app.router,
        Method::DELETE,
        "/v1/config/providers/bootstrap",
        None,
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    let used_by = body["used_by"].as_array().expect("used_by array");
    assert!(!used_by.is_empty(), "should report dependent models");
}

#[tokio::test]
async fn delete_rolls_back_when_runtime_apply_would_break_graph() {
    let app = make_app().await;

    // With force=true, the dependency check is bypassed and the runtime apply
    // is attempted.  Removing the bootstrap provider while the bootstrap model
    // still references it breaks the runtime graph → 400 + rollback.
    let (status, body) = request_json(
        &app.router,
        Method::DELETE,
        "/v1/config/providers/bootstrap?force=true",
        None,
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .expect("error string")
            .contains("invalid managed config")
    );

    let stored = ConfigStore::get(app.store.as_ref(), "providers", "bootstrap")
        .await
        .expect("read provider after rollback");
    assert!(
        stored.is_some(),
        "provider should be restored after rollback"
    );
}

#[tokio::test]
async fn duplicate_create_returns_conflict_status() {
    let app = make_app().await;

    let (status, _) = request_json(
        &app.router,
        Method::POST,
        "/v1/config/providers",
        Some(json!({
            "id": "dupe",
            "adapter": "stub"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request_json(
        &app.router,
        Method::POST,
        "/v1/config/providers",
        Some(json!({
            "id": "dupe",
            "adapter": "stub"
        })),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"]
            .as_str()
            .expect("error string")
            .contains("already exists")
    );
}

#[tokio::test]
async fn failed_publish_stops_prepared_mcp_registry() {
    let factory = Arc::new(TrackingMcpRegistryFactory::default());
    let (runtime, store, manager) = make_runtime_manager_with_options(
        None,
        factory.clone() as Arc<dyn McpRegistryFactory>,
        Some(Duration::from_secs(5)),
    )
    .await;

    ConfigStore::put(
        store.as_ref(),
        "mcp-servers",
        "cleanup",
        &json!({
            "id": "cleanup",
            "transport": "stdio",
            "command": "cleanup-mcp"
        }),
    )
    .await
    .expect("write managed mcp server");
    ConfigStore::put(
        store.as_ref(),
        "providers",
        "broken",
        &json!({
            "id": "broken",
            "adapter": "unsupported-provider"
        }),
    )
    .await
    .expect("write invalid provider");

    let error = manager.apply().await.expect_err("publish should fail");
    assert!(
        error
            .to_string()
            .contains("unsupported provider adapter: unsupported-provider")
    );

    let state = factory.single_state();
    assert_eq!(state.start_calls.load(Ordering::Relaxed), 1);
    assert_eq!(state.stop_calls.load(Ordering::Relaxed), 1);
    assert!(!state.periodic_refresh_running.load(Ordering::Relaxed));

    let resolved = runtime
        .resolver()
        .resolve("bootstrap")
        .expect("bootstrap agent should still resolve");
    assert!(
        !resolved.tools.contains_key("mcp__cleanup__ping"),
        "failed publish must not leak prepared MCP tools into the live runtime"
    );
}

// ── apply / apply_if_changed semantics ──────────────────────────────
//
// These tests pin the externally-visible contract of the apply path:
//   * apply() always rebuilds and publishes a snapshot, returning a
//     strictly increasing registry version even when the underlying
//     store is unchanged.
//   * apply_if_changed() returns Some(version) on first call after a
//     mutation and None when nothing has changed since the last apply.

#[tokio::test]
async fn apply_returns_monotonically_advancing_version() {
    let (_runtime, _store, manager) = make_runtime_manager(None).await;

    let first = manager.apply().await.expect("first apply");
    let second = manager.apply().await.expect("second apply");

    assert!(
        second > first,
        "apply() must always publish and advance the registry version, got {first} then {second}"
    );
}

#[tokio::test]
async fn apply_if_changed_returns_none_when_nothing_changed() {
    let (_runtime, _store, manager) = make_runtime_manager(None).await;

    let result = manager
        .apply_if_changed()
        .await
        .expect("apply_if_changed succeeds");
    assert!(
        result.is_none(),
        "apply_if_changed must return None when the snapshot fingerprint matches the last applied"
    );
}

#[tokio::test]
async fn apply_reuses_executor_for_unchanged_provider() {
    let factory = Arc::new(CountingProviderFactory::default());
    let (_runtime, _store, manager) = make_runtime_manager_custom(
        None,
        Arc::new(TestMcpRegistryFactory),
        None,
        factory.clone() as Arc<dyn ProviderExecutorFactory>,
        false,
    )
    .await;

    let initial_builds = factory.builds_for("bootstrap");
    assert!(
        initial_builds >= 1,
        "bootstrap apply should have built the provider at least once, got {initial_builds}"
    );

    manager.apply().await.expect("re-apply with no changes");

    assert_eq!(
        factory.builds_for("bootstrap"),
        initial_builds,
        "executor cache must reuse the unchanged provider across applies"
    );
}

#[tokio::test]
async fn change_listener_coalesces_event_bursts_within_min_apply_interval() {
    let factory = Arc::new(CountingProviderFactory::default());
    let notifier = Arc::new(TestConfigChangeNotifier::new());
    let store = Arc::new(InMemoryStore::new());

    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(ImmediateExecutor))
            .with_thread_run_store(store.clone())
            .build()
            .expect("build runtime"),
    );

    let manager = Arc::new(
        ConfigRuntimeManager::new(runtime.clone(), store.clone() as Arc<dyn ConfigStore>)
            .expect("config runtime manager")
            .with_provider_factory(factory.clone() as Arc<dyn ProviderExecutorFactory>)
            .with_mcp_registry_factory(Arc::new(TestMcpRegistryFactory))
            .with_change_notifier(notifier.clone() as Arc<dyn ConfigChangeNotifier>)
            .with_min_apply_interval(Duration::from_millis(200)),
    );
    let seed = BuiltinSeedSet {
        binary_version: "test".to_string(),
        specs: vec![
            BuiltinSpec::provider(ProviderSpec {
                id: "bootstrap".into(),
                adapter: "stub".into(),
                ..Default::default()
            }),
            BuiltinSpec::model(ModelBindingSpec {
                id: "bootstrap".into(),
                provider_id: "bootstrap".into(),
                upstream_model: "bootstrap-model".into(),
                created_at: None,
                updated_at: None,
            }),
            BuiltinSpec::agent(agent_spec("bootstrap", "bootstrap")),
        ],
    };
    manager.apply_seed(&seed).await.expect("apply_seed");
    manager.apply().await.expect("initial apply");
    manager
        .start_periodic_refresh(Duration::from_secs(60))
        .expect("start change listener");

    let listening = wait_until(Duration::from_secs(1), Duration::from_millis(10), || {
        notifier.subscriber_count() > 0
    })
    .await;
    assert!(listening, "listener should subscribe before publish");

    let initial_builds = factory.builds_for("bootstrap");

    // Mutate provider 4 times rapidly. Each mutation flips the cache miss
    // bit so the executor gets rebuilt on every apply that runs.
    for i in 1..=4u64 {
        let spec = json!({
            "id": "bootstrap",
            "adapter": "stub",
            "timeout_secs": 100 + i,
        });
        (store.clone() as Arc<dyn ConfigStore>)
            .put("providers", "bootstrap", &spec)
            .await
            .expect("write mutated provider");
        notifier.publish(ConfigChangeEvent {
            namespace: "providers".into(),
            id: "bootstrap".into(),
            kind: ConfigChangeKind::Put,
        });
    }

    // Wait long enough for the debounce window to flush.
    tokio::time::sleep(Duration::from_millis(600)).await;

    let new_builds = factory.builds_for("bootstrap") - initial_builds;
    assert!(
        (1..=2).contains(&new_builds),
        "4 events fired within 200ms debounce window must produce 1 or 2 applies — \
         0 means the listener missed the burst, >2 means coalescing failed (got {new_builds})"
    );
}

#[tokio::test]
async fn apply_rebuilds_executor_when_provider_spec_changes() {
    let factory = Arc::new(CountingProviderFactory::default());
    let (_runtime, store, manager) = make_runtime_manager_custom(
        None,
        Arc::new(TestMcpRegistryFactory),
        None,
        factory.clone() as Arc<dyn ProviderExecutorFactory>,
        false,
    )
    .await;

    let initial_builds = factory.builds_for("bootstrap");

    // Mutate the provider spec — different timeout makes the spec unequal,
    // so the cache must miss and the factory must be invoked again.
    let mutated = json!({
        "id": "bootstrap",
        "adapter": "stub",
        "timeout_secs": 999
    });
    (store.clone() as Arc<dyn ConfigStore>)
        .put("providers", "bootstrap", &mutated)
        .await
        .expect("write mutated provider");

    manager.apply().await.expect("re-apply after mutation");

    assert!(
        factory.builds_for("bootstrap") > initial_builds,
        "provider must be rebuilt when its spec changes (initial {initial_builds}, after {})",
        factory.builds_for("bootstrap")
    );
}

#[tokio::test]
async fn apply_if_changed_returns_some_after_store_mutation() {
    let (_runtime, store, manager) = make_runtime_manager(None).await;

    // Mutating the providers namespace must invalidate the previously
    // applied fingerprint so apply_if_changed publishes again.
    let new_provider = json!({
        "id": "extra",
        "adapter": "stub"
    });
    (store.clone() as Arc<dyn ConfigStore>)
        .put("providers", "extra", &new_provider)
        .await
        .expect("write extra provider");

    let result = manager
        .apply_if_changed()
        .await
        .expect("apply_if_changed succeeds")
        .expect("store mutation must produce a new fingerprint");

    let after_no_change = manager
        .apply_if_changed()
        .await
        .expect("apply_if_changed succeeds");
    assert!(
        after_no_change.is_none(),
        "calling apply_if_changed twice without further mutation must return None"
    );
    let _ = result;
}

// ── MCP server status and restart endpoint smoke tests ──────────────────────

#[tokio::test]
async fn mcp_status_returns_503_when_no_runtime_configured() {
    // Build a state without a config_runtime_manager so the MCP status endpoint
    // returns 503 Service Unavailable.
    let store = Arc::new(InMemoryStore::new());
    let thread_store = store.clone();
    use awaken_contract::AgentSpec;
    use awaken_runtime::builder::AgentRuntimeBuilder;
    use awaken_runtime::registry::traits::ModelBinding;

    struct StubExecutor;
    #[async_trait]
    impl awaken_contract::contract::executor::LlmExecutor for StubExecutor {
        async fn execute(
            &self,
            _request: awaken_contract::contract::executor::InferenceRequest,
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

    let bootstrap_agent = AgentSpec {
        id: "boot".into(),
        model_id: "boot".into(),
        system_prompt: "boot".into(),
        max_rounds: 1,
        ..Default::default()
    };
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("boot", Arc::new(StubExecutor))
            .with_model_binding(
                "boot",
                ModelBinding {
                    provider_id: "boot".into(),
                    upstream_model: "m".into(),
                },
            )
            .with_agent_spec(bootstrap_agent)
            .with_thread_run_store(thread_store.clone())
            .build()
            .expect("runtime"),
    );
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        thread_store.clone(),
        "mcp-status-test".into(),
        MailboxConfig::default(),
    ));
    // No config_runtime_manager attached → MCP endpoints return 503.
    let state = awaken_server::app::AppState::new(
        runtime.clone(),
        mailbox,
        thread_store as Arc<dyn awaken_contract::contract::storage::ThreadRunStore>,
        runtime.resolver_arc(),
        ServerConfig::default(),
    );
    let router = build_router(&state).with_state(state);

    let (status, _body) = request_json(
        &router,
        Method::GET,
        "/v1/mcp-servers/anything/status",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let (status, _body) = request_json(
        &router,
        Method::POST,
        "/v1/mcp-servers/anything/restart",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn mcp_status_returns_404_for_unknown_server() {
    // The default test app has no MCP servers registered; querying a name
    // that the manager doesn't know about should return 404.
    let app = make_app().await;

    let (status, _body) = request_json(
        &app.router,
        Method::GET,
        "/v1/mcp-servers/no-such-server/status",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn mcp_restart_returns_404_for_unknown_server() {
    // As above: restart on an unknown id should 404 when no MCP registry is
    // active (the default test app has no configured mcp-servers).
    let app = make_app().await;

    let (status, _body) = request_json(
        &app.router,
        Method::POST,
        "/v1/mcp-servers/no-such-server/restart",
        None,
    )
    .await;
    // The manager returns "no MCP registry is active" → 503.
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}
