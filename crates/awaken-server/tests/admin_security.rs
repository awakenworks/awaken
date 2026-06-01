//! Security regression tests for the shared admin HTTP surface.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_ext_observability::RuntimeStatsRegistry;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_server::app::{AdminApiConfig, ConfigModuleState, ServerConfig, ServerState};
use awaken_server::mailbox::{Mailbox, MailboxConfig};
use awaken_server::routes::build_router;
use awaken_server::services::audit_log::AuditLogger;
use awaken_server::services::config_runtime::{
    ConfigRuntimeError, ConfigRuntimeManager, ProviderExecutorFactory,
};
use awaken_server_contract::contract::executor::{
    InferenceExecutionError, InferenceRequest, LlmExecutor,
};
use awaken_server_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_server_contract::registry_spec::{AgentSpec, ModelSpec, ProviderSpec};
use awaken_server_contract::{BuiltinSeedSet, BuiltinSpec};
use awaken_stores::InMemoryStore;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

const ADMIN_TOKEN: &str = "super-secret-admin-token";

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

async fn build_secure_admin_router() -> axum::Router {
    let config_store = Arc::new(InMemoryStore::new());
    let thread_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(ImmediateExecutor))
            .with_in_memory_thread_run_store(thread_store.clone())
            .build()
            .expect("build runtime"),
    );

    let audit_logger = Arc::new(AuditLogger::new(config_store.clone()));
    let manager = Arc::new(
        ConfigRuntimeManager::new(runtime.clone(), config_store.clone())
            .expect("config runtime manager")
            .with_provider_factory(Arc::new(TestProviderFactory))
            .with_audit_log(audit_logger.clone()),
    );
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
    manager.apply().await.expect("apply");

    let resolver = runtime.resolver_arc();
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        thread_store.clone(),
        "admin-security-test".into(),
        MailboxConfig::default(),
    ));
    let mut state = ServerState::new(
        runtime,
        mailbox,
        thread_store,
        resolver,
        ServerConfig::default(),
    );
    state.config = Some(ConfigModuleState::new(config_store, manager).with_audit_log(audit_logger));
    state.run.runtime_stats = Some(Arc::new(RuntimeStatsRegistry::new()));
    state.admin.admin_api_config = AdminApiConfig {
        bearer_token: Some(ADMIN_TOKEN.into()),
        ..Default::default()
    };

    build_router(&state)
}

async fn request(
    app: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    auth_headers: &[String],
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    for value in auth_headers {
        builder = builder.header("authorization", value.as_str());
    }

    let request = if let Some(body) = body {
        builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request")
    } else {
        builder.body(Body::empty()).expect("request")
    };

    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    let body = String::from_utf8_lossy(&bytes).into_owned();
    (status, body)
}

fn http_contract_shape(value: Value) -> Value {
    match value {
        Value::Null => json!("<null>"),
        Value::Bool(_) => json!("<bool>"),
        Value::Number(number) if number.is_u64() => json!("<u64>"),
        Value::Number(number) if number.is_i64() => json!("<i64>"),
        Value::Number(_) => json!("<number>"),
        Value::String(_) => json!("<string>"),
        Value::Array(items) => Value::Array(items.into_iter().map(http_contract_shape).collect()),
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (key, http_contract_shape(value)))
                .collect(),
        ),
    }
}

async fn get_json_contract_shape(app: &axum::Router, uri: &str) -> Value {
    let header = format!("Bearer {ADMIN_TOKEN}");
    let (status, body) = request(app, Method::GET, uri, None, &[header]).await;
    assert_eq!(status, StatusCode::OK, "GET {uri} failed: {body}");
    http_contract_shape(serde_json::from_str(&body).expect("json response"))
}

async fn json_contract_shape(
    app: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    expected_status: StatusCode,
) -> Value {
    let header = format!("Bearer {ADMIN_TOKEN}");
    let (status, response_body) = request(app, method.clone(), uri, body, &[header]).await;
    assert_eq!(
        status, expected_status,
        "{method} {uri} returned {status}: {response_body}"
    );
    http_contract_shape(serde_json::from_str(&response_body).expect("json response"))
}

#[tokio::test]
async fn admin_routes_reject_missing_wrong_and_ambiguous_authorization_before_handler_logic() {
    let app = build_secure_admin_router().await;
    let routes = [
        (Method::GET, "/v1/system/info", None),
        (Method::GET, "/v1/agents/runtime-stats", None),
        (Method::GET, "/v1/agents/bootstrap/runtime-stats", None),
        (Method::GET, "/v1/runs/summary", None),
        (Method::GET, "/v1/capabilities", None),
        (Method::GET, "/v1/config/providers", None),
        (Method::GET, "/v1/config/providers/$schema", None),
        (Method::GET, "/v1/agents", None),
        (Method::GET, "/v1/audit-log", None),
        (
            Method::POST,
            "/v1/config/providers",
            Some(json!({"id": "attack", "adapter": "stub"})),
        ),
        (
            Method::PUT,
            "/v1/config/providers/bootstrap",
            Some(json!({"id": "bootstrap", "adapter": "stub"})),
        ),
        (Method::DELETE, "/v1/config/providers/bootstrap", None),
    ];

    for (method, uri, body) in routes {
        let valid_header = format!("Bearer {ADMIN_TOKEN}");
        let tab_header = format!("Bearer\t{ADMIN_TOKEN}");
        for headers in [
            Vec::new(),
            vec!["Bearer wrong-token".to_string()],
            vec![valid_header.clone(), "Bearer wrong-token".to_string()],
            vec![tab_header.clone()],
        ] {
            let (status, response_body) =
                request(&app, method.clone(), uri, body.clone(), &headers).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} {uri} with headers {headers:?} returned {status}: {response_body}"
            );
            assert!(
                !response_body.contains(ADMIN_TOKEN),
                "401 body must not leak the configured token: {response_body}"
            );
        }
    }
}

