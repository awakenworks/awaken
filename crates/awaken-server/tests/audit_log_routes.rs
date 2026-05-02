//! Integration tests for `GET /v1/audit-log`.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::{AgentSpec, ModelBindingSpec, ProviderSpec};
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::registry::traits::ModelBinding;
use awaken_server::app::{AppState, ServerConfig};
use awaken_server::mailbox::{Mailbox, MailboxConfig};
use awaken_server::routes::build_router;
use awaken_server::services::audit_log::AuditLogger;
use awaken_server::services::config_runtime::{
    ConfigRuntimeError, ConfigRuntimeManager, ProviderExecutorFactory,
};
use awaken_stores::InMemoryStore;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
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

fn bootstrap_agent() -> AgentSpec {
    AgentSpec {
        id: "bootstrap".into(),
        model_id: "bootstrap".into(),
        system_prompt: "bootstrap".into(),
        max_rounds: 1,
        ..Default::default()
    }
}

async fn build_test_app_with_audit(token: Option<&str>) -> axum::Router {
    let config_store = Arc::new(InMemoryStore::new());
    let thread_store = Arc::new(InMemoryStore::new());
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
            .with_agent_spec(bootstrap_agent())
            .with_thread_run_store(thread_store.clone())
            .build()
            .expect("build runtime"),
    );

    let manager = Arc::new(
        ConfigRuntimeManager::new(runtime.clone(), config_store.clone())
            .expect("config runtime manager")
            .with_provider_factory(Arc::new(TestProviderFactory)),
    );
    manager
        .bootstrap_if_empty(
            &[ProviderSpec {
                id: "bootstrap".into(),
                adapter: "stub".into(),
                ..Default::default()
            }],
            &[ModelBindingSpec {
                id: "bootstrap".into(),
                provider_id: "bootstrap".into(),
                upstream_model: "bootstrap-model".into(),
                created_at: None,
                updated_at: None,
            }],
            &[bootstrap_agent()],
            &[],
        )
        .await
        .expect("bootstrap");
    manager.apply().await.expect("apply");

    let audit_logger = Arc::new(AuditLogger::new(config_store.clone()));
    let resolver = runtime.resolver_arc();
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        thread_store.clone(),
        "audit-test".into(),
        MailboxConfig::default(),
    ));
    let mut state = AppState::new(
        runtime,
        mailbox,
        thread_store,
        resolver,
        ServerConfig::default(),
    )
    .with_config_store(config_store)
    .with_config_runtime_manager(manager)
    .with_audit_log(audit_logger);

    if let Some(tok) = token {
        state = state.with_admin_api_bearer_token(tok);
    }

    build_router(&state).with_state(state)
}

async fn build_test_app_without_audit() -> axum::Router {
    let config_store = Arc::new(InMemoryStore::new());
    let thread_store = Arc::new(InMemoryStore::new());
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
            .with_agent_spec(bootstrap_agent())
            .with_thread_run_store(thread_store.clone())
            .build()
            .expect("build runtime"),
    );
    let manager = Arc::new(
        ConfigRuntimeManager::new(runtime.clone(), config_store.clone())
            .expect("manager")
            .with_provider_factory(Arc::new(TestProviderFactory)),
    );
    manager
        .bootstrap_if_empty(
            &[ProviderSpec {
                id: "bootstrap".into(),
                adapter: "stub".into(),
                ..Default::default()
            }],
            &[ModelBindingSpec {
                id: "bootstrap".into(),
                provider_id: "bootstrap".into(),
                upstream_model: "bootstrap-model".into(),
                created_at: None,
                updated_at: None,
            }],
            &[bootstrap_agent()],
            &[],
        )
        .await
        .expect("bootstrap");
    manager.apply().await.expect("apply");

    let resolver = runtime.resolver_arc();
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        thread_store.clone(),
        "audit-test-no-log".into(),
        MailboxConfig::default(),
    ));
    let state = AppState::new(
        runtime,
        mailbox,
        thread_store,
        resolver,
        ServerConfig::default(),
    )
    .with_config_store(config_store)
    .with_config_runtime_manager(manager);
    // No audit_log attached.
    build_router(&state).with_state(state)
}

