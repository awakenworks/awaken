use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use awaken_contract::contract::config_store::{
    ConfigChangeEvent, ConfigChangeKind, ConfigChangeNotifier, ConfigChangeSubscriber, ConfigStore,
};
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::storage::StorageError;
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken_contract::{AgentSpec, McpServerSpec, ModelBindingSpec, ProviderSpec};
use awaken_runtime::AgentRuntime;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::registry::ToolRegistry;
use awaken_runtime::registry::memory::MapToolRegistry;
use awaken_runtime::registry::traits::ModelBinding;
use awaken_server::app::{
    AppState, ServerConfig, SkillCatalogArgument, SkillCatalogContext, SkillCatalogEntry,
    SkillCatalogProvider,
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
    let store = Arc::new(InMemoryStore::new());
    let bootstrap_provider = ProviderSpec {
        id: "bootstrap".into(),
        adapter: "stub".into(),
        ..Default::default()
    };
    let bootstrap_model = ModelBindingSpec {
        id: "bootstrap".into(),
        provider_id: "bootstrap".into(),
        upstream_model: "bootstrap-model".into(),
    };
    let bootstrap_agent = agent_spec("bootstrap", "bootstrap");

    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(ImmediateExecutor))
            .with_model_binding(
                "bootstrap",
                ModelBinding {
                    provider_id: "bootstrap".into(),
                    upstream_model: "bootstrap-model".into(),
                },
            )
            .with_agent_spec(bootstrap_agent.clone())
            .with_thread_run_store(store.clone())
            .build()
            .expect("build runtime"),
    );

    let config_store = store.clone() as Arc<dyn ConfigStore>;
    let mut manager = ConfigRuntimeManager::new(runtime.clone(), config_store.clone())
        .expect("config runtime manager")
        .with_provider_factory(Arc::new(TestProviderFactory))
        .with_mcp_registry_factory(mcp_registry_factory);
    if let Some(notifier) = change_notifier {
        manager = manager.with_change_notifier(notifier);
    }
    if let Some(interval) = mcp_refresh_interval {
        manager = manager.with_mcp_refresh_interval(interval);
    }
    let manager = Arc::new(manager);
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

    (runtime, store, manager)
}

async fn make_app() -> TestApp {
    make_app_with_skill_catalog(None).await
}

async fn make_app_with_skill_catalog(
    skill_catalog_provider: Option<Arc<dyn SkillCatalogProvider>>,
) -> TestApp {
    let notifier = Arc::new(TestConfigChangeNotifier::new());
    let (runtime, store, manager) =
        make_runtime_manager(Some(notifier.clone() as Arc<dyn ConfigChangeNotifier>)).await;
    let config_store = store.clone() as Arc<dyn ConfigStore>;

    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        "config-api-test".into(),
        MailboxConfig::default(),
    ));
    let mut state = AppState::new(
        runtime.clone(),
        mailbox,
        store.clone(),
        runtime.resolver_arc(),
        ServerConfig::default(),
    )
    .with_config_store(config_store)
    .with_config_runtime_manager(manager.clone());
    if let Some(provider) = skill_catalog_provider {
        state = state.with_skill_catalog_provider(provider);
    }

    TestApp {
        router: build_router().with_state(state),
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
async fn delete_rolls_back_when_runtime_apply_would_break_graph() {
    let app = make_app().await;

    let (status, body) = request_json(
        &app.router,
        Method::DELETE,
        "/v1/config/providers/bootstrap",
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