#[tokio::test]
async fn unauthorized_admin_mutation_does_not_execute_handler_side_effects() {
    let app = build_secure_admin_router().await;

    let (status, body) = request(
        &app,
        Method::POST,
        "/v1/config/providers",
        Some(json!({
            "id": "unauthorized-provider",
            "adapter": "stub"
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body={body}");

    let header = format!("Bearer {ADMIN_TOKEN}");
    let (status, body) = request(
        &app,
        Method::GET,
        "/v1/config/providers/unauthorized-provider",
        None,
        &[header],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "unauthorized mutation must not create a provider: {body}"
    );
}

#[tokio::test]
async fn admin_http_contract_snapshot_covers_frontend_route_shapes() {
    let app = build_secure_admin_router().await;

    let snapshots = [
        (
            "/v1/system/info",
            json!({
                "audit_log_enabled": "<bool>",
                "config_store_enabled": "<bool>",
                "runtime_stats_enabled": "<bool>",
                "scope_id": "<string>",
                "uptime_seconds": "<u64>",
                "version": "<string>",
            }),
        ),
        (
            "/v1/system/modules",
            json!({
                "modules": [
                    "<string>",
                    "<string>",
                    "<string>",
                    "<string>"
                ],
            }),
        ),
        (
            "/v1/config/providers",
            json!({
                "items": [{
                    "adapter": "<string>",
                    "id": "<string>",
                    "timeout_secs": "<u64>"
                }],
                "limit": "<u64>",
                "namespace": "<string>",
                "offset": "<u64>"
            }),
        ),
        (
            "/v1/config/models",
            json!({
                "items": [{
                    "id": "<string>",
                    "provider_id": "<string>",
                    "upstream_model": "<string>"
                }],
                "limit": "<u64>",
                "namespace": "<string>",
                "offset": "<u64>"
            }),
        ),
        (
            "/v1/config/agents",
            json!({
                "items": [{
                    "allowed_tool_patterns": ["<string>"],
                    "allowed_tools": "<null>",
                    "id": "<string>",
                    "max_continuation_retries": "<u64>",
                    "max_rounds": "<u64>",
                    "model_id": "<string>",
                    "plugin_ids": [],
                    "system_prompt": "<string>"
                }],
                "limit": "<u64>",
                "namespace": "<string>",
                "offset": "<u64>"
            }),
        ),
    ];

    for (uri, expected) in snapshots {
        let actual = get_json_contract_shape(&app, uri).await;
        assert_eq!(actual, expected, "HTTP contract shape changed for {uri}");
    }
}

#[tokio::test]
async fn admin_http_contract_snapshot_covers_mutation_error_and_audit_shapes() {
    let app = build_secure_admin_router().await;

    let created = json_contract_shape(
        &app,
        Method::POST,
        "/v1/config/providers",
        Some(json!({
            "id": "frontend-secret-provider",
            "adapter": "stub",
            "api_key": "redaction-test-token"
        })),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(
        created,
        json!({
            "adapter": "<string>",
            "has_api_key": "<bool>",
            "id": "<string>"
        })
    );

    let fetched =
        get_json_contract_shape(&app, "/v1/config/providers/frontend-secret-provider").await;
    assert_eq!(fetched, created);

    let duplicate = json_contract_shape(
        &app,
        Method::POST,
        "/v1/config/providers",
        Some(json!({
            "id": "frontend-secret-provider",
            "adapter": "stub"
        })),
        StatusCode::CONFLICT,
    )
    .await;
    assert_eq!(
        duplicate,
        json!({
            "error": "<string>"
        })
    );

    let audit = get_json_contract_shape(&app, "/v1/audit-log").await;
    assert_eq!(
        audit,
        json!({
            "items": [{
                "action": "<string>",
                "actor": "<string>",
                "after": {
                    "adapter": "<string>",
                    "api_key": "<string>",
                    "id": "<string>"
                },
                "id": "<string>",
                "resource": "<string>",
                "ts": "<string>"
            }, {
                "action": "<string>",
                "actor": "<string>",
                "after": {
                    "bucket": "<string>",
                    "count": "<u64>",
                    "sample": ["<string>", "<string>", "<string>"],
                    "truncated": "<bool>"
                },
                "id": "<string>",
                "resource": "<string>",
                "ts": "<string>"
            }],
            "next_cursor": "<null>"
        })
    );

    let header = format!("Bearer {ADMIN_TOKEN}");
    let (status, body) = request(
        &app,
        Method::GET,
        "/v1/config/providers/frontend-secret-provider",
        None,
        std::slice::from_ref(&header),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.contains("redaction-test-token"),
        "admin config response must redact provider secret material: {body}"
    );
    let (status, body) = request(&app, Method::GET, "/v1/audit-log", None, &[header]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.contains("redaction-test-token"),
        "audit response must redact provider secret material: {body}"
    );
}

#[tokio::test]
async fn valid_bearer_reaches_admin_handlers_without_auth_failure() {
    let app = build_secure_admin_router().await;
    let header = format!("Bearer {ADMIN_TOKEN}");

    for uri in [
        "/v1/system/info",
        "/v1/agents/runtime-stats",
        "/v1/runs/summary",
        "/v1/config/providers",
        "/v1/audit-log",
    ] {
        let (status, body) =
            request(&app, Method::GET, uri, None, std::slice::from_ref(&header)).await;
        assert_ne!(
            status,
            StatusCode::UNAUTHORIZED,
            "valid bearer must pass auth for {uri}: {body}"
        );
    }
}
