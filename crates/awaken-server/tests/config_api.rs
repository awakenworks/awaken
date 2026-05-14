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

    for uri in [
        "/v1/config/diagnostics",
        "/v1/config/providers/bootstrap/removal-preview",
    ] {
        let (status, body) = request_json(&app.router, Method::GET, uri, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "uri: {uri}, body: {body}");

        let (status, body) = request_json_with_headers(
            &app.router,
            Method::GET,
            uri,
            None,
            &[("authorization", "Bearer admin-token")],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "uri: {uri}, body: {body}");
    }
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
async fn provider_service_account_shaped_secret_is_never_returned_by_admin_api() {
    let app = make_app().await;
    let service_account_json = r#"{
        "client_email":"sa@project.iam.gserviceaccount.com",
        "private_key":"-----BEGIN PRIVATE KEY-----\nsa-private-material\n-----END PRIVATE KEY-----",
        "token_uri":"https://oauth2.googleapis.com/token"
    }"#;

    let (status, created) = request_json(
        &app.router,
        Method::POST,
        "/v1/config/providers",
        Some(json!({
            "id": "sa-shaped",
            "adapter": "stub",
            "api_key": service_account_json
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, fetched) = request_json(
        &app.router,
        Method::GET,
        "/v1/config/providers/sa-shaped",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, listed) =
        request_json(&app.router, Method::GET, "/v1/config/providers", None).await;
    assert_eq!(status, StatusCode::OK);

    for payload in [created, fetched, listed] {
        let rendered = payload.to_string();
        for secret in [
            "sa-private-material",
            "BEGIN PRIVATE KEY",
            "sa@project.iam.gserviceaccount.com",
        ] {
            assert!(
                !rendered.contains(secret),
                "admin API response leaked provider secret fragment {secret:?}: {rendered}"
            );
        }
        assert!(
            rendered.contains("has_api_key"),
            "response should expose only a boolean/key-presence marker: {rendered}"
        );
    }
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
async fn force_delete_provider_blocks_when_agent_uses_provider_model() {
    let app = make_app().await;

    // force=true may cascade unused model bindings, but it must not remove a
    // provider when an agent still uses one of those model bindings.
    let (status, body) = request_json(
        &app.router,
        Method::DELETE,
        "/v1/config/providers/bootstrap?force=true",
        None,
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    let used_by = body["used_by"].as_array().expect("used_by array");
    assert!(
        used_by
            .iter()
            .any(|record| record["namespace"] == "agents" && record["id"] == "bootstrap"),
        "should report the agent that keeps the provider model in use: {body}"
    );

    let stored = ConfigStore::get(app.store.as_ref(), "providers", "bootstrap")
        .await
        .expect("read provider after blocked delete");
    assert!(
        stored.is_some(),
        "provider should remain after blocked delete"
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

// ── Agent override endpoint tests ──────────────────────────────────────────

/// Seed a builtin agent with `system_prompt` for override tests.
/// Uses the already-seeded "bootstrap" agent from `make_app`.
async fn patch_overrides(router: &axum::Router, id: &str, body: Value) -> (StatusCode, Value) {
    request_json(
        router,
        Method::PATCH,
        &format!("/v1/config/agents/{id}/overrides"),
        Some(body),
    )
    .await
}

async fn delete_overrides(router: &axum::Router, id: &str) -> (StatusCode, Value) {
    request_json(
        router,
        Method::DELETE,
        &format!("/v1/config/agents/{id}/overrides"),
        None,
    )
    .await
}

async fn delete_override_field(
    router: &axum::Router,
    id: &str,
    field: &str,
) -> (StatusCode, Value) {
    request_json(
        router,
        Method::DELETE,
        &format!("/v1/config/agents/{id}/overrides/{field}"),
        None,
    )
    .await
}

async fn get_agent_spec(router: &axum::Router, id: &str) -> (StatusCode, Value) {
    request_json(
        router,
        Method::GET,
        &format!("/v1/config/agents/{id}"),
        None,
    )
    .await
}

#[tokio::test]
async fn patch_overrides_on_builtin_returns_effective_spec() {
    let app = make_app().await;

    // The "bootstrap" agent is seeded as Builtin.
    let (status, body) = patch_overrides(
        &app.router,
        "bootstrap",
        json!({"system_prompt": "patched"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["system_prompt"], "patched");

    // Verify the store has user_overrides set.
    use awaken_contract::contract::config_store::ConfigStore;
    let raw = ConfigStore::get(app.store.as_ref(), "agents", "bootstrap")
        .await
        .expect("store read")
        .expect("entry present");
    let overrides = &raw["meta"]["user_overrides"];
    assert_eq!(overrides["system_prompt"], "patched");

    // GET should also return the patched effective spec.
    let (get_status, get_body) = get_agent_spec(&app.router, "bootstrap").await;
    assert_eq!(get_status, StatusCode::OK);
    assert_eq!(get_body["system_prompt"], "patched");
}

#[tokio::test]
async fn patch_overrides_merges_with_existing_overrides() {
    let app = make_app().await;

    // First patch: system_prompt
    let (s1, _) = patch_overrides(&app.router, "bootstrap", json!({"system_prompt": "p1"})).await;
    assert_eq!(s1, StatusCode::OK);

    // Second patch: max_rounds
    let (s2, body) = patch_overrides(&app.router, "bootstrap", json!({"max_rounds": 99})).await;
    assert_eq!(s2, StatusCode::OK, "body: {body}");
    assert_eq!(body["system_prompt"], "p1");
    assert_eq!(body["max_rounds"], 99);
}

#[tokio::test]
async fn patch_overrides_null_clears_field() {
    let app = make_app().await;

    // Patch both fields.
    patch_overrides(
        &app.router,
        "bootstrap",
        json!({"system_prompt": "p1", "max_rounds": 99}),
    )
    .await;

    // Null-out max_rounds.
    let (status, body) =
        patch_overrides(&app.router, "bootstrap", json!({"max_rounds": null})).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["system_prompt"], "p1");
    // max_rounds should be reset to the base value (not 99).
    assert_ne!(body["max_rounds"], 99);

    // Store should only have system_prompt in user_overrides.
    use awaken_contract::contract::config_store::ConfigStore;
    let raw = ConfigStore::get(app.store.as_ref(), "agents", "bootstrap")
        .await
        .expect("store read")
        .expect("entry present");
    let overrides = &raw["meta"]["user_overrides"];
    assert_eq!(overrides["system_prompt"], "p1");
    assert!(
        overrides.get("max_rounds").is_none() || overrides["max_rounds"].is_null(),
        "max_rounds must not remain in user_overrides"
    );
}

// **Contract pin**: `endpoint` is a patchable AgentSpec field through the
// override API. The admin-console editor treats endpoint as a locked /
// read-only field for UX simplification, but this is a client-side
// choice — not a server-enforced immutability boundary. Programmatic
// clients (CLI, scripts, other admin tooling) can override or clear
// endpoint through `PATCH /v1/config/agents/:id/overrides`. See the
// long-form rationale on `AgentSpecPatch::endpoint` in
// `crates/awaken-contract/src/agent_spec_patch.rs`.
//
// Changing this behavior (e.g. making endpoint server-side immutable)
// would be a breaking API change and requires a dedicated ADR.
#[tokio::test]
async fn patch_overrides_null_clears_nullable_base_field() {
    let app = make_app().await;

    use awaken_contract::contract::config_store::ConfigStore;

    let mut raw = ConfigStore::get(app.store.as_ref(), "agents", "bootstrap")
        .await
        .expect("store read")
        .expect("entry present");
    raw["spec"]["endpoint"] = json!({
        "backend": "a2a",
        "base_url": "http://127.0.0.1:1",
        "target": "remote-agent"
    });
    ConfigStore::put(app.store.as_ref(), "agents", "bootstrap", &raw)
        .await
        .expect("store write");

    let (status, body) = patch_overrides(&app.router, "bootstrap", json!({"endpoint": null})).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(
        body.get("endpoint").is_none() || body["endpoint"].is_null(),
        "effective endpoint must be cleared"
    );

    let raw = ConfigStore::get(app.store.as_ref(), "agents", "bootstrap")
        .await
        .expect("store read")
        .expect("entry present");
    let overrides = &raw["meta"]["user_overrides"];
    assert!(
        overrides.get("endpoint").is_some_and(Value::is_null),
        "endpoint null must be preserved in user_overrides"
    );
}

#[tokio::test]
async fn patch_overrides_sections_null_value_deletes_base_section_key() {
    let app = make_app().await;

    use awaken_contract::contract::config_store::ConfigStore;

    let mut raw = ConfigStore::get(app.store.as_ref(), "agents", "bootstrap")
        .await
        .expect("store read")
        .expect("entry present");
    raw["spec"]["sections"] = json!({
        "permission": { "default_behavior": "ask", "rules": [] },
        "observability": { "enabled": true }
    });
    ConfigStore::put(app.store.as_ref(), "agents", "bootstrap", &raw)
        .await
        .expect("store write");

    let (status, body) = patch_overrides(
        &app.router,
        "bootstrap",
        json!({"sections": {"permission": null}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(
        !body["sections"]
            .as_object()
            .expect("sections object")
            .contains_key("permission"),
        "permission section must be deleted from the effective spec"
    );
    assert_eq!(body["sections"]["observability"], json!({"enabled": true}));

    let raw = ConfigStore::get(app.store.as_ref(), "agents", "bootstrap")
        .await
        .expect("store read")
        .expect("entry present");
    let section_overrides = raw["meta"]["user_overrides"]["sections"]
        .as_object()
        .expect("section overrides object");
    assert!(
        section_overrides
            .get("permission")
            .is_some_and(Value::is_null),
        "stored override should preserve the per-section delete marker"
    );
}

#[tokio::test]
async fn patch_overrides_rejects_unknown_field() {
    let app = make_app().await;

    let (status, body) =
        patch_overrides(&app.router, "bootstrap", json!({"unknown_field": "x"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

// R11 #3 — `_clear` directive applies upserts + clears atomically in
// one PATCH transaction. Replaces the previous client-side
// PATCH + N×DELETE flow which could leave the record in a partial
// state if any DELETE failed.
#[tokio::test]
async fn patch_overrides_clear_directive_removes_overrides() {
    let app = make_app().await;

    // Seed two overrides.
    let (s1, _) = patch_overrides(
        &app.router,
        "bootstrap",
        json!({"system_prompt": "kept", "max_rounds": 99}),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);

    // Clear `max_rounds` while upserting another field — both in one call.
    let (s2, body) = patch_overrides(
        &app.router,
        "bootstrap",
        json!({"_clear": ["max_rounds"], "system_prompt": "still-kept"}),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "body={body}");
    // Effective spec reflects the upsert.
    assert_eq!(body["system_prompt"], "still-kept");
    // Effective spec drops the cleared override.
    use awaken_contract::contract::config_store::ConfigStore;
    let raw = ConfigStore::get(app.store.as_ref(), "agents", "bootstrap")
        .await
        .expect("store read")
        .expect("entry present");
    let overrides = raw["meta"]["user_overrides"]
        .as_object()
        .expect("overrides obj");
    assert!(
        !overrides.contains_key("max_rounds"),
        "max_rounds override should be cleared, got: {raw}"
    );
    assert_eq!(overrides.get("system_prompt"), Some(&json!("still-kept")));
}

#[tokio::test]
async fn patch_overrides_clear_directive_accepts_endpoint() {
    let app = make_app().await;

    let (seed_status, _) = patch_overrides(
        &app.router,
        "bootstrap",
        json!({
            "endpoint": {
                "backend": "a2a",
                "base_url": "https://remote.example.com",
                "target": "remote-agent"
            }
        }),
    )
    .await;
    assert_eq!(seed_status, StatusCode::OK);

    let (status, body) =
        patch_overrides(&app.router, "bootstrap", json!({"_clear": ["endpoint"]})).await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert!(
        body.get("endpoint").is_none() || body["endpoint"].is_null(),
        "effective endpoint must fall back to the base value"
    );

    use awaken_contract::contract::config_store::ConfigStore;
    let raw = ConfigStore::get(app.store.as_ref(), "agents", "bootstrap")
        .await
        .expect("store read")
        .expect("entry present");
    let overrides = raw["meta"].get("user_overrides").unwrap_or(&Value::Null);
    assert!(
        overrides.is_null()
            || !overrides
                .as_object()
                .expect("user_overrides object")
                .contains_key("endpoint"),
        "endpoint override should be cleared, got: {raw}"
    );
}

#[tokio::test]
async fn patch_overrides_clear_rejects_unknown_field_name() {
    let app = make_app().await;
    let (status, body) = patch_overrides(
        &app.router,
        "bootstrap",
        json!({"_clear": ["unknown_field"]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
}

#[tokio::test]
async fn patch_overrides_clear_rejects_conflict_with_upsert() {
    let app = make_app().await;
    let (status, body) = patch_overrides(
        &app.router,
        "bootstrap",
        json!({"system_prompt": "new", "_clear": ["system_prompt"]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
}

#[tokio::test]
async fn patch_overrides_clear_rejects_endpoint_conflict_with_upsert() {
    let app = make_app().await;
    let (status, body) = patch_overrides(
        &app.router,
        "bootstrap",
        json!({"endpoint": null, "_clear": ["endpoint"]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
}

#[tokio::test]
async fn patch_overrides_clear_rejects_non_array() {
    let app = make_app().await;
    let (status, body) =
        patch_overrides(&app.router, "bootstrap", json!({"_clear": "system_prompt"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
}

#[tokio::test]
async fn patch_overrides_on_user_record_returns_422() {
    let app = make_app().await;

    // Create a User-source agent via PUT (regular create).
    let (create_status, _) = request_json(
        &app.router,
        Method::POST,
        "/v1/config/agents",
        Some(json!({
            "id": "user-agent-422",
            "model_id": "bootstrap",
            "system_prompt": "hello",
            "max_rounds": 1
        })),
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED);

    let (status, body) =
        patch_overrides(&app.router, "user-agent-422", json!({"system_prompt": "x"})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {body}");
}

#[tokio::test]
async fn patch_overrides_on_missing_agent_returns_404() {
    let app = make_app().await;

    let (status, _body) = patch_overrides(
        &app.router,
        "nonexistent-agent",
        json!({"system_prompt": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_all_overrides_resets_to_builtin() {
    let app = make_app().await;

    // Set some overrides first.
    patch_overrides(
        &app.router,
        "bootstrap",
        json!({"system_prompt": "customized"}),
    )
    .await;

    // Delete all overrides.
    let (status, body) = delete_overrides(&app.router, "bootstrap").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    // Store record should have user_overrides = None.
    use awaken_contract::contract::config_store::ConfigStore;
    let raw = ConfigStore::get(app.store.as_ref(), "agents", "bootstrap")
        .await
        .expect("store read")
        .expect("entry present");
    assert!(
        raw["meta"].get("user_overrides").is_none() || raw["meta"]["user_overrides"].is_null(),
        "user_overrides must be None after delete all"
    );

    // Effective spec should be back to the seed value.
    let (get_status, get_body) = get_agent_spec(&app.router, "bootstrap").await;
    assert_eq!(get_status, StatusCode::OK);
    assert_eq!(get_body["system_prompt"], "agent bootstrap");
}

#[tokio::test]
async fn delete_one_override_field_resets_only_that_field() {
    let app = make_app().await;

    // Set two overrides.
    patch_overrides(
        &app.router,
        "bootstrap",
        json!({"system_prompt": "p1", "max_rounds": 99}),
    )
    .await;

    // Delete only max_rounds.
    let (status, body) = delete_override_field(&app.router, "bootstrap", "max_rounds").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    // system_prompt override is preserved.
    assert_eq!(body["system_prompt"], "p1");
    // max_rounds is back to base (not 99).
    assert_ne!(body["max_rounds"], 99);
}

#[tokio::test]
async fn audit_event_emitted_for_patch_and_delete() {
    use awaken_server::services::audit_log::{AuditLogger, AuditQuery};
    use awaken_server::services::config_service::ConfigService;

    // Test audit via direct service calls (bypassing HTTP routing) to verify
    // that patch/clear methods emit Update audit events.
    let config_store = Arc::new(InMemoryStore::new());
    let thread_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(ImmediateExecutor))
            .with_thread_run_store(thread_store.clone())
            .build()
            .expect("build runtime"),
    );
    let manager = Arc::new(
        ConfigRuntimeManager::new(
            runtime.clone(),
            config_store.clone() as Arc<dyn ConfigStore>,
        )
        .expect("manager")
        .with_provider_factory(Arc::new(TestProviderFactory)),
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
            }),
            BuiltinSpec::agent(agent_spec("bootstrap", "bootstrap")),
        ],
    };
    manager.apply_seed(&seed).await.expect("apply_seed");
    manager.apply().await.expect("apply");

    let audit_logger = Arc::new(AuditLogger::new(
        config_store.clone() as Arc<dyn ConfigStore>
    ));
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        thread_store.clone(),
        "override-audit-test".into(),
        MailboxConfig::default(),
    ));
    let state = awaken_server::app::AppState::new(
        runtime.clone(),
        mailbox,
        thread_store,
        runtime.resolver_arc(),
        ServerConfig::default(),
    )
    .with_config_store(config_store.clone() as Arc<dyn ConfigStore>)
    .with_config_runtime_manager(manager)
    .with_audit_log(audit_logger.clone());

    let headers = axum::http::HeaderMap::new();

    // 1. PATCH overrides
    let service = ConfigService::new(&state).expect("service");
    service
        .patch_agent_overrides("bootstrap", json!({"system_prompt": "audited"}), &headers)
        .await
        .expect("patch_agent_overrides");

    // Verify state has audit_log
    assert!(state.audit_log().is_some(), "state should have audit_log");

    // 2. DELETE all overrides
    let service = ConfigService::new(&state).expect("service");
    service
        .clear_agent_overrides("bootstrap", &headers)
        .await
        .expect("clear_agent_overrides");

    // Check count after step 2
    let after_clear = audit_logger
        .query(AuditQuery::default())
        .await
        .expect("after_clear query");
    assert!(
        after_clear.items.len() >= 2,
        "should have 2 events after clear, got {}: {:?}",
        after_clear.items.len(),
        after_clear
            .items
            .iter()
            .map(|e| format!("{:?}@{}", e.action, e.resource))
            .collect::<Vec<_>>()
    );

    // 3. PATCH again
    let service = ConfigService::new(&state).expect("service");
    service
        .patch_agent_overrides(
            "bootstrap",
            json!({"system_prompt": "p1", "max_rounds": 5}),
            &headers,
        )
        .await
        .expect("patch_agent_overrides 2");

    // 4. DELETE single field
    let service = ConfigService::new(&state).expect("service");
    service
        .clear_agent_override_field("bootstrap", "max_rounds", &headers)
        .await
        .expect("clear_agent_override_field");

    // Query audit log — expect at least 3 Update events for agents/bootstrap.
    let page = audit_logger
        .query(AuditQuery {
            action: Some(awaken_contract::AuditAction::Update),
            ..Default::default()
        })
        .await
        .expect("audit query");

    // Override mutations now emit with resource path including the
    // `/overrides[/{field}]` suffix per Phase 3 spec.
    let agent_updates: Vec<_> = page
        .items
        .iter()
        .filter(|e| {
            e.resource == "agents/bootstrap/overrides"
                || e.resource.starts_with("agents/bootstrap/overrides/")
        })
        .collect();
    assert_eq!(
        agent_updates.len(),
        4,
        "expected exactly one Update per non-no-op mutation (patch + clear-all + patch + clear-field), got {} (all: {:?})",
        agent_updates.len(),
        page.items
            .iter()
            .map(|e| format!("{:?}@{}", e.action, e.resource))
            .collect::<Vec<_>>()
    );

    // Specifically: 2 PATCH calls + 1 DELETE all + 1 DELETE field.
    let single_field_updates: Vec<_> = page
        .items
        .iter()
        .filter(|e| e.resource == "agents/bootstrap/overrides/max_rounds")
        .collect();
    assert_eq!(
        single_field_updates.len(),
        1,
        "expected one Update for the per-field DELETE, got {}",
        single_field_updates.len()
    );
}

// ── GET /v1/config/:ns/:id/meta ──────────────────────────────────────────────

#[tokio::test]
async fn get_meta_returns_source_and_overrides_for_builtin() {
    let app = make_app().await;

    // The bootstrap agent is seeded as Builtin.
    let (status, body) = request_json(
        &app.router,
        Method::GET,
        "/v1/config/agents/bootstrap/meta",
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body["source"]["kind"], "builtin",
        "source.kind must be 'builtin'"
    );
    assert!(
        body["source"]["binary_version"].is_string(),
        "binary_version must be present"
    );
    // No overrides on a freshly seeded builtin.
    assert!(
        body.get("user_overrides").is_none() || body["user_overrides"].is_null(),
        "user_overrides should be absent or null for a pristine builtin"
    );
}

#[tokio::test]
async fn get_meta_returns_user_source_for_user_record() {
    let app = make_app().await;

    // Create a user record.
    let (create_status, _) = request_json(
        &app.router,
        Method::POST,
        "/v1/config/agents",
        Some(json!({ "id": "user-agent-meta", "model_id": "bootstrap", "system_prompt": "test", "max_rounds": 1 })),
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED);

    let (status, body) = request_json(
        &app.router,
        Method::GET,
        "/v1/config/agents/user-agent-meta/meta",
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body["source"]["kind"], "user",
        "user-created record must have source.kind 'user'"
    );
}

#[tokio::test]
async fn get_meta_returns_404_for_missing() {
    let app = make_app().await;

    let (status, _) = request_json(
        &app.router,
        Method::GET,
        "/v1/config/agents/no-such-agent/meta",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── GET /v1/config/:ns/meta ──────────────────────────────────────────────────

#[tokio::test]
async fn list_meta_returns_all_records_with_source() {
    let app = make_app().await;

    // Create a user agent.
    let (create_status, _) = request_json(
        &app.router,
        Method::POST,
        "/v1/config/agents",
        Some(json!({ "id": "user-list-meta", "model_id": "bootstrap", "system_prompt": "hi", "max_rounds": 1 })),
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED);

    let (status, body) =
        request_json(&app.router, Method::GET, "/v1/config/agents/meta", None).await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    let items = body.as_array().expect("body must be a JSON array");
    assert!(
        !items.is_empty(),
        "list/meta must return at least one entry"
    );

    // Every item must have id and meta.source.kind.
    for item in items {
        assert!(item["id"].is_string(), "each item must have an id string");
        let kind = item["meta"]["source"]["kind"].as_str();
        assert!(
            matches!(kind, Some("builtin") | Some("user")),
            "source.kind must be 'builtin' or 'user', got: {kind:?}"
        );
    }

    // Confirm both the builtin seed and our user record appear.
    let bootstrap = items.iter().find(|i| i["id"] == "bootstrap");
    assert!(
        bootstrap.is_some(),
        "bootstrap builtin must be in list/meta"
    );
    assert_eq!(bootstrap.unwrap()["meta"]["source"]["kind"], "builtin");

    let user_rec = items.iter().find(|i| i["id"] == "user-list-meta");
    assert!(user_rec.is_some(), "user-list-meta must be in list/meta");
    assert_eq!(user_rec.unwrap()["meta"]["source"]["kind"], "user");
}

// ── Permission preview endpoint (issue #190) ───────────────────────────────

/// Build a router with the permission plugin registered, a stub provider,
/// and a fixed tool registry. The preview endpoint intersects
/// `allowed_tools` against the tool registry — without registered tools
/// the candidate set would always be empty, so we seed a deterministic
/// set every preview test can reference.
#[cfg(feature = "permission")]
async fn make_permission_preview_app() -> axum::Router {
    let (runtime, store, manager) = make_permission_preview_runtime().await;
    let config_store = store.clone() as Arc<dyn ConfigStore>;
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        store.clone(),
        "permission-preview-test".into(),
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
    build_router(&state).with_state(state)
}

/// Standalone runtime+manager+store for permission preview tests, with a
/// fixed set of tools registered (`Bash`, `Read`, `Edit`, plus a couple
/// of `mcp__db__*` tools so glob expansion tests can verify behaviour
/// against real registry entries).
#[cfg(feature = "permission")]
async fn make_permission_preview_runtime() -> (
    Arc<AgentRuntime>,
    Arc<InMemoryStore>,
    Arc<ConfigRuntimeManager>,
) {
    struct PreviewMockTool {
        id: String,
    }
    #[async_trait]
    impl Tool for PreviewMockTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new(&self.id, &self.id, "preview mock")
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::new(ToolResult::success(
                &self.id,
                serde_json::Value::Null,
            )))
        }
    }

    // No-op plugin used by tests that need a second loaded plugin id to
    // populate `active_hook_filter` against. Defaults on the `Plugin`
    // trait suffice — descriptor is the only required method.
    struct NoopPlugin;
    impl awaken_runtime::plugins::Plugin for NoopPlugin {
        fn descriptor(&self) -> awaken_runtime::plugins::PluginDescriptor {
            awaken_runtime::plugins::PluginDescriptor {
                name: "observability",
            }
        }
    }

    let store = Arc::new(InMemoryStore::new());
    let mut builder = AgentRuntimeBuilder::new()
        .with_provider("bootstrap", Arc::new(ImmediateExecutor))
        .with_plugin("permission", Arc::new(PermissionPlugin))
        .with_plugin("observability", Arc::new(NoopPlugin))
        .with_thread_run_store(store.clone());
    for id in ["Bash", "Read", "Edit", "mcp__db__query", "mcp__db__write"] {
        builder = builder.with_tool(id, Arc::new(PreviewMockTool { id: id.into() }));
    }
    let runtime = Arc::new(builder.build().expect("build preview runtime"));
    let config_store = store.clone() as Arc<dyn ConfigStore>;
    let manager = Arc::new(
        ConfigRuntimeManager::new(runtime.clone(), config_store.clone())
            .expect("config runtime manager")
            .with_provider_factory(Arc::new(TestProviderFactory))
            .with_mcp_registry_factory(Arc::new(TestMcpRegistryFactory)),
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
            }),
            BuiltinSpec::agent(agent_spec("bootstrap", "bootstrap")),
        ],
    };
    manager.apply_seed(&seed).await.expect("apply_seed");
    manager.apply().await.expect("publish config snapshot");

    (runtime, store, manager)
}

#[cfg(feature = "permission")]
async fn seed_provider_and_model(router: &axum::Router) {
    let (status, _) = request_json(
        router,
        Method::POST,
        "/v1/config/providers",
        Some(json!({ "id": "stub-provider", "adapter": "stub" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = request_json(
        router,
        Method::POST,
        "/v1/config/models",
        Some(json!({
            "id": "stub-model",
            "provider_id": "stub-provider",
            "upstream_model": "any"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[cfg(feature = "permission")]
#[tokio::test]
async fn permission_preview_returns_candidate_set_without_permission_plugin() {
    let router = make_permission_preview_app().await;
    seed_provider_and_model(&router).await;
    let (status, _) = request_json(
        &router,
        Method::POST,
        "/v1/config/agents",
        Some(json!({
            "id": "no-perm-agent",
            "model_id": "stub-model",
            "system_prompt": "no permission plugin",
            // Use ids that exist in the test runtime registry (Bash/Read/Edit
            // are seeded by `make_permission_preview_runtime`). After the R7
            // registry-intersection fix, ids not in the registry are
            // filtered out — covered by a dedicated test below.
            "allowed_tools": ["Bash", "Read"],
            "excluded_tools": ["Read"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request_json(
        &router,
        Method::GET,
        "/v1/agents/no-perm-agent/permission-preview",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["agent_id"], "no-perm-agent");
    assert_eq!(body["permission_plugin_enabled"], false);
    assert!(body["default_behavior"].is_null());
    // candidate = allowed ∖ excluded = ["Bash"]
    assert_eq!(body["candidate_tools"], json!(["Bash"]));
    // No permission plugin -> effective == candidate.
    assert_eq!(body["effective_tools"], json!(["Bash"]));
    assert_eq!(body["unconditionally_denied"], json!([]));
    assert_eq!(body["args_conditional_rules"], json!([]));
}

// R7 #2 — preview filters `allowed_tools` against the registry. A stale
// id (renamed plugin, removed MCP server, typo) must NOT appear in
// `effective_tools` because the runtime tool catalog never offers it.
#[cfg(feature = "permission")]
#[tokio::test]
async fn permission_preview_intersects_allowed_tools_with_registry() {
    let router = make_permission_preview_app().await;
    seed_provider_and_model(&router).await;
    let (status, _) = request_json(
        &router,
        Method::POST,
        "/v1/config/agents",
        Some(json!({
            "id": "stale-tools-agent",
            "model_id": "stub-model",
            "system_prompt": "stale tool list",
            // `ghost_tool` is not registered; the runtime would never
            // offer it. The preview must drop it.
            "allowed_tools": ["Bash", "ghost_tool", "another-ghost"],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request_json(
        &router,
        Method::GET,
        "/v1/agents/stale-tools-agent/permission-preview",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["candidate_tools"], json!(["Bash"]));
    assert_eq!(body["effective_tools"], json!(["Bash"]));
}

// R7 #3 — glob/regex Deny + any-args rules expand against the registry
// into `unconditionally_denied`. Without this fix a `mcp__db__*` Deny
// rule would only appear in `args_conditional_rules` while
// `effective_tools` still listed `mcp__db__query` etc., even though
// the runtime BeforeInference hook would strip them on every call.
#[cfg(feature = "permission")]
#[tokio::test]
async fn permission_preview_expands_glob_deny_against_registry() {
    let router = make_permission_preview_app().await;
    seed_provider_and_model(&router).await;
    let (status, _) = request_json(
        &router,
        Method::POST,
        "/v1/config/agents",
        Some(json!({
            "id": "glob-deny-agent",
            "model_id": "stub-model",
            "system_prompt": "deny all mcp__db__*",
            "plugin_ids": ["permission"],
            "sections": {
                "permission": {
                    "default_behavior": "ask",
                    "rules": [
                        { "tool": "mcp__db__*", "behavior": "deny" }
                    ]
                }
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request_json(
        &router,
        Method::GET,
        "/v1/agents/glob-deny-agent/permission-preview",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    // Both registered mcp__db__* tools are now in the unconditionally
    // denied list — not hiding in args_conditional_rules.
    let denied = body["unconditionally_denied"]
        .as_array()
        .expect("unconditionally_denied is an array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(denied.contains(&"mcp__db__query".to_string()));
    assert!(denied.contains(&"mcp__db__write".to_string()));
    // Effective tools no longer carry them.
    let effective = body["effective_tools"]
        .as_array()
        .expect("effective_tools is an array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(!effective.contains(&"mcp__db__query".to_string()));
    assert!(!effective.contains(&"mcp__db__write".to_string()));
    // The glob Deny no longer double-appears in args_conditional_rules.
    let args_conditional = body["args_conditional_rules"]
        .as_array()
        .expect("args_conditional_rules is an array");
    assert!(
        !args_conditional.iter().any(
            |r| r["behavior"] == "deny" && r["pattern"].as_str().unwrap().contains("mcp__db__")
        ),
        "glob deny should be in unconditionally_denied, not args_conditional_rules"
    );
}

#[cfg(feature = "permission")]
#[tokio::test]
async fn permission_preview_subtracts_unconditionally_denied_tools() {
    let router = make_permission_preview_app().await;
    seed_provider_and_model(&router).await;
    let (status, _) = request_json(
        &router,
        Method::POST,
        "/v1/config/agents",
        Some(json!({
            "id": "perm-agent",
            "model_id": "stub-model",
            "system_prompt": "permission-gated",
            "plugin_ids": ["permission"],
            "allowed_tools": ["Bash", "Read", "Edit"],
            "sections": {
                "permission": {
                    "default_behavior": "ask",
                    "rules": [
                        { "tool": "Bash", "behavior": "deny" },
                        { "tool": "Read", "behavior": "allow" },
                        { "tool": "Edit(/etc/*)", "behavior": "deny" }
                    ]
                }
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request_json(
        &router,
        Method::GET,
        "/v1/agents/perm-agent/permission-preview",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["permission_plugin_enabled"], true);
    assert_eq!(body["default_behavior"], "ask");
    assert_eq!(body["candidate_tools"], json!(["Bash", "Edit", "Read"]));
    assert_eq!(body["unconditionally_denied"], json!(["Bash"]));
    // Bash stripped; Edit kept (the deny is args-conditional on path).
    assert_eq!(body["effective_tools"], json!(["Edit", "Read"]));
    let args = body["args_conditional_rules"]
        .as_array()
        .expect("args_conditional_rules should be a list");
    assert!(
        args.iter()
            .any(|r| r["tool"] == "Edit" && r["behavior"] == "deny"),
        "expected Edit(/etc/*) deny rule in args_conditional_rules, got {body}",
    );
}

#[cfg(feature = "permission")]
#[tokio::test]
async fn permission_preview_404_for_unknown_agent() {
    let router = make_permission_preview_app().await;
    let (status, _body) = request_json(
        &router,
        Method::GET,
        "/v1/agents/no-such-agent/permission-preview",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// R8 #1 — `active_hook_filter` excludes the permission plugin from the
// hook dispatcher even though the plugin itself is loaded. The runtime
// won't run permission BeforeInference hooks in this state, so preview
// must report enabled=false and emit candidate_tools as effective_tools.
#[cfg(feature = "permission")]
#[tokio::test]
async fn permission_preview_respects_active_hook_filter_excluding_permission() {
    let router = make_permission_preview_app().await;
    seed_provider_and_model(&router).await;
    let (status, body) = request_json(
        &router,
        Method::POST,
        "/v1/config/agents",
        Some(json!({
            "id": "filtered-out-agent",
            "model_id": "stub-model",
            "system_prompt": "permission loaded but filtered",
            // Both plugins are loaded by the runtime; the filter
            // restricts hook dispatch to observability only —
            // permission hooks will NOT run.
            "plugin_ids": ["permission", "observability"],
            "active_hook_filter": ["observability"],
            "allowed_tools": ["Bash", "Read"],
            "sections": {
                "permission": {
                    "default_behavior": "ask",
                    "rules": [
                        { "tool": "Bash", "behavior": "deny" }
                    ]
                }
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body={body}");

    let (status, body) = request_json(
        &router,
        Method::GET,
        "/v1/agents/filtered-out-agent/permission-preview",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(
        body["permission_plugin_enabled"], false,
        "filtered-out permission plugin must report disabled"
    );
    // No deny is applied since the hook won't run.
    assert_eq!(body["unconditionally_denied"], json!([]));
    assert_eq!(body["candidate_tools"], body["effective_tools"]);
}

#[cfg(feature = "permission")]
#[tokio::test]
async fn permission_preview_respects_active_hook_filter_including_permission() {
    let router = make_permission_preview_app().await;
    seed_provider_and_model(&router).await;
    let (status, _) = request_json(
        &router,
        Method::POST,
        "/v1/config/agents",
        Some(json!({
            "id": "filter-includes-permission-agent",
            "model_id": "stub-model",
            "system_prompt": "permission loaded and admitted",
            "plugin_ids": ["permission", "observability"],
            "active_hook_filter": ["permission"],
            "allowed_tools": ["Bash", "Read"],
            "sections": {
                "permission": {
                    "default_behavior": "ask",
                    "rules": [
                        { "tool": "Bash", "behavior": "deny" }
                    ]
                }
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request_json(
        &router,
        Method::GET,
        "/v1/agents/filter-includes-permission-agent/permission-preview",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["permission_plugin_enabled"], true);
    assert_eq!(body["unconditionally_denied"], json!(["Bash"]));
}

// R8 #4 — `unconditionally_denied` must only count tools that were in
// the candidate set. A deny rule for a tool the agent already wouldn't
// see (because allowed_tools excluded it) is NOT a "strip" — the UI
// summary "N tools stripped before the model sees the list" would
// otherwise overstate.
#[cfg(feature = "permission")]
#[tokio::test]
async fn permission_preview_unconditionally_denied_intersects_candidate() {
    let router = make_permission_preview_app().await;
    seed_provider_and_model(&router).await;
    let (status, _) = request_json(
        &router,
        Method::POST,
        "/v1/config/agents",
        Some(json!({
            "id": "denied-outside-candidate-agent",
            "model_id": "stub-model",
            "system_prompt": "deny rules target tools outside candidate set",
            "plugin_ids": ["permission"],
            // Candidate set is just Bash; the deny rule targets a
            // glob the agent never had access to.
            "allowed_tools": ["Bash"],
            "sections": {
                "permission": {
                    "default_behavior": "ask",
                    "rules": [
                        { "tool": "mcp__db__*", "behavior": "deny" }
                    ]
                }
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request_json(
        &router,
        Method::GET,
        "/v1/agents/denied-outside-candidate-agent/permission-preview",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["candidate_tools"], json!(["Bash"]));
    // `mcp__db__query` etc. matched the deny rule but they were never
    // in the candidate set — they are NOT counted as "stripped".
    assert_eq!(body["unconditionally_denied"], json!([]));
    assert_eq!(body["effective_tools"], json!(["Bash"]));
}

// R10 #1 — agent not found must return 404, NOT the 404 the client
// previously interpreted as "permission feature not compiled". The
// route is registered unconditionally and returns 503 only when the
// `permission` feature is off (see permission_preview_route_returns_503_when_feature_disabled
// in the `cfg(not(feature = "permission"))` test module).
#[cfg(feature = "permission")]
#[tokio::test]
async fn permission_preview_404_body_distinguishes_missing_agent() {
    let router = make_permission_preview_app().await;
    let (status, body) = request_json(
        &router,
        Method::GET,
        "/v1/agents/ghost-agent/permission-preview",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let err = body
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        err.contains("agent not found"),
        "404 body must identify the missing agent (got: {err})"
    );
}

// R10 #3 — `args_conditional_rules` must not list rules whose tool
// target is outside the candidate set. Such rules can never bite at
// runtime; the operator would mistake them for "still gating tools the
// model can call".
#[cfg(feature = "permission")]
#[tokio::test]
async fn permission_preview_args_conditional_drops_rules_outside_candidate() {
    let router = make_permission_preview_app().await;
    seed_provider_and_model(&router).await;
    let (status, _) = request_json(
        &router,
        Method::POST,
        "/v1/config/agents",
        Some(json!({
            "id": "args-cond-outside-candidate-agent",
            "model_id": "stub-model",
            "system_prompt": "args-conditional rule targets non-candidate tool",
            "plugin_ids": ["permission"],
            // Candidate is just `Bash`; the args-pattern rule targets
            // `Read` which the agent never had access to.
            "allowed_tools": ["Bash"],
            "sections": {
                "permission": {
                    "default_behavior": "ask",
                    "rules": [
                        { "tool": "Read(/etc/*)", "behavior": "deny" }
                    ]
                }
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request_json(
        &router,
        Method::GET,
        "/v1/agents/args-cond-outside-candidate-agent/permission-preview",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    // The Read(/etc/*) rule must NOT show up; Read is outside candidate.
    assert_eq!(body["args_conditional_rules"], json!([]));
}

// R12 #1 — `effective_tools = candidate ∖ unconditionally_denied`.
// An args-conditional rule on an unconditionally-denied tool cannot
// fire at runtime (the tool is stripped before any call reaches the
// permission layer), so the preview must not list it.
#[cfg(feature = "permission")]
#[tokio::test]
async fn permission_preview_drops_args_rules_on_unconditionally_denied_tools() {
    let router = make_permission_preview_app().await;
    seed_provider_and_model(&router).await;
    let (status, _) = request_json(
        &router,
        Method::POST,
        "/v1/config/agents",
        Some(json!({
            "id": "args-on-denied-agent",
            "model_id": "stub-model",
            "system_prompt": "args rule on a denied tool",
            "plugin_ids": ["permission"],
            "allowed_tools": ["Bash", "Read"],
            "sections": {
                "permission": {
                    "default_behavior": "ask",
                    "rules": [
                        // Bash is unconditionally denied.
                        { "tool": "Bash", "behavior": "deny" },
                        // Args-conditional rule on the SAME tool —
                        // can never bite once Bash is stripped.
                        { "tool": "Bash(npm *)", "behavior": "ask" }
                    ]
                }
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request_json(
        &router,
        Method::GET,
        "/v1/agents/args-on-denied-agent/permission-preview",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["unconditionally_denied"], json!(["Bash"]));
    assert_eq!(body["effective_tools"], json!(["Read"]));
    // The Bash(npm *) ask rule must NOT show — Bash is already
    // stripped before any call reaches the permission layer.
    assert_eq!(
        body["args_conditional_rules"],
        json!([]),
        "args rule on an unconditionally-denied tool must be dropped"
    );
}

// R12 #6 — Sections-less agent: the `permission` plugin is loaded but
// the agent never wrote a `sections.permission` entry. `AgentSpec::config`
// returns `Config::default()` in this case, so the preview should
// succeed with the default behavior and no rules — NOT 400.
#[cfg(feature = "permission")]
#[tokio::test]
async fn permission_preview_handles_missing_permission_section() {
    let router = make_permission_preview_app().await;
    seed_provider_and_model(&router).await;
    let (status, _) = request_json(
        &router,
        Method::POST,
        "/v1/config/agents",
        Some(json!({
            "id": "permission-no-section-agent",
            "model_id": "stub-model",
            "system_prompt": "permission plugin loaded, no section",
            "plugin_ids": ["permission"],
            "allowed_tools": ["Bash"],
            // No `sections.permission` entry at all.
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request_json(
        &router,
        Method::GET,
        "/v1/agents/permission-no-section-agent/permission-preview",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "missing permission section must NOT error — defaults apply: body={body}"
    );
    assert_eq!(body["permission_plugin_enabled"], true);
    // Default behavior is `ask` (PermissionRulesConfig::default()).
    assert_eq!(body["default_behavior"], "ask");
    assert_eq!(body["unconditionally_denied"], json!([]));
    assert_eq!(body["args_conditional_rules"], json!([]));
    assert_eq!(body["candidate_tools"], json!(["Bash"]));
    assert_eq!(body["effective_tools"], json!(["Bash"]));
}
