use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_server_contract::contract::config_store::ConfigStore;
use awaken_server_contract::contract::executor::{
    InferenceExecutionError, InferenceRequest, LlmExecutor,
};
use awaken_server_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_server_contract::{
    AgentSpec, BuiltinSeedSet, BuiltinSpec, ConfigRecord, ModelSpec, ProviderSpec, RecordMeta,
    SkillSpec,
};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::app::{ServerConfig, ServerState};
use crate::mailbox::{Mailbox, MailboxConfig};
use crate::services::config_runtime::{ConfigRuntimeManager, ProviderExecutorFactory};

use super::{
    ConfigNamespace, ConfigService, ConfigServiceError, TOOLS_NAMESPACE, tool_schema_json,
};

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
    fn build(
        &self,
        spec: &ProviderSpec,
    ) -> Result<Arc<dyn LlmExecutor>, crate::services::config_runtime::ConfigRuntimeError> {
        if spec.adapter.eq_ignore_ascii_case("stub") {
            return Ok(Arc::new(ImmediateExecutor));
        }

        Err(
            crate::services::config_runtime::ConfigRuntimeError::UnsupportedProviderAdapter(
                spec.adapter.clone(),
            ),
        )
    }
}

struct BlockingConfigStore {
    inner: Arc<awaken_stores::InMemoryStore>,
    block_lists: AtomicBool,
    list_started: AtomicBool,
    release_lists: Notify,
}

struct FailingModelDeleteConfigStore {
    inner: Arc<awaken_stores::InMemoryStore>,
    fail_model_delete_call: usize,
    model_delete_calls: AtomicUsize,
}

impl FailingModelDeleteConfigStore {
    fn new(inner: Arc<awaken_stores::InMemoryStore>, fail_model_delete_call: usize) -> Self {
        Self {
            inner,
            fail_model_delete_call,
            model_delete_calls: AtomicUsize::new(0),
        }
    }
}

impl BlockingConfigStore {
    fn new(inner: Arc<awaken_stores::InMemoryStore>) -> Self {
        Self {
            inner,
            block_lists: AtomicBool::new(false),
            list_started: AtomicBool::new(false),
            release_lists: Notify::new(),
        }
    }

    fn block_lists(&self) {
        self.list_started.store(false, Ordering::SeqCst);
        self.block_lists.store(true, Ordering::SeqCst);
    }

    fn unblock_lists(&self) {
        self.block_lists.store(false, Ordering::SeqCst);
        self.release_lists.notify_waiters();
    }

    fn list_started(&self) -> bool {
        self.list_started.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ConfigStore for BlockingConfigStore {
    async fn get(
        &self,
        namespace: &str,
        id: &str,
    ) -> Result<Option<Value>, awaken_server_contract::contract::storage::StorageError> {
        ConfigStore::get(self.inner.as_ref(), namespace, id).await
    }

    async fn list(
        &self,
        namespace: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<(String, Value)>, awaken_server_contract::contract::storage::StorageError> {
        if self.block_lists.load(Ordering::SeqCst) {
            self.list_started.store(true, Ordering::SeqCst);
            self.release_lists.notified().await;
        }

        ConfigStore::list(self.inner.as_ref(), namespace, offset, limit).await
    }

    async fn put(
        &self,
        namespace: &str,
        id: &str,
        value: &Value,
    ) -> Result<(), awaken_server_contract::contract::storage::StorageError> {
        ConfigStore::put(self.inner.as_ref(), namespace, id, value).await
    }

    async fn delete(
        &self,
        namespace: &str,
        id: &str,
    ) -> Result<(), awaken_server_contract::contract::storage::StorageError> {
        ConfigStore::delete(self.inner.as_ref(), namespace, id).await
    }
}

#[async_trait]
impl ConfigStore for FailingModelDeleteConfigStore {
    async fn get(
        &self,
        namespace: &str,
        id: &str,
    ) -> Result<Option<Value>, awaken_server_contract::contract::storage::StorageError> {
        ConfigStore::get(self.inner.as_ref(), namespace, id).await
    }

    async fn list(
        &self,
        namespace: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<(String, Value)>, awaken_server_contract::contract::storage::StorageError> {
        ConfigStore::list(self.inner.as_ref(), namespace, offset, limit).await
    }

    async fn put(
        &self,
        namespace: &str,
        id: &str,
        value: &Value,
    ) -> Result<(), awaken_server_contract::contract::storage::StorageError> {
        ConfigStore::put(self.inner.as_ref(), namespace, id, value).await
    }

    async fn delete(
        &self,
        namespace: &str,
        id: &str,
    ) -> Result<(), awaken_server_contract::contract::storage::StorageError> {
        ConfigStore::delete(self.inner.as_ref(), namespace, id).await
    }

    async fn put_if_absent(
        &self,
        namespace: &str,
        id: &str,
        value: &Value,
    ) -> Result<(), awaken_server_contract::contract::storage::StorageError> {
        ConfigStore::put_if_absent(self.inner.as_ref(), namespace, id, value).await
    }

    async fn put_if_revision(
        &self,
        namespace: &str,
        id: &str,
        value: &Value,
        expected_revision: u64,
    ) -> Result<(), awaken_server_contract::contract::storage::StorageError> {
        ConfigStore::put_if_revision(self.inner.as_ref(), namespace, id, value, expected_revision)
            .await
    }

    async fn delete_if_revision(
        &self,
        namespace: &str,
        id: &str,
        expected_revision: u64,
    ) -> Result<(), awaken_server_contract::contract::storage::StorageError> {
        if namespace == ConfigNamespace::Models.as_str() {
            let call = self.model_delete_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call == self.fail_model_delete_call {
                return Err(awaken_server_contract::contract::storage::StorageError::Io(
                    format!("forced model delete failure for {id}"),
                ));
            }
        }
        ConfigStore::delete_if_revision(self.inner.as_ref(), namespace, id, expected_revision).await
    }
}

fn bootstrap_agent() -> AgentSpec {
    AgentSpec {
        id: "bootstrap".into(),
        model_id: "bootstrap".into(),
        system_prompt: "bootstrap".into(),
        max_rounds: 1,
        ..Default::default()
    }
}

async fn build_state(
    config_store: Arc<dyn ConfigStore>,
) -> (ServerState, Arc<ConfigRuntimeManager>) {
    let thread_store = Arc::new(awaken_stores::InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(ImmediateExecutor))
            .with_in_memory_thread_run_store(thread_store.clone())
            .build()
            .expect("build runtime"),
    );

    let manager = Arc::new(
        ConfigRuntimeManager::new(runtime.clone(), config_store.clone())
            .expect("config runtime manager")
            .with_provider_factory(Arc::new(TestProviderFactory)),
    );
    let resolver = runtime.resolver_arc();
    let seed = BuiltinSeedSet {
        binary_version: "test".to_string(),
        specs: vec![
            BuiltinSpec::provider(ProviderSpec {
                id: "bootstrap".into(),
                adapter: "stub".into(),
                ..Default::default()
            }),
            BuiltinSpec::model(ModelSpec::new("bootstrap", "bootstrap", "bootstrap-model")),
            BuiltinSpec::agent(bootstrap_agent()),
        ],
    };
    manager.apply_seed(&seed).await.expect("apply_seed");
    manager.apply().await.expect("publish config");

    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        thread_store.clone(),
        "config-service-test".into(),
        MailboxConfig::default(),
    ));
    let mut state = ServerState::new(
        runtime,
        mailbox,
        thread_store,
        resolver,
        ServerConfig::default(),
    );
    state.config = Some(crate::app::ConfigModuleState::new(
        config_store,
        manager.clone(),
    ));

    (state, manager)
}

async fn wait_until(
    timeout: Duration,
    interval: Duration,
    mut predicate: impl FnMut() -> bool,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if predicate() {
            return true;
        }
        tokio::time::sleep(interval).await;
    }
    predicate()
}