async fn get_audit_log(app: &axum::Router, qs: &str) -> (StatusCode, Value) {
    get_audit_log_with_token(app, qs, None).await
}

async fn get_audit_log_with_token(
    app: &axum::Router,
    qs: &str,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let uri = if qs.is_empty() {
        "/v1/audit-log".to_string()
    } else {
        format!("/v1/audit-log?{qs}")
    };
    let mut builder = Request::builder().method("GET").uri(&uri);
    if let Some(tok) = token {
        builder = builder.header("authorization", format!("Bearer {tok}"));
    }
    let req = builder.body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn create_config(app: &axum::Router, namespace: &str, body: &Value) -> StatusCode {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/v1/config/{namespace}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

// ── tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_returns_events_after_create() {
    let app = build_test_app_with_audit(None).await;

    let status = create_config(
        &app,
        "agents",
        &json!({ "id": "audit-agent-1", "model_id": "bootstrap", "system_prompt": "hi", "max_rounds": 1 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = get_audit_log(&app, "").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let items = body["items"].as_array().expect("items array");
    assert!(!items.is_empty(), "should have at least one audit event");
    assert!(
        items
            .iter()
            .any(|e| e["resource"].as_str() == Some("agents/audit-agent-1")),
        "event for created agent must be present"
    );
}

#[tokio::test]
async fn filter_by_resource_returns_only_matching_events() {
    let app = build_test_app_with_audit(None).await;

    create_config(
        &app,
        "agents",
        &json!({ "id": "res-a", "model_id": "bootstrap", "system_prompt": "a", "max_rounds": 1 }),
    )
    .await;
    create_config(
        &app,
        "agents",
        &json!({ "id": "res-b", "model_id": "bootstrap", "system_prompt": "b", "max_rounds": 1 }),
    )
    .await;

    let (status, body) = get_audit_log(&app, "resource=agents/res-a").await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["resource"], "agents/res-a");
}

#[tokio::test]
async fn cursor_pagination_across_many_entries() {
    let app = build_test_app_with_audit(None).await;

    // Create 10 agents.
    for i in 0..10usize {
        create_config(
            &app,
            "agents",
            &json!({ "id": format!("pg-agent-{i:02}"), "model_id": "bootstrap", "system_prompt": "x", "max_rounds": 1 }),
        )
        .await;
        // Small sleep to ensure distinct ULIDs.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    // Page 1: limit 4.
    let (status, body1) = get_audit_log(&app, "limit=4").await;
    assert_eq!(status, StatusCode::OK);
    let items1 = body1["items"].as_array().unwrap();
    assert_eq!(items1.len(), 4);
    let cursor = body1["next_cursor"].as_str().expect("next_cursor present");

    // Page 2: continue.
    let (status, body2) = get_audit_log(&app, &format!("limit=4&cursor={cursor}")).await;
    assert_eq!(status, StatusCode::OK);
    let items2 = body2["items"].as_array().unwrap();
    assert!(!items2.is_empty(), "page 2 must have items");

    // No id overlap between pages.
    let ids1: Vec<&str> = items1.iter().filter_map(|e| e["id"].as_str()).collect();
    let ids2: Vec<&str> = items2.iter().filter_map(|e| e["id"].as_str()).collect();
    for id in &ids1 {
        assert!(!ids2.contains(id), "pages must not overlap");
    }
}

#[tokio::test]
async fn unauthorized_without_token_returns_401() {
    let app = build_test_app_with_audit(Some("secret-token")).await;
    let (status, _body) = get_audit_log(&app, "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn authorized_with_correct_token_returns_200() {
    let app = build_test_app_with_audit(Some("secret-token")).await;
    let (status, body) = get_audit_log_with_token(&app, "", Some("secret-token")).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
}

#[tokio::test]
async fn returns_503_when_audit_not_configured() {
    let app = build_test_app_without_audit().await;
    let (status, _body) = get_audit_log(&app, "").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}
