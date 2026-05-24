//! Security regression tests for the shared admin HTTP surface.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::registry_spec::{AgentSpec, ModelBindingSpec, ProviderSpec};
use awaken_contract::{BuiltinSeedSet, BuiltinSpec};
use awaken_ext_observability::RuntimeStatsRegistry;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_server::app::{AdminApiConfig, AppState, ServerConfig};
use awaken_server::mailbox::{Mailbox, MailboxConfig};
use awaken_server::routes::build_router;
use awaken_server::services::audit_log::AuditLogger;
use awaken_server::services::config_runtime::{
    ConfigRuntimeError, ConfigRuntimeManager, ProviderExecutorFactory,
};
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
            BuiltinSpec::model(ModelBindingSpec {
                id: "bootstrap".into(),
                provider_id: "bootstrap".into(),
                upstream_model: "bootstrap-model".into(),
                input_token_price_per_million_usd: None,
                output_token_price_per_million_usd: None,
            }),
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
    let state = AppState::new(
        runtime,
        mailbox,
        thread_store,
        resolver,
        ServerConfig::default(),
    )
    .with_config_store(config_store)
    .with_config_runtime_manager(manager)
    .with_audit_log(audit_logger)
    .with_runtime_stats(Arc::new(RuntimeStatsRegistry::new()))
    .with_admin_api_config(AdminApiConfig {
        bearer_token: Some(ADMIN_TOKEN.into()),
        ..Default::default()
    });

    build_router(&state).with_state(state)
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

#[tokio::test]
async fn admin_routes_reject_missing_wrong_and_ambiguous_authorization_before_handler_logic() {
    let app = build_secure_admin_router().await;
    let routes = [
        (Method::GET, "/v1/system/info", None),
        (Method::GET, "/v1/agents/runtime-stats", None),
        (Method::GET, "/v1/agents/bootstrap/runtime-stats", None),
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
async fn valid_bearer_reaches_admin_handlers_without_auth_failure() {
    let app = build_secure_admin_router().await;
    let header = format!("Bearer {ADMIN_TOKEN}");

    for uri in [
        "/v1/system/info",
        "/v1/agents/runtime-stats",
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