#[tokio::test]
async fn create_waits_for_in_flight_apply_before_writing_store() {
    let raw_store = Arc::new(awaken_stores::InMemoryStore::new());
    let blocking_store = Arc::new(BlockingConfigStore::new(raw_store.clone()));
    let config_store = blocking_store.clone() as Arc<dyn ConfigStore>;
    let (state, manager) = build_state(config_store.clone()).await;

    blocking_store.block_lists();
    let apply_task = tokio::spawn({
        let manager = manager.clone();
        async move {
            manager
                .apply_if_changed()
                .await
                .expect("apply_if_changed should complete")
        }
    });

    let list_blocked = wait_until(Duration::from_secs(1), Duration::from_millis(10), || {
        blocking_store.list_started()
    })
    .await;
    assert!(
        list_blocked,
        "background apply should enter the config snapshot load"
    );

    let create_task = tokio::spawn({
        let state = state.clone();
        async move {
            let service = ConfigService::new(&state).expect("config service");
            service
                .create_with_headers(
                    ConfigNamespace::Providers,
                    json!({
                        "id": "serialized",
                        "adapter": "stub"
                    }),
                    &axum::http::HeaderMap::new(),
                )
                .await
        }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let pending = ConfigStore::get(config_store.as_ref(), "providers", "serialized")
        .await
        .expect("read provider");
    assert!(
        pending.is_none(),
        "config writes must wait for in-flight apply snapshots before touching the store"
    );
    assert!(
        !create_task.is_finished(),
        "create should stay blocked behind the apply lock"
    );

    blocking_store.unblock_lists();
    let apply_result = apply_task.await.expect("join apply task");
    assert_eq!(apply_result, None);

    let created = create_task
        .await
        .expect("join create task")
        .expect("create should succeed");
    assert_eq!(created["id"], "serialized");

    let stored = ConfigStore::get(config_store.as_ref(), "providers", "serialized")
        .await
        .expect("read provider after create");
    // The stored value is now a ConfigRecord envelope; extract id from spec layer.
    assert_eq!(
        stored
            .as_ref()
            .and_then(|value| {
                // Prefer spec layer for envelope, fall back to bare spec.
                value.get("spec").or(Some(value))
            })
            .and_then(|spec| spec.get("id"))
            .and_then(Value::as_str),
        Some("serialized")
    );
}

#[tokio::test]
async fn service_requires_runtime_manager_for_mutations() {
    let thread_store = Arc::new(awaken_stores::InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(ImmediateExecutor))
            .with_model(ModelSpec::new("bootstrap", "bootstrap", "bootstrap-model"))
            .with_agent_spec(bootstrap_agent())
            .with_in_memory_thread_run_store(thread_store.clone())
            .build()
            .expect("build runtime"),
    );
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        thread_store.clone(),
        "config-service-test".into(),
        MailboxConfig::default(),
    ));
    let state = ServerState::new(
        runtime.clone(),
        mailbox,
        thread_store,
        runtime.resolver_arc(),
        ServerConfig::default(),
    );

    let error = match ConfigService::new(&state) {
        Ok(service) => service
            .create_with_headers(
                ConfigNamespace::Providers,
                json!({
                    "id": "missing-manager",
                    "adapter": "stub"
                }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect_err("missing manager should reject writes"),
        Err(error) => error,
    };
    assert!(matches!(error, ConfigServiceError::NotEnabled));
}

// ── find_dependents / blocked delete tests ──────────────────────────────

#[tokio::test]
async fn find_dependents_provider_returns_referencing_models() {
    let config_store = Arc::new(awaken_stores::InMemoryStore::new());
    let (state, _manager) = build_state(config_store.clone()).await;
    let service = ConfigService::new(&state).expect("config service");

    // Create a model that references provider "bootstrap"
    service
        .create_with_headers(
            ConfigNamespace::Models,
            json!({
                "id": "model-ref-bootstrap",
                "provider_id": "bootstrap",
                "upstream_model": "gpt-4"
            }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("create model");

    let dependents = service
        .find_dependents(ConfigNamespace::Providers, "bootstrap")
        .await
        .expect("find_dependents");

    assert_eq!(dependents.len(), 2, "bootstrap model + model-ref-bootstrap");
    let ids: Vec<&str> = dependents.iter().map(|d| d.id.as_str()).collect();
    assert!(ids.contains(&"model-ref-bootstrap"));
    for d in &dependents {
        assert_eq!(d.namespace, "models");
    }
}

#[tokio::test]
async fn find_dependents_model_returns_referencing_agents() {
    let config_store = Arc::new(awaken_stores::InMemoryStore::new());
    let (state, _manager) = build_state(config_store.clone()).await;
    let service = ConfigService::new(&state).expect("config service");

    // Create an agent referencing the bootstrap model
    service
        .create_with_headers(
            ConfigNamespace::Agents,
            json!({
                "id": "agent-ref-bootstrap",
                "model_id": "bootstrap",
                "system_prompt": "test",
                "max_rounds": 1
            }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("create agent");

    let dependents = service
        .find_dependents(ConfigNamespace::Models, "bootstrap")
        .await
        .expect("find_dependents");

    assert!(!dependents.is_empty());
    let ids: Vec<&str> = dependents.iter().map(|d| d.id.as_str()).collect();
    assert!(ids.contains(&"agent-ref-bootstrap"));
    for d in &dependents {
        assert_eq!(d.namespace, "agents");
    }
}

#[tokio::test]
async fn find_dependents_model_uses_effective_agent_model_override() {
    let config_store = Arc::new(awaken_stores::InMemoryStore::new());
    let (state, _manager) = build_state(config_store.clone()).await;
    let service = ConfigService::new(&state).expect("config service");

    service
        .create_with_headers(
            ConfigNamespace::Providers,
            json!({ "id": "prov-b", "adapter": "stub" }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("create provider-b");
    service
        .create_with_headers(
            ConfigNamespace::Models,
            json!({
                "id": "model-b",
                "provider_id": "prov-b",
                "upstream_model": "gpt-4"
            }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("create model-b");

    let raw = ConfigStore::get(config_store.as_ref(), "agents", "bootstrap")
        .await
        .expect("read bootstrap agent")
        .expect("bootstrap agent exists");
    let mut record = awaken_server_contract::ConfigRecord::<AgentSpec>::from_value(raw)
        .expect("parse bootstrap agent record");
    record.meta.user_overrides = Some(json!({ "model_id": "model-b" }));
    ConfigStore::put(
        config_store.as_ref(),
        "agents",
        "bootstrap",
        &record.to_value().expect("serialize bootstrap override"),
    )
    .await
    .expect("write bootstrap override");

    let effective_deps = service
        .find_dependents(ConfigNamespace::Models, "model-b")
        .await
        .expect("find effective model dependents");
    assert!(effective_deps.iter().any(|dep| dep.id == "bootstrap"));

    let base_deps = service
        .find_dependents(ConfigNamespace::Models, "bootstrap")
        .await
        .expect("find base model dependents");
    assert!(!base_deps.iter().any(|dep| dep.id == "bootstrap"));

    let preview = service
        .preview_remove_provider("prov-b")
        .await
        .expect("preview provider removal");
    assert_eq!(preview.model_ids, vec!["model-b"]);
    assert_eq!(preview.agent_ids, vec!["bootstrap"]);
}

#[tokio::test]
async fn find_dependents_model_ignores_effective_remote_endpoint_agents() {
    let config_store = Arc::new(awaken_stores::InMemoryStore::new());
    let (state, _manager) = build_state(config_store.clone()).await;
    let service = ConfigService::new(&state).expect("config service");

    let raw = ConfigStore::get(config_store.as_ref(), "agents", "bootstrap")
        .await
        .expect("read bootstrap agent")
        .expect("bootstrap agent exists");
    let mut record = awaken_server_contract::ConfigRecord::<AgentSpec>::from_value(raw)
        .expect("parse bootstrap agent record");
    record.meta.user_overrides = Some(json!({
        "endpoint": {
            "base_url": "http://remote-agent.example/"
        }
    }));
    ConfigStore::put(
        config_store.as_ref(),
        "agents",
        "bootstrap",
        &record.to_value().expect("serialize endpoint override"),
    )
    .await
    .expect("write endpoint override");

    let dependents = service
        .find_dependents(ConfigNamespace::Models, "bootstrap")
        .await
        .expect("find model dependents");
    assert!(!dependents.iter().any(|dep| dep.id == "bootstrap"));
}

#[tokio::test]
async fn provider_removal_preview_ignores_effective_remote_endpoint_agents() {
    let config_store = Arc::new(awaken_stores::InMemoryStore::new());
    let (state, _manager) = build_state(config_store.clone()).await;
    let service = ConfigService::new(&state).expect("config service");

    service
        .create_with_headers(
            ConfigNamespace::Providers,
            json!({ "id": "prov-remote", "adapter": "stub" }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("create provider");
    service
        .create_with_headers(
            ConfigNamespace::Models,
            json!({
                "id": "model-remote",
                "provider_id": "prov-remote",
                "upstream_model": "gpt-4"
            }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("create model");
    service
        .create_with_headers(
            ConfigNamespace::Agents,
            json!({
                "id": "agent-remote",
                "model_id": "model-remote",
                "system_prompt": "remote",
                "max_rounds": 1,
                "endpoint": {
                    "base_url": "http://remote-agent.example/"
                }
            }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("create remote endpoint agent");

    let preview = service
        .preview_remove_provider("prov-remote")
        .await
        .expect("preview provider removal");
    assert_eq!(preview.model_ids, vec!["model-remote"]);
    assert!(
        preview.agent_ids.is_empty(),
        "remote endpoint agents must not block provider model cascade"
    );
}

#[tokio::test]
async fn provider_removal_preview_collects_dependents_across_multiple_models() {
    let config_store = Arc::new(awaken_stores::InMemoryStore::new());
    let (state, _manager) = build_state(config_store.clone()).await;
    let service = ConfigService::new(&state).expect("config service");

    for provider_id in ["prov-fanout", "prov-other"] {
        service
            .create_with_headers(
                ConfigNamespace::Providers,
                json!({ "id": provider_id, "adapter": "stub" }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create provider");
    }
    for (model_id, provider_id) in [
        ("fanout-a", "prov-fanout"),
        ("fanout-b", "prov-fanout"),
        ("fanout-c", "prov-fanout"),
        ("other-a", "prov-other"),
    ] {
        service
            .create_with_headers(
                ConfigNamespace::Models,
                json!({
                    "id": model_id,
                    "provider_id": provider_id,
                    "upstream_model": "gpt-4"
                }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create model");
    }
    for (agent_id, model_id) in [
        ("agent-uses-a", "fanout-a"),
        ("agent-uses-b", "fanout-b"),
        ("agent-uses-c-1", "fanout-c"),
        ("agent-uses-c-2", "fanout-c"),
        ("agent-uses-other", "other-a"),
    ] {
        service
            .create_with_headers(
                ConfigNamespace::Agents,
                json!({
                    "id": agent_id,
                    "model_id": model_id,
                    "system_prompt": "fanout",
                    "max_rounds": 1
                }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create agent");
    }

    let preview = service
        .preview_remove_provider("prov-fanout")
        .await
        .expect("preview provider removal");
    assert_eq!(
        preview.model_ids,
        vec!["fanout-a".to_string(), "fanout-b".into(), "fanout-c".into()]
    );
    assert_eq!(
        preview.agent_ids,
        vec![
            "agent-uses-a".to_string(),
            "agent-uses-b".into(),
            "agent-uses-c-1".into(),
            "agent-uses-c-2".into(),
        ],
        "preview must collect dependents across all provider models in a single pass"
    );
    assert!(!preview.block_if_referenced_allowed);
    assert!(!preview.cascade_unused_models_allowed);
}

#[tokio::test]
async fn test_provider_redacts_provider_secrets_from_error() {
    let config_store = Arc::new(awaken_stores::InMemoryStore::new());
    let (state, _manager) = build_state(config_store.clone()).await;
    let service = ConfigService::new(&state).expect("config service");

    let secret = "sk-provider-test-secret-redaction";
    let mut headers = serde_json::Map::new();
    headers.insert(format!("{secret} invalid"), json!("header-value"));
    let mut adapter_options = std::collections::BTreeMap::new();
    adapter_options.insert("headers".to_string(), Value::Object(headers));
    let record = awaken_server_contract::ConfigRecord {
        spec: ProviderSpec {
            id: "leaky-provider".into(),
            adapter: "openai".into(),
            api_key: Some(secret.to_string().into()),
            adapter_options,
            ..Default::default()
        },
        meta: awaken_server_contract::RecordMeta::new_user(),
    };
    ConfigStore::put(
        config_store.as_ref(),
        "providers",
        "leaky-provider",
        &record.to_value().expect("serialize provider"),
    )
    .await
    .expect("write provider");

    let result = service
        .test_provider("leaky-provider")
        .await
        .expect("test provider");

    assert!(!result.ok);
    let error = result.error.expect("provider test error");
    assert!(
        !error.contains(secret),
        "provider preflight error leaked secret: {error}"
    );
    assert!(
        error.contains("***"),
        "provider preflight error should include a redaction marker: {error}"
    );
}

#[tokio::test]
async fn find_dependents_agents_and_mcp_servers_are_leaf_nodes() {
    let config_store = Arc::new(awaken_stores::InMemoryStore::new());
    let (state, _manager) = build_state(config_store.clone()).await;
    let service = ConfigService::new(&state).expect("config service");

    let agent_deps = service
        .find_dependents(ConfigNamespace::Agents, "any-agent")
        .await
        .expect("find_dependents agents");
    assert!(agent_deps.is_empty());

    let mcp_deps = service
        .find_dependents(ConfigNamespace::McpServers, "any-mcp")
        .await
        .expect("find_dependents mcp-servers");
    assert!(mcp_deps.is_empty());
}

#[tokio::test]
async fn delete_without_force_returns_blocked_when_dependents_exist() {
    let config_store = Arc::new(awaken_stores::InMemoryStore::new());
    let (state, _manager) = build_state(config_store.clone()).await;
    let service = ConfigService::new(&state).expect("config service");

    // Create a second provider and a model referencing it
    service
        .create_with_headers(
            ConfigNamespace::Providers,
            json!({ "id": "prov-b", "adapter": "stub" }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("create provider-b");

    service
        .create_with_headers(
            ConfigNamespace::Models,
            json!({
                "id": "model-b",
                "provider_id": "prov-b",
                "upstream_model": "gpt-4"
            }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("create model-b");

    let err = service
        .delete_with_options(
            ConfigNamespace::Providers,
            "prov-b",
            false,
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect_err("should be blocked");

    assert!(
        matches!(err, ConfigServiceError::Conflict(ref message) if message.contains("model-b")),
        "expected dependency conflict, got {err:?}"
    );
}

#[tokio::test]
async fn delete_with_force_cascades_unused_provider_models() {
    let config_store = Arc::new(awaken_stores::InMemoryStore::new());
    let (state, _manager) = build_state(config_store.clone()).await;
    let service = ConfigService::new(&state).expect("config service");

    service
        .create_with_headers(
            ConfigNamespace::Providers,
            json!({ "id": "prov-c", "adapter": "stub" }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("create provider-c");

    service
        .create_with_headers(
            ConfigNamespace::Models,
            json!({
                "id": "model-c",
                "provider_id": "prov-c",
                "upstream_model": "gpt-4"
            }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("create model-c");

    service
        .delete_with_options(
            ConfigNamespace::Providers,
            "prov-c",
            true,
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("force delete should succeed");

    assert!(
        config_store
            .get(ConfigNamespace::Models.as_str(), "model-c")
            .await
            .unwrap()
            .is_none(),
        "provider force delete must remove model bindings that point to it"
    );
}

#[tokio::test]
async fn delete_with_force_rolls_back_cascade_when_model_delete_fails() {
    let raw_store = Arc::new(awaken_stores::InMemoryStore::new());
    let failing_store = Arc::new(FailingModelDeleteConfigStore::new(raw_store.clone(), 2));
    let config_store = failing_store.clone() as Arc<dyn ConfigStore>;
    let (state, _manager) = build_state(config_store).await;
    let service = ConfigService::new(&state).expect("config service");

    service
        .create_with_headers(
            ConfigNamespace::Providers,
            json!({ "id": "prov-e", "adapter": "stub" }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("create provider-e");

    for model_id in ["model-e-a", "model-e-b"] {
        service
            .create_with_headers(
                ConfigNamespace::Models,
                json!({
                    "id": model_id,
                    "provider_id": "prov-e",
                    "upstream_model": "gpt-4"
                }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create provider model");
    }

    let err = service
        .delete_with_options(
            ConfigNamespace::Providers,
            "prov-e",
            true,
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect_err("forced model delete failure must reject the delete");
    assert!(err.to_string().contains("forced model delete failure"));

    for (namespace, id) in [
        (ConfigNamespace::Providers.as_str(), "prov-e"),
        (ConfigNamespace::Models.as_str(), "model-e-a"),
        (ConfigNamespace::Models.as_str(), "model-e-b"),
    ] {
        assert!(
            ConfigStore::get(raw_store.as_ref(), namespace, id)
                .await
                .expect("read after rollback")
                .is_some(),
            "{namespace}/{id} should be restored after cascade failure"
        );
    }
}

#[tokio::test]
async fn delete_provider_with_force_blocks_when_agents_use_provider_models() {
    let config_store = Arc::new(awaken_stores::InMemoryStore::new());
    let (state, _manager) = build_state(config_store.clone()).await;
    let service = ConfigService::new(&state).expect("config service");

    service
        .create_with_headers(
            ConfigNamespace::Providers,
            json!({ "id": "prov-d", "adapter": "stub" }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("create provider-d");

    service
        .create_with_headers(
            ConfigNamespace::Models,
            json!({
                "id": "model-d",
                "provider_id": "prov-d",
                "upstream_model": "gpt-4"
            }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("create model-d");

    service
        .create_with_headers(
            ConfigNamespace::Agents,
            json!({
                "id": "agent-d",
                "model_id": "model-d",
                "system_prompt": "test"
            }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("create agent-d");

    let err = service
        .delete_with_options(
            ConfigNamespace::Providers,
            "prov-d",
            true,
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect_err("force delete must not orphan agent model references");

    assert!(
        matches!(err, ConfigServiceError::Conflict(ref message) if message.contains("agent-d")),
        "expected agent dependency blocker, got {err:?}"
    );
}

struct FailingProviderFactory;

impl ProviderExecutorFactory for FailingProviderFactory {
    fn build(
        &self,
        _spec: &ProviderSpec,
    ) -> Result<Arc<dyn LlmExecutor>, crate::services::config_runtime::ConfigRuntimeError> {
        Err(
            crate::services::config_runtime::ConfigRuntimeError::InvalidConfig(
                "forced failure for rollback test".into(),
            ),
        )
    }
}

#[tokio::test]
async fn delete_rollback_re_emits_envelope() {
    // Step 1: build a manager with the succeeding TestProviderFactory and PUT a provider.
    let config_store: Arc<dyn awaken_server_contract::contract::config_store::ConfigStore> =
        Arc::new(awaken_stores::InMemoryStore::new());
    let (state, _manager) = build_state(config_store.clone()).await;
    let service = ConfigService::new(&state).expect("config service");

    service
        .create_with_headers(
            ConfigNamespace::Providers,
            json!({ "id": "rollback-prov", "adapter": "stub" }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("create rollback-prov");

    // Step 2: verify the stored record is already an envelope (precondition).
    let stored_before = ConfigStore::get(config_store.as_ref(), "providers", "rollback-prov")
        .await
        .expect("read before delete")
        .expect("provider must exist");
    assert!(
        stored_before.get("spec").is_some(),
        "stored record must be envelope-shaped before delete (has 'spec' key)"
    );

    // Step 3: build a second manager over the same store, with FailingProviderFactory.
    let thread_store = Arc::new(awaken_stores::InMemoryStore::new());
    let runtime_failing = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(ImmediateExecutor))
            .with_in_memory_thread_run_store(thread_store.clone())
            .build()
            .expect("build runtime"),
    );
    let manager_failing = Arc::new(
        crate::services::config_runtime::ConfigRuntimeManager::new(
            runtime_failing.clone(),
            config_store.clone(),
        )
        .expect("config runtime manager")
        .with_provider_factory(Arc::new(FailingProviderFactory)),
    );

    let mailbox_failing = Arc::new(crate::mailbox::Mailbox::new(
        runtime_failing.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        thread_store.clone(),
        "rollback-test".into(),
        crate::mailbox::MailboxConfig::default(),
    ));
    let mut state_failing = crate::app::ServerState::new(
        runtime_failing.clone(),
        mailbox_failing,
        thread_store,
        runtime_failing.resolver_arc(),
        crate::app::ServerConfig::default(),
    );
    state_failing.config = Some(crate::app::ConfigModuleState::new(
        config_store.clone(),
        manager_failing,
    ));

    // Step 4: attempt DELETE via the failing service — apply_locked will fail.
    let service_failing = ConfigService::new(&state_failing).expect("failing config service");
    let delete_result = service_failing
        .delete_with_options(
            ConfigNamespace::Providers,
            "rollback-prov",
            true,
            &axum::http::HeaderMap::new(),
        )
        .await;

    assert!(
        delete_result.is_err(),
        "delete must fail when apply_locked fails"
    );

    // Step 5: assert the store still has the provider AND it is envelope-shaped.
    let stored_after = ConfigStore::get(config_store.as_ref(), "providers", "rollback-prov")
        .await
        .expect("read after delete")
        .expect("provider must have been rolled back");

    assert!(
        stored_after.get("spec").is_some(),
        "rolled-back record must be envelope-shaped (has 'spec' key)"
    );
    assert!(
        stored_after.get("meta").is_some(),
        "rolled-back record must be envelope-shaped (has 'meta' key)"
    );
    assert_eq!(
        stored_after["spec"]["id"],
        Value::String("rollback-prov".into()),
        "rolled-back spec must preserve the original provider id"
    );
}

// ── ConfigNamespace::all() / iter_str() tests ─────────────────────────

#[test]
fn namespace_all_lists_every_variant() {
    let all = ConfigNamespace::all();
    assert_eq!(all.len(), 6, "all writable config namespaces");

    // Each variant must appear exactly once.
    let has = |v: ConfigNamespace| all.iter().filter(|&&x| x == v).count();
    assert_eq!(has(ConfigNamespace::Agents), 1);
    assert_eq!(has(ConfigNamespace::Providers), 1);
    assert_eq!(has(ConfigNamespace::Models), 1);
    assert_eq!(has(ConfigNamespace::ModelPools), 1);
    assert_eq!(has(ConfigNamespace::McpServers), 1);
    assert_eq!(has(ConfigNamespace::Skills), 1);
}

#[test]
fn namespace_all_matches_builtin_spec_namespace() {
    use awaken_server_contract::{BuiltinSpec, McpServerSpec, ModelPoolSpec, SkillSpec};

    for &ns in ConfigNamespace::all() {
        let spec = match ns {
            ConfigNamespace::Agents => BuiltinSpec::Agent(Box::new(AgentSpec {
                id: "x".into(),
                model_id: "m".into(),
                system_prompt: "s".into(),
                ..Default::default()
            })),
            ConfigNamespace::Providers => BuiltinSpec::Provider(ProviderSpec {
                id: "x".into(),
                adapter: "openai".into(),
                ..Default::default()
            }),
            ConfigNamespace::Models => BuiltinSpec::Model(ModelSpec::new("x", "p", "m")),
            ConfigNamespace::ModelPools => BuiltinSpec::ModelPool(ModelPoolSpec::new("x", ["m"])),
            ConfigNamespace::McpServers => BuiltinSpec::McpServer(McpServerSpec {
                id: "x".into(),
                ..Default::default()
            }),
            ConfigNamespace::Skills => BuiltinSpec::Skill(SkillSpec {
                id: "x".into(),
                name: "x".into(),
                description: "x".into(),
                instructions_md: "x".into(),
                ..Default::default()
            }),
        };
        assert_eq!(
            spec.namespace(),
            ns.as_str(),
            "BuiltinSpec::namespace() drifted from ConfigNamespace::as_str() for {ns:?}"
        );
    }
}

#[tokio::test]
async fn get_skills_merges_user_overrides_into_effective_spec() {
    let raw_store = Arc::new(awaken_stores::InMemoryStore::new());
    let config_store = raw_store.clone() as Arc<dyn ConfigStore>;
    let (state, _manager) = build_state(config_store.clone()).await;

    let mut record = ConfigRecord {
        spec: SkillSpec {
            id: "db-management".into(),
            name: "Database Management".into(),
            description: "Built-in description".into(),
            instructions_md: "Built-in instructions.".into(),
            when_to_use: Some("built-in hint".into()),
            model_override: Some("built-in-model".into()),
            ..Default::default()
        },
        meta: RecordMeta::new_builtin("test"),
    };
    record.meta.user_overrides = Some(json!({
        "description": "Patched description",
        "instructions_md": "Patched instructions.",
        "when_to_use": null,
        "model_invocable": false,
        "model_override": null
    }));
    config_store
        .put(
            ConfigNamespace::Skills.as_str(),
            "db-management",
            &record.to_value().expect("serialize skill record"),
        )
        .await
        .expect("write skill record");

    let service = ConfigService::new(&state).expect("config service");
    let value = service
        .get(ConfigNamespace::Skills, "db-management")
        .await
        .expect("get skill")
        .expect("skill exists");

    assert_eq!(value["description"], "Patched description");
    assert_eq!(value["instructions_md"], "Patched instructions.");
    assert!(value.get("when_to_use").is_none() || value["when_to_use"].is_null());
    assert_eq!(value["model_invocable"], false);
    assert!(value.get("model_override").is_none() || value["model_override"].is_null());
}

// ── audit integration tests ────────────────────────────────────────────

mod audit_integration {
    use std::sync::Arc;

    use awaken_server_contract::AuditAction;
    use axum::http::HeaderMap;
    use serde_json::json;

    use crate::services::audit_log::{AUDIT_NAMESPACE, AuditLogger, AuditQuery};
    use crate::services::config_service::{ConfigNamespace, ConfigService};

    use super::build_state;

    #[tokio::test]
    async fn create_emits_audit_create_event() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let audit_logger = Arc::new(AuditLogger::new(config_store.clone()));
        let mut state = state;
        state.config.as_mut().expect("config module").audit_log = Some(audit_logger.clone());

        let service = ConfigService::new(&state).expect("service");
        service
            .create_with_headers(
                ConfigNamespace::Providers,
                json!({ "id": "audit-prov", "adapter": "stub" }),
                &HeaderMap::new(),
            )
            .await
            .expect("create");

        let page = audit_logger.query(AuditQuery::default()).await.unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].action, AuditAction::Create);
        assert_eq!(page.items[0].resource, "providers/audit-prov");
        assert!(page.items[0].before.is_none());
        assert!(page.items[0].after.is_some());
    }

    #[tokio::test]
    async fn update_emits_audit_update_event_with_before_after() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let audit_logger = Arc::new(AuditLogger::new(config_store.clone()));
        let mut state = state;
        state.config.as_mut().expect("config module").audit_log = Some(audit_logger.clone());

        let service = ConfigService::new(&state).expect("service");
        service
                .create_with_headers(
                    ConfigNamespace::Agents,
                    json!({ "id": "upd-agent", "model_id": "bootstrap", "system_prompt": "v1", "max_rounds": 1 }),
                    &HeaderMap::new(),
                )
                .await
                .expect("create");

        service
                .update_with_headers(
                    ConfigNamespace::Agents,
                    "upd-agent",
                    json!({ "id": "upd-agent", "model_id": "bootstrap", "system_prompt": "v2", "max_rounds": 1 }),
                    &HeaderMap::new(),
                )
                .await
                .expect("update");

        let page = audit_logger
            .query(AuditQuery {
                action: Some(AuditAction::Update),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].action, AuditAction::Update);
        assert!(page.items[0].before.is_some(), "before must be set");
        assert!(page.items[0].after.is_some(), "after must be set");
    }

    #[tokio::test]
    async fn delete_emits_audit_delete_event_with_before() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let audit_logger = Arc::new(AuditLogger::new(config_store.clone()));
        let mut state = state;
        state.config.as_mut().expect("config module").audit_log = Some(audit_logger.clone());

        let service = ConfigService::new(&state).expect("service");
        service
                .create_with_headers(
                    ConfigNamespace::Agents,
                    json!({ "id": "del-agent", "model_id": "bootstrap", "system_prompt": "hi", "max_rounds": 1 }),
                    &HeaderMap::new(),
                )
                .await
                .expect("create");

        service
            .delete_with_options(
                ConfigNamespace::Agents,
                "del-agent",
                false,
                &HeaderMap::new(),
            )
            .await
            .expect("delete");

        // Only the Delete event should be in audit (create is there too but filter by Delete).
        let page = audit_logger
            .query(AuditQuery {
                action: Some(AuditAction::Delete),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].action, AuditAction::Delete);
        assert!(
            page.items[0].before.is_some(),
            "before must contain deleted payload"
        );
        assert!(
            page.items[0].after.is_none(),
            "after must be None for delete"
        );
    }

    #[tokio::test]
    async fn provider_force_delete_emits_audit_for_cascaded_model_delete() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let audit_logger = Arc::new(AuditLogger::new(config_store.clone()));
        let mut state = state;
        state.config.as_mut().expect("config module").audit_log = Some(audit_logger.clone());

        let service = ConfigService::new(&state).expect("service");
        service
            .create_with_headers(
                ConfigNamespace::Providers,
                json!({ "id": "audit-cascade-prov", "adapter": "stub" }),
                &HeaderMap::new(),
            )
            .await
            .expect("create provider");
        service
            .create_with_headers(
                ConfigNamespace::Models,
                json!({
                    "id": "audit-cascade-model",
                    "provider_id": "audit-cascade-prov",
                    "upstream_model": "gpt-4"
                }),
                &HeaderMap::new(),
            )
            .await
            .expect("create model");

        service
            .delete_with_options(
                ConfigNamespace::Providers,
                "audit-cascade-prov",
                true,
                &HeaderMap::new(),
            )
            .await
            .expect("force delete provider");

        let page = audit_logger
            .query(AuditQuery {
                action: Some(AuditAction::Delete),
                ..Default::default()
            })
            .await
            .unwrap();
        let mut resources = page
            .items
            .iter()
            .map(|event| event.resource.as_str())
            .collect::<Vec<_>>();
        resources.sort_unstable();
        assert_eq!(
            resources,
            vec!["models/audit-cascade-model", "providers/audit-cascade-prov"]
        );
        for event in page.items {
            assert!(
                event.before.is_some(),
                "delete audit for {} must include before payload",
                event.resource
            );
            assert!(
                event.after.is_none(),
                "delete audit for {} must omit after payload",
                event.resource
            );
        }
    }

    #[tokio::test]
    async fn config_write_succeeds_even_when_audit_store_separate_and_no_logger() {
        // Verify that without an audit logger, create still succeeds.
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        // No audit_log attached.

        let service = ConfigService::new(&state).expect("service");
        service
                .create_with_headers(
                    ConfigNamespace::Agents,
                    json!({ "id": "no-audit-agent", "model_id": "bootstrap", "system_prompt": "hi", "max_rounds": 1 }),
                    &HeaderMap::new(),
                )
                .await
                .expect("create without audit should succeed");

        // Confirm no audit entries exist.
        let audit_entries = awaken_server_contract::contract::config_store::ConfigStore::list(
            config_store.as_ref(),
            AUDIT_NAMESPACE,
            0,
            usize::MAX,
        )
        .await
        .unwrap();
        assert!(audit_entries.is_empty());
    }
}

// ── ConfigRecord envelope tests ─────────────────────────────────────────

#[tokio::test]
async fn put_emits_envelope_with_user_meta() {
    let config_store = Arc::new(awaken_stores::InMemoryStore::new());
    let (state, _manager) = build_state(config_store.clone()).await;
    let service = ConfigService::new(&state).expect("service");

    service
        .create_with_headers(
            ConfigNamespace::Agents,
            json!({
                "id": "env-agent",
                "model_id": "bootstrap",
                "system_prompt": "test",
                "max_rounds": 1
            }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("create agent");

    let raw = awaken_server_contract::contract::config_store::ConfigStore::get(
        config_store.as_ref(),
        "agents",
        "env-agent",
    )
    .await
    .expect("store read")
    .expect("entry present");

    let obj = raw.as_object().expect("must be JSON object");
    assert!(
        obj.contains_key("spec"),
        "stored value must have 'spec' key"
    );
    assert!(
        obj.contains_key("meta"),
        "stored value must have 'meta' key"
    );

    let meta = &raw["meta"];
    assert_eq!(
        meta["source"]["kind"].as_str(),
        Some("user"),
        "source.kind must be 'user'"
    );
    assert_ne!(
        meta["created_at"].as_u64(),
        Some(0),
        "created_at must be non-zero"
    );
}

#[tokio::test]
async fn put_existing_envelope_preserves_created_at() {
    let config_store = Arc::new(awaken_stores::InMemoryStore::new());
    let (state, _manager) = build_state(config_store.clone()).await;
    let service = ConfigService::new(&state).expect("service");

    service
        .create_with_headers(
            ConfigNamespace::Agents,
            json!({
                "id": "ts-agent",
                "model_id": "bootstrap",
                "system_prompt": "v1",
                "max_rounds": 1
            }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("create agent");

    // Read back created_at from envelope
    let first = awaken_server_contract::contract::config_store::ConfigStore::get(
        config_store.as_ref(),
        "agents",
        "ts-agent",
    )
    .await
    .expect("read")
    .expect("present");
    let created_at_1 = first["meta"]["created_at"].as_u64().expect("created_at");

    // Sleep briefly so updated_at will differ
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    service
        .update_with_headers(
            ConfigNamespace::Agents,
            "ts-agent",
            json!({
                "id": "ts-agent",
                "model_id": "bootstrap",
                "system_prompt": "v2",
                "max_rounds": 1
            }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("update agent");

    let second = awaken_server_contract::contract::config_store::ConfigStore::get(
        config_store.as_ref(),
        "agents",
        "ts-agent",
    )
    .await
    .expect("read")
    .expect("present");

    let created_at_2 = second["meta"]["created_at"]
        .as_u64()
        .expect("created_at after update");
    let updated_at_2 = second["meta"]["updated_at"]
        .as_u64()
        .expect("updated_at after update");

    assert_eq!(
        created_at_1, created_at_2,
        "created_at must be preserved across updates"
    );
    assert!(
        updated_at_2 >= created_at_2,
        "updated_at must be >= created_at after update"
    );
}

#[tokio::test]
async fn audit_payload_is_bare_spec_not_envelope() {
    use crate::services::audit_log::{AuditLogger, AuditQuery};

    let config_store = Arc::new(awaken_stores::InMemoryStore::new());
    let (state, _manager) = build_state(config_store.clone()).await;
    let audit_logger = Arc::new(AuditLogger::new(config_store.clone()));
    let mut state = state;
    state.config.as_mut().expect("config module").audit_log = Some(audit_logger.clone());

    let service = ConfigService::new(&state).expect("service");
    service
        .create_with_headers(
            ConfigNamespace::Agents,
            json!({
                "id": "audit-env-agent",
                "model_id": "bootstrap",
                "system_prompt": "audit test",
                "max_rounds": 1
            }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("create");

    let page = audit_logger
        .query(AuditQuery::default())
        .await
        .expect("query");
    assert_eq!(page.items.len(), 1);

    let after = page.items[0].after.as_ref().expect("after must be present");
    let after_obj = after.as_object().expect("after must be JSON object");
    assert!(
        !after_obj.contains_key("meta"),
        "audit 'after' must not contain 'meta' key (must be bare spec)"
    );
    assert!(
        !after_obj.contains_key("spec"),
        "audit 'after' must not contain 'spec' wrapper key (must be bare spec)"
    );
    assert!(
        after_obj.contains_key("id"),
        "audit 'after' must contain spec field 'id'"
    );
}

#[test]
fn config_namespace_rejects_tools_to_keep_public_enum_compatible() {
    assert!(ConfigNamespace::parse("tools").is_err());
}

#[test]
fn config_namespace_all_excludes_tools_to_keep_public_enum_compatible() {
    assert_eq!(ConfigNamespace::ALL.len(), 6);
    assert!(
        !ConfigNamespace::ALL
            .iter()
            .any(|namespace| namespace.as_str() == "tools")
    );
}

#[test]
fn config_namespace_schema_for_tools_is_object() {
    let schema = tool_schema_json().expect("schema");
    // schemars 0.8: top-level object schema shape
    assert!(schema.get("$defs").is_some() || schema.get("type").is_some());
}

// ── patch_tool_overrides helpers ──────────────────────────────────────────

use crate::services::audit_log::{AuditLogger, AuditQuery};
use awaken_server_contract::ToolSpec;
use awaken_server_contract::contract::audit_log::AuditEvent;
use awaken_server_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};

struct StubTool {
    id: String,
    desc: String,
}

#[async_trait]
impl Tool for StubTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(self.id.clone(), self.id.clone(), self.desc.clone())
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolCallContext,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolResult::success(&self.id, serde_json::json!({})).into())
    }
}

async fn build_test_service_with_tool(
    id: &str,
    description: &str,
) -> (ConfigService, Arc<AuditLogger>) {
    use awaken_server_contract::{BuiltinSeedSet, BuiltinSpec, RecordMeta};

    let config_store: Arc<dyn awaken_server_contract::contract::config_store::ConfigStore> =
        Arc::new(awaken_stores::InMemoryStore::new());
    let audit_store: Arc<dyn awaken_server_contract::contract::config_store::ConfigStore> =
        Arc::new(awaken_stores::InMemoryStore::new());
    let audit_logger = Arc::new(AuditLogger::new(audit_store));

    let thread_store = Arc::new(awaken_stores::InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(ImmediateExecutor))
            .with_in_memory_thread_run_store(thread_store.clone())
            .with_tool(
                id,
                Arc::new(StubTool {
                    id: id.to_string(),
                    desc: description.to_string(),
                }),
            )
            .build()
            .expect("build runtime"),
    );

    let manager = Arc::new(
        crate::services::config_runtime::ConfigRuntimeManager::new(
            runtime.clone(),
            config_store.clone(),
        )
        .expect("config runtime manager")
        .with_provider_factory(Arc::new(TestProviderFactory)),
    );
    let resolver = runtime.resolver_arc();
    let seed = BuiltinSeedSet {
        binary_version: "test".to_string(),
        specs: vec![
            BuiltinSpec::provider(ProviderSpec {
                id: "bootstrap".into(),
                adapter: "stub".into(),
                ..Default::default()
            }),
            BuiltinSpec::model(awaken_server_contract::ModelSpec::new(
                "bootstrap",
                "bootstrap",
                "bootstrap-model",
            )),
            BuiltinSpec::agent(bootstrap_agent()),
        ],
    };
    manager.apply_seed(&seed).await.expect("apply_seed");
    manager.apply().await.expect("publish config");

    // Write a Builtin ConfigRecord for the tool directly into the store.
    let tool_spec = ToolSpec {
        id: id.to_string(),
        name: id.to_string(),
        description: description.to_string(),
        ..Default::default()
    };
    let mut meta = RecordMeta::new_builtin("test");
    meta.user_overrides = None;
    meta.revision = 1;
    let record = awaken_server_contract::ConfigRecord {
        spec: tool_spec,
        meta,
    };
    let envelope = record.to_value().expect("serialize tool record");
    awaken_server_contract::contract::config_store::ConfigStore::put_if_absent(
        config_store.as_ref(),
        "tools",
        id,
        &envelope,
    )
    .await
    .expect("put tool record");

    let mailbox = Arc::new(crate::mailbox::Mailbox::new(
        runtime.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        thread_store.clone(),
        "tool-override-test".into(),
        crate::mailbox::MailboxConfig::default(),
    ));
    let mut state = ServerState::new(
        runtime,
        mailbox,
        thread_store,
        resolver,
        crate::app::ServerConfig::default(),
    );
    state.config = Some(
        crate::app::ConfigModuleState::new(config_store, manager)
            .with_audit_log(audit_logger.clone()),
    );

    // SAFETY: state is owned for the duration of the test; the 'static bound is
    // satisfied by leaking the Box – acceptable in tests only.
    let state: &'static ServerState = Box::leak(Box::new(state));
    let service = ConfigService::new(state).expect("config service");
    (service, audit_logger)
}

async fn recent_audit_events(audit_logger: &AuditLogger, resource: &str) -> Vec<AuditEvent> {
    let page = audit_logger
        .query(AuditQuery::default())
        .await
        .expect("audit query");
    page.items
        .into_iter()
        .filter(|e| e.resource == resource || e.resource.starts_with(&format!("{resource}/")))
        .collect()
}

// ── patch_tool_overrides tests ────────────────────────────────────────────

#[tokio::test]
async fn patch_tool_overrides_replaces_description_and_emits_audit() {
    let (service, audit_logger) = build_test_service_with_tool("echo", "stock description").await;
    let patch = serde_json::json!({"description": "custom override"});
    let after = service
        .patch_tool_overrides("echo", patch, &axum::http::HeaderMap::new())
        .await
        .expect("patch ok");
    assert_eq!(after["description"], "custom override");
    assert_eq!(after["id"], "echo");
    let events: Vec<AuditEvent> = recent_audit_events(&audit_logger, "tools/echo").await;
    let event = events
        .iter()
        .find(|e| e.action == awaken_server_contract::AuditAction::Update)
        .expect("audit event missing");
    assert_eq!(event.resource, "tools/echo/overrides");
    let before = event.before.as_ref().expect("before payload missing");
    let after_payload = event.after.as_ref().expect("after payload missing");
    assert_eq!(before["description"], "stock description");
    assert_eq!(after_payload["description"], "custom override");
}

#[tokio::test]
async fn get_tools_merges_overrides_into_effective_spec() {
    // Regression for the bug where `effective_spec` only merged for
    // Agents, leaving Tools' GET endpoint returning the unpatched
    // description even though the override was persisted in meta.
    let (service, _audit_logger) = build_test_service_with_tool("echo", "stock description").await;
    service
        .patch_tool_overrides(
            "echo",
            serde_json::json!({"description": "patched"}),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("patch ok");
    let value = service
        .get_tool("echo")
        .await
        .expect("get ok")
        .expect("present");
    assert_eq!(value["description"], "patched");
}

#[tokio::test]
async fn patch_tool_overrides_404_for_unknown_id() {
    let (service, _audit_logger) = build_test_service_with_tool("echo", "x").await;
    let err = service
        .patch_tool_overrides(
            "nope",
            serde_json::json!({"description": "x"}),
            &Default::default(),
        )
        .await
        .expect_err("unknown id");
    assert!(matches!(err, ConfigServiceError::NotFound(_)));
}

#[tokio::test]
async fn patch_tool_overrides_422_for_unknown_field() {
    let (service, _audit_logger) = build_test_service_with_tool("echo", "x").await;
    let err = service
        .patch_tool_overrides(
            "echo",
            serde_json::json!({"name": "renamed"}),
            &Default::default(),
        )
        .await
        .expect_err("unknown field");
    assert!(matches!(err, ConfigServiceError::InvalidPayload(_)));
}

#[tokio::test]
async fn patch_tool_overrides_rejects_empty_description() {
    let (service, _audit_logger) = build_test_service_with_tool("echo", "x").await;
    let err = service
        .patch_tool_overrides(
            "echo",
            serde_json::json!({"description": ""}),
            &Default::default(),
        )
        .await
        .expect_err("empty description");
    assert!(matches!(err, ConfigServiceError::InvalidPayload(_)));
}

#[tokio::test]
async fn patch_tool_overrides_rejects_overlong_description() {
    let (service, _audit_logger) = build_test_service_with_tool("echo", "x").await;
    let too_long = "x".repeat(4097);
    let err = service
        .patch_tool_overrides(
            "echo",
            serde_json::json!({"description": too_long}),
            &Default::default(),
        )
        .await
        .expect_err("overlong");
    assert!(matches!(err, ConfigServiceError::InvalidPayload(_)));
}

#[tokio::test]
async fn clear_tool_overrides_reverts_to_builtin() {
    let (service, _audit_logger) = build_test_service_with_tool("echo", "stock").await;
    service
        .patch_tool_overrides(
            "echo",
            serde_json::json!({"description": "custom"}),
            &Default::default(),
        )
        .await
        .unwrap();
    let after = service
        .clear_tool_overrides("echo", &Default::default())
        .await
        .unwrap();
    assert_eq!(after["description"], "stock");
}

#[tokio::test]
async fn clear_tool_overrides_idempotent_when_already_empty() {
    let (service, _audit_logger) = build_test_service_with_tool("echo", "stock").await;
    let after = service
        .clear_tool_overrides("echo", &Default::default())
        .await
        .unwrap();
    assert_eq!(after["description"], "stock");
}

#[tokio::test]
async fn clear_tool_override_field_unknown_returns_422() {
    let (service, _audit_logger) = build_test_service_with_tool("echo", "stock").await;
    let err = service
        .clear_tool_override_field("echo", "garbage", &Default::default())
        .await
        .expect_err("unknown field");
    assert!(matches!(err, ConfigServiceError::InvalidPayload(_)));
}

#[tokio::test]
async fn clear_tool_override_field_known_clears_only_that_field() {
    let (service, _audit_logger) = build_test_service_with_tool("echo", "stock").await;
    service
        .patch_tool_overrides(
            "echo",
            serde_json::json!({"description": "custom"}),
            &Default::default(),
        )
        .await
        .unwrap();
    let after = service
        .clear_tool_override_field("echo", "description", &Default::default())
        .await
        .unwrap();
    assert_eq!(after["description"], "stock");
}

// ── CAS / revision tests ──────────────────────────────────────────────────

#[tokio::test]
async fn patch_tool_overrides_bumps_revision() {
    let (service, _audit) = build_test_service_with_tool("echo", "stock").await;

    let meta_before = service
        .get_tool_meta("echo")
        .await
        .expect("get_meta")
        .expect("present");
    assert_eq!(
        meta_before.revision, 1,
        "fresh seed must start at revision 1"
    );

    service
        .patch_tool_overrides(
            "echo",
            serde_json::json!({"description": "patched"}),
            &Default::default(),
        )
        .await
        .expect("first patch ok");

    let meta_after = service
        .get_tool_meta("echo")
        .await
        .expect("get_meta")
        .expect("present");
    assert!(
        meta_after.revision > meta_before.revision,
        "patch must bump revision: before={}, after={}",
        meta_before.revision,
        meta_after.revision,
    );
}

#[tokio::test]
async fn patch_tool_overrides_conflict_on_stale_revision() {
    use awaken_server_contract::ConfigRecord;

    let (service, _audit) = build_test_service_with_tool("echo", "stock").await;

    let store = service.store.clone();
    let raw = awaken_server_contract::contract::config_store::ConfigStore::get(
        store.as_ref(),
        "tools",
        "echo",
    )
    .await
    .expect("read")
    .expect("present");

    let mut stale_record =
        ConfigRecord::<awaken_server_contract::ToolSpec>::from_value(raw.clone())
            .expect("parse record");
    let stale_expected = stale_record.meta.revision;

    let mut concurrent_record =
        ConfigRecord::<awaken_server_contract::ToolSpec>::from_value(raw).expect("parse current");
    concurrent_record.spec.description = "concurrent".into();
    concurrent_record.meta.revision = stale_expected + 1;
    let concurrent_envelope = concurrent_record.to_value().expect("serialize concurrent");
    awaken_server_contract::contract::config_store::ConfigStore::put_if_revision(
        store.as_ref(),
        "tools",
        "echo",
        &concurrent_envelope,
        stale_expected,
    )
    .await
    .expect("concurrent writer succeeds");

    stale_record.spec.description = "stale".into();
    let err = service
        .cas_put_record_in_namespace(TOOLS_NAMESPACE, "echo", &mut stale_record, stale_expected)
        .await
        .expect_err("stale write must conflict");
    assert!(matches!(err, ConfigServiceError::Conflict(_)));

    let meta_final = service
        .get_tool_meta("echo")
        .await
        .expect("get_meta final")
        .expect("present final");
    assert_eq!(
        meta_final.revision,
        stale_expected + 1,
        "stale writer must not advance the stored revision"
    );
}

// ── ApplyFailed audit emission tests ─────────────────────────────────────

#[tokio::test]
async fn patch_tool_overrides_apply_failure_emits_apply_failed_audit_event() {
    use awaken_server_contract::{BuiltinSeedSet, BuiltinSpec, RecordMeta};

    // Step 1: seed a config store with a builtin tool, apply successfully.
    let config_store: Arc<dyn awaken_server_contract::contract::config_store::ConfigStore> =
        Arc::new(awaken_stores::InMemoryStore::new());
    let audit_store: Arc<dyn awaken_server_contract::contract::config_store::ConfigStore> =
        Arc::new(awaken_stores::InMemoryStore::new());
    let audit_logger = Arc::new(AuditLogger::new(audit_store));

    let thread_store = Arc::new(awaken_stores::InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(ImmediateExecutor))
            .with_in_memory_thread_run_store(thread_store.clone())
            .with_tool(
                "echo",
                Arc::new(StubTool {
                    id: "echo".to_string(),
                    desc: "stock".to_string(),
                }),
            )
            .build()
            .expect("build runtime"),
    );

    let manager_ok = Arc::new(
        crate::services::config_runtime::ConfigRuntimeManager::new(
            runtime.clone(),
            config_store.clone(),
        )
        .expect("config runtime manager")
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
            BuiltinSpec::model(awaken_server_contract::ModelSpec::new(
                "bootstrap",
                "bootstrap",
                "bootstrap-model",
            )),
            BuiltinSpec::agent(bootstrap_agent()),
        ],
    };
    manager_ok.apply_seed(&seed).await.expect("apply_seed");
    manager_ok.apply().await.expect("initial apply");

    // Write a builtin tool record directly.
    let tool_spec = ToolSpec {
        id: "echo".to_string(),
        name: "echo".to_string(),
        description: "stock".to_string(),
        ..Default::default()
    };
    let mut meta = RecordMeta::new_builtin("test");
    meta.user_overrides = None;
    meta.revision = 1;
    let record = awaken_server_contract::ConfigRecord {
        spec: tool_spec,
        meta,
    };
    let envelope = record.to_value().expect("serialize tool record");
    awaken_server_contract::contract::config_store::ConfigStore::put_if_absent(
        config_store.as_ref(),
        "tools",
        "echo",
        &envelope,
    )
    .await
    .expect("put tool record");

    // Step 2: build a second manager with FailingProviderFactory over the same store.
    let runtime_failing = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(ImmediateExecutor))
            .with_in_memory_thread_run_store(thread_store.clone())
            .build()
            .expect("build failing runtime"),
    );
    let manager_failing = Arc::new(
        crate::services::config_runtime::ConfigRuntimeManager::new(
            runtime_failing.clone(),
            config_store.clone(),
        )
        .expect("config runtime manager")
        .with_provider_factory(Arc::new(FailingProviderFactory)),
    );
    let mailbox = Arc::new(crate::mailbox::Mailbox::new(
        runtime_failing.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        thread_store.clone(),
        "apply-failed-test".into(),
        crate::mailbox::MailboxConfig::default(),
    ));
    let mut state = ServerState::new(
        runtime_failing.clone(),
        mailbox,
        thread_store,
        runtime_failing.resolver_arc(),
        crate::app::ServerConfig::default(),
    );
    state.config = Some(
        crate::app::ConfigModuleState::new(config_store.clone(), manager_failing)
            .with_audit_log(audit_logger.clone()),
    );

    let state: &'static ServerState = Box::leak(Box::new(state));
    let service = ConfigService::new(state).expect("failing config service");

    // Step 3: attempt patch_tool_overrides — apply_locked fails.
    let result = service
        .patch_tool_overrides(
            "echo",
            serde_json::json!({"description": "patched"}),
            &axum::http::HeaderMap::new(),
        )
        .await;
    assert!(result.is_err(), "patch must fail when apply_locked fails");

    // Step 4: assert ApplyFailed event was emitted with the correct fields.
    let page = audit_logger
        .query(crate::services::audit_log::AuditQuery::default())
        .await
        .expect("audit query");
    let failed_events: Vec<_> = page
        .items
        .iter()
        .filter(|e| e.action == awaken_server_contract::AuditAction::ApplyFailed)
        .collect();
    assert_eq!(
        failed_events.len(),
        1,
        "exactly one ApplyFailed event must be emitted"
    );
    let ev = &failed_events[0];
    assert!(
        ev.resource.contains("tools/echo"),
        "resource must reference tools/echo, got: {}",
        ev.resource
    );
    assert!(
        ev.error.is_some(),
        "ApplyFailed event must carry an error string"
    );
    assert!(
        ev.before.is_some(),
        "ApplyFailed event must carry the before spec"
    );
}

// ── catalog validation on agent PUT (AgentSpec::validate_catalog wiring) ──

#[tokio::test]
async fn create_agent_rejects_invalid_tool_pattern() {
    let config_store = Arc::new(awaken_stores::InMemoryStore::new());
    let (state, _manager) = build_state(config_store.clone()).await;
    let service = ConfigService::new(&state).expect("config service");

    // Dangling `\` is an invalid pattern -> validate_catalog returns an
    // Error issue -> PUT must reject with InvalidPayload.
    let err = service
        .create_with_headers(
            ConfigNamespace::Agents,
            json!({
                "id": "bad-pattern-agent",
                "model_id": "bootstrap",
                "system_prompt": "test",
                "allowed_tool_patterns": ["foo\\"],
            }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect_err("invalid pattern must be rejected");

    let ConfigServiceError::InvalidPayload(msg) = &err else {
        panic!("expected InvalidPayload, got {err:?}");
    };
    assert!(
        msg.contains("allowed_tool_patterns"),
        "error message must name the offending field: {msg}"
    );
    assert!(
        msg.contains("bad-pattern-agent"),
        "error message must name the agent id: {msg}"
    );

    // Spec must not have been persisted.
    let stored = ConfigStore::get(config_store.as_ref(), "agents", "bad-pattern-agent")
        .await
        .expect("read");
    assert!(stored.is_none(), "rejected spec must not reach the store");
}

#[tokio::test]
async fn create_agent_accepts_star_in_literal_field_as_warning() {
    let config_store = Arc::new(awaken_stores::InMemoryStore::new());
    let (state, _manager) = build_state(config_store.clone()).await;
    let service = ConfigService::new(&state).expect("config service");

    // `mcp:*` in a literal field is a Warning, not an Error: spec loads.
    let result = service
        .create_with_headers(
            ConfigNamespace::Agents,
            json!({
                "id": "warn-literal-agent",
                "model_id": "bootstrap",
                "system_prompt": "test",
                "allowed_tools": ["mcp:*"],
            }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect("warning-only catalog must succeed");
    assert_eq!(result["id"], "warn-literal-agent");

    let stored = ConfigStore::get(config_store.as_ref(), "agents", "warn-literal-agent")
        .await
        .expect("read")
        .expect("spec must be persisted despite warning");
    let spec_value = stored.get("spec").unwrap_or(&stored);
    assert_eq!(spec_value["id"], "warn-literal-agent");
}

#[tokio::test]
async fn patch_agent_overrides_rejects_invalid_tool_pattern() {
    let config_store = Arc::new(awaken_stores::InMemoryStore::new());
    let (state, _manager) = build_state(config_store.clone()).await;
    let service = ConfigService::new(&state).expect("config service");

    // bootstrap_agent is registered as Builtin via apply_seed, so it accepts overrides.
    let err = service
        .patch_agent_overrides(
            "bootstrap",
            json!({ "allowed_tool_patterns": ["foo\\"] }),
            &axum::http::HeaderMap::new(),
        )
        .await
        .expect_err("invalid pattern in overrides must be rejected");
    assert!(
        matches!(err, ConfigServiceError::InvalidPayload(ref msg) if msg.contains("allowed_tool_patterns")),
        "expected InvalidPayload naming the field, got {err:?}"
    );
}

#[tokio::test]
async fn validate_agent_overrides_surfaces_invalid_tool_pattern() {
    let config_store = Arc::new(awaken_stores::InMemoryStore::new());
    let (state, _manager) = build_state(config_store.clone()).await;
    let service = ConfigService::new(&state).expect("config service");

    let err = service
        .validate_agent_overrides("bootstrap", json!({ "excluded_tool_patterns": [""] }))
        .await
        .expect_err("dry-run validation must reject invalid pattern");
    assert!(
        matches!(err, ConfigServiceError::InvalidPayload(ref msg) if msg.contains("excluded_tool_patterns")),
        "expected InvalidPayload naming the field, got {err:?}"
    );
}
