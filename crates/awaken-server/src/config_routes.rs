use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize, Default)]
struct DeleteParams {
    #[serde(default)]
    force: bool,
}

use crate::app::AppState;
use crate::routes::ApiError;
use crate::services::config_service::{
    ConfigNamespace, ConfigService, ConfigServiceError, ProviderTestResult,
};

#[derive(Deserialize)]
struct ListParams {
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    100
}

pub fn config_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/capabilities", get(get_capabilities))
        .route(
            "/v1/config/:namespace",
            get(list_config).post(create_config),
        )
        .route(
            "/v1/config/:namespace/:id",
            get(get_config).put(put_config).delete(delete_config),
        )
        .route("/v1/config/:namespace/$schema", get(get_schema))
        .route("/v1/agents", get(list_agents))
        .route("/v1/agents/:id", get(get_agent))
        .route("/v1/providers/:id/test", post(test_provider_connection))
}

async fn get_capabilities(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ConfigRouteError> {
    ensure_admin_auth(&state, &headers)?;
    let service = ConfigService::new(&state).map_err(map_service_error)?;
    Ok(Json(
        service.capabilities().await.map_err(map_service_error)?,
    ))
}

async fn get_schema(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(namespace): Path<String>,
) -> Result<impl IntoResponse, ConfigRouteError> {
    ensure_admin_auth(&state, &headers)?;
    let namespace = ConfigNamespace::parse(&namespace).map_err(map_service_error)?;
    Ok(Json(namespace.schema_json().map_err(map_service_error)?))
}

async fn list_agents(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Query<ListParams>,
) -> Result<impl IntoResponse, ConfigRouteError> {
    ensure_admin_auth(&state, &headers)?;
    list_config_inner(state, "agents".to_string(), query.0).await
}

async fn get_agent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ConfigRouteError> {
    ensure_admin_auth(&state, &headers)?;
    get_config_inner(state, "agents".to_string(), id).await
}

async fn list_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(namespace): Path<String>,
    Query(params): Query<ListParams>,
) -> Result<impl IntoResponse, ConfigRouteError> {
    ensure_admin_auth(&state, &headers)?;
    list_config_inner(state, namespace, params).await
}

async fn list_config_inner(
    state: AppState,
    namespace: String,
    params: ListParams,
) -> Result<impl IntoResponse, ConfigRouteError> {
    let namespace = ConfigNamespace::parse(&namespace).map_err(map_service_error)?;
    let service = ConfigService::new(&state).map_err(map_service_error)?;
    let items = service
        .list(namespace, params.offset, params.limit)
        .await
        .map_err(map_service_error)?;
    Ok(Json(json!({
        "namespace": namespace.as_str(),
        "items": items,
        "offset": params.offset,
        "limit": params.limit,
    })))
}

async fn create_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(namespace): Path<String>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ConfigRouteError> {
    ensure_admin_auth(&state, &headers)?;
    let namespace = ConfigNamespace::parse(&namespace).map_err(map_service_error)?;
    let service = ConfigService::new(&state).map_err(map_service_error)?;
    let created = service
        .create(namespace, body)
        .await
        .map_err(map_service_error)?;
    Ok((StatusCode::CREATED, Json(created)))
}

async fn get_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((namespace, id)): Path<(String, String)>,
) -> Result<impl IntoResponse, ConfigRouteError> {
    ensure_admin_auth(&state, &headers)?;
    get_config_inner(state, namespace, id).await
}

async fn get_config_inner(
    state: AppState,
    namespace: String,
    id: String,
) -> Result<impl IntoResponse, ConfigRouteError> {
    let namespace = ConfigNamespace::parse(&namespace).map_err(map_service_error)?;
    let service = ConfigService::new(&state).map_err(map_service_error)?;
    let value = service
        .get(namespace, &id)
        .await
        .map_err(map_service_error)?
        .ok_or_else(|| ApiError::NotFound(format!("{}/{}", namespace.as_str(), id)))?;
    Ok(Json(value))
}

async fn put_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((namespace, id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ConfigRouteError> {
    ensure_admin_auth(&state, &headers)?;
    let namespace = ConfigNamespace::parse(&namespace).map_err(map_service_error)?;
    let service = ConfigService::new(&state).map_err(map_service_error)?;
    let updated = service
        .update(namespace, &id, body)
        .await
        .map_err(map_service_error)?;
    Ok(Json(updated))
}

async fn delete_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((namespace, id)): Path<(String, String)>,
    Query(params): Query<DeleteParams>,
) -> Response {
    if let Err(err) = ensure_admin_auth(&state, &headers) {
        return err.into_response();
    }
    let namespace = match ConfigNamespace::parse(&namespace) {
        Ok(ns) => ns,
        Err(e) => return ConfigRouteError::Api(map_service_error(e)).into_response(),
    };
    let service = match ConfigService::new(&state) {
        Ok(s) => s,
        Err(e) => return ConfigRouteError::Api(map_service_error(e)).into_response(),
    };
    match service.delete(namespace, &id, params.force).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(ConfigServiceError::Blocked { used_by }) => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "cannot delete: other records depend on this resource",
                "used_by": used_by,
            })),
        )
            .into_response(),
        Err(e) => ConfigRouteError::Api(map_service_error(e)).into_response(),
    }
}

async fn test_provider_connection(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ConfigRouteError> {
    ensure_admin_auth(&state, &headers)?;
    let service = ConfigService::new(&state).map_err(map_service_error)?;
    let result: ProviderTestResult = service
        .test_provider(&id)
        .await
        .map_err(map_service_error)?;
    Ok(Json(result))
}

#[derive(Debug)]
enum ConfigRouteError {
    Api(ApiError),
    Unauthorized(String),
}

impl From<ApiError> for ConfigRouteError {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

impl IntoResponse for ConfigRouteError {
    fn into_response(self) -> Response {
        match self {
            ConfigRouteError::Api(error) => error.into_response(),
            ConfigRouteError::Unauthorized(message) => {
                (StatusCode::UNAUTHORIZED, Json(json!({ "error": message }))).into_response()
            }
        }
    }
}

fn ensure_admin_auth(state: &AppState, headers: &HeaderMap) -> Result<(), ConfigRouteError> {
    let config = crate::app::admin_api_config(state);
    ensure_admin_auth_for_token(config.bearer_token.as_ref(), headers)
}

fn ensure_admin_auth_for_token(
    expected: Option<&awaken_contract::RedactedString>,
    headers: &HeaderMap,
) -> Result<(), ConfigRouteError> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let Some(auth) = headers.get(axum::http::header::AUTHORIZATION) else {
        return Err(ConfigRouteError::Unauthorized(
            "admin authentication required".into(),
        ));
    };
    let auth = auth
        .to_str()
        .map_err(|_| ConfigRouteError::Unauthorized("invalid Authorization header".into()))?;
    let Some(token) = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))
    else {
        return Err(ConfigRouteError::Unauthorized(
            "Authorization header must use Bearer authentication".into(),
        ));
    };
    if token != expected.expose_secret() {
        return Err(ConfigRouteError::Unauthorized(
            "invalid admin bearer token".into(),
        ));
    }
    Ok(())
}

fn map_service_error(error: ConfigServiceError) -> ApiError {
    match error {
        ConfigServiceError::NotEnabled | ConfigServiceError::InvalidPayload(_) => {
            ApiError::BadRequest(error.to_string())
        }
        ConfigServiceError::UnknownNamespace(_)
        | ConfigServiceError::NotFound(_)
        | ConfigServiceError::Storage(
            awaken_contract::contract::storage::StorageError::NotFound(_),
        ) => ApiError::NotFound(error.to_string()),
        ConfigServiceError::MissingId => ApiError::BadRequest(error.to_string()),
        ConfigServiceError::Conflict(_) => ApiError::Conflict(error.to_string()),
        ConfigServiceError::Serialization(_)
        | ConfigServiceError::Apply(_)
        | ConfigServiceError::Storage(_) => ApiError::Internal(error.to_string()),
        // Blocked is matched inline in delete_config before reaching this function.
        ConfigServiceError::Blocked { .. } => ApiError::Conflict(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::RedactedString;
    use axum::http::{HeaderMap, HeaderValue, header};

    #[test]
    fn admin_auth_allows_unconfigured_routes() {
        let headers = HeaderMap::new();
        assert!(ensure_admin_auth_for_token(None, &headers).is_ok());
    }

    #[test]
    fn admin_auth_rejects_missing_or_wrong_token() {
        let expected = RedactedString::from("secret");
        let headers = HeaderMap::new();
        let missing = ensure_admin_auth_for_token(Some(&expected), &headers).unwrap_err();
        assert_eq!(missing.into_response().status(), StatusCode::UNAUTHORIZED);

        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong"),
        );
        let wrong = ensure_admin_auth_for_token(Some(&expected), &headers).unwrap_err();
        assert_eq!(wrong.into_response().status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn admin_auth_accepts_bearer_token() {
        let expected = RedactedString::from("secret");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        );
        assert!(ensure_admin_auth_for_token(Some(&expected), &headers).is_ok());
    }

    // ── delete 409 / force integration tests ──────────────────────────────

    mod delete_integration {
        use std::sync::Arc;

        use async_trait::async_trait;
        use awaken_contract::contract::executor::{
            InferenceExecutionError, InferenceRequest, LlmExecutor,
        };
        use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
        use awaken_contract::{AgentSpec, ModelBindingSpec, ProviderSpec};
        use awaken_runtime::builder::AgentRuntimeBuilder;
        use awaken_runtime::registry::traits::ModelBinding;
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use http_body_util::BodyExt;
        use serde_json::Value;
        use tower::ServiceExt;

        use crate::app::{AppState, ServerConfig};
        use crate::mailbox::{Mailbox, MailboxConfig};
        use crate::routes::build_router;
        use crate::services::config_runtime::{ConfigRuntimeManager, ProviderExecutorFactory};
        use crate::services::config_service::ConfigNamespace;

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
            ) -> Result<Arc<dyn LlmExecutor>, crate::services::config_runtime::ConfigRuntimeError>
            {
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

        fn bootstrap_agent() -> AgentSpec {
            AgentSpec {
                id: "bootstrap".into(),
                model_id: "bootstrap".into(),
                system_prompt: "bootstrap".into(),
                max_rounds: 1,
                ..Default::default()
            }
        }

        async fn build_test_app() -> axum::Router {
            let config_store = Arc::new(awaken_stores::InMemoryStore::new());
            let thread_store = Arc::new(awaken_stores::InMemoryStore::new());
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
                .expect("bootstrap config store");
            manager.apply().await.expect("publish config");

            let resolver = runtime.resolver_arc();
            let mailbox = Arc::new(Mailbox::new(
                runtime.clone(),
                Arc::new(awaken_stores::InMemoryMailboxStore::new()),
                thread_store.clone(),
                "route-test".into(),
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

            build_router(&state).with_state(state)
        }

        async fn create_record(app: &axum::Router, namespace: &str, body: &str) -> StatusCode {
            let req = Request::builder()
                .method("POST")
                .uri(format!("/v1/config/{namespace}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap();
            app.clone().oneshot(req).await.unwrap().status()
        }

        async fn delete_record(
            app: &axum::Router,
            namespace: &str,
            id: &str,
            force: bool,
        ) -> (StatusCode, Value) {
            let uri = if force {
                format!("/v1/config/{namespace}/{id}?force=true")
            } else {
                format!("/v1/config/{namespace}/{id}")
            };
            let req = Request::builder()
                .method("DELETE")
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            let body: Value = if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes).unwrap_or(Value::Null)
            };
            (status, body)
        }

        #[tokio::test]
        async fn delete_provider_with_referencing_model_returns_409_with_used_by() {
            let app = build_test_app().await;

            // Create a new provider and a model referencing it
            assert_eq!(
                create_record(&app, "providers", r#"{"id":"prov-x","adapter":"stub"}"#).await,
                StatusCode::CREATED
            );
            assert_eq!(
                create_record(
                    &app,
                    "models",
                    r#"{"id":"model-x","provider_id":"prov-x","upstream_model":"gpt-4"}"#
                )
                .await,
                StatusCode::CREATED
            );

            let (status, body) = delete_record(&app, "providers", "prov-x", false).await;
            assert_eq!(status, StatusCode::CONFLICT);
            let used_by = body["used_by"].as_array().expect("used_by array");
            assert!(!used_by.is_empty());
            assert!(used_by.iter().any(|r| r["id"] == "model-x"));
        }

        #[tokio::test]
        async fn delete_provider_with_force_true_succeeds_despite_dependents() {
            let app = build_test_app().await;

            assert_eq!(
                create_record(&app, "providers", r#"{"id":"prov-y","adapter":"stub"}"#).await,
                StatusCode::CREATED
            );
            assert_eq!(
                create_record(
                    &app,
                    "models",
                    r#"{"id":"model-y","provider_id":"prov-y","upstream_model":"gpt-4"}"#
                )
                .await,
                StatusCode::CREATED
            );

            let (status, _) = delete_record(&app, "providers", "prov-y", true).await;
            assert_eq!(status, StatusCode::NO_CONTENT);
        }

        #[tokio::test]
        async fn delete_agent_is_always_unblocked() {
            let app = build_test_app().await;

            // Bootstrap agent is a leaf — should delete without blocker
            // (bootstrap is a leaf, no dependents)
            // Create a standalone agent
            assert_eq!(
                create_record(
                    &app,
                    "agents",
                    r#"{"id":"agent-leaf","model_id":"bootstrap","system_prompt":"hi","max_rounds":1}"#
                )
                .await,
                StatusCode::CREATED
            );

            let (status, _) = delete_record(&app, "agents", "agent-leaf", false).await;
            assert_eq!(status, StatusCode::NO_CONTENT);
        }

        async fn test_provider(app: &axum::Router, id: &str) -> (StatusCode, Value) {
            let req = Request::builder()
                .method("POST")
                .uri(format!("/v1/providers/{id}/test"))
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            (status, body)
        }

        #[tokio::test]
        async fn test_provider_existing_openai_spec_returns_200_with_result() {
            // The bootstrap provider has adapter "stub" which is not a valid
            // genai adapter, so build_genai_provider_executor returns an error
            // and ok=false. The route still returns HTTP 200 — the ok field
            // inside the body conveys the probe outcome.
            let app = build_test_app().await;
            let (status, body) = test_provider(&app, "bootstrap").await;
            assert_eq!(status, StatusCode::OK, "body: {body}");
            // The response must contain ok and latency_ms regardless of outcome.
            assert!(body.get("ok").is_some(), "must have ok field");
            assert!(
                body["latency_ms"].is_number(),
                "expected latency_ms to be a number"
            );
        }

        #[tokio::test]
        async fn test_provider_with_valid_genai_adapter_returns_ok_true() {
            // Create a provider with a genai-supported adapter via the route.
            // TestProviderFactory accepts any adapter for apply so we override it
            // in the test state. Instead, we can create the spec directly in the
            // config store and bypass the apply path.
            let config_store = Arc::new(awaken_stores::InMemoryStore::new());
            let thread_store = Arc::new(awaken_stores::InMemoryStore::new());
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
            // Use GenaiProviderExecutorFactory so we can create openai providers.
            let manager = Arc::new(
                crate::services::config_runtime::ConfigRuntimeManager::new(
                    runtime.clone(),
                    config_store.clone(),
                )
                .expect("manager"),
            );
            // Write an openai provider directly into the store (skip apply).
            awaken_contract::contract::config_store::ConfigStore::put(
                config_store.as_ref(),
                "providers",
                "prov-openai",
                &serde_json::json!({ "id": "prov-openai", "adapter": "openai" }),
            )
            .await
            .expect("put provider");

            let resolver = runtime.resolver_arc();
            let mailbox = Arc::new(Mailbox::new(
                runtime.clone(),
                Arc::new(awaken_stores::InMemoryMailboxStore::new()),
                thread_store.clone(),
                "route-test-2".into(),
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
            let app = build_router(&state).with_state(state);

            let (status, body) = test_provider(&app, "prov-openai").await;
            assert_eq!(status, StatusCode::OK, "body: {body}");
            assert_eq!(body["ok"], true, "expected ok=true for openai adapter");
            assert!(body.get("error").is_none(), "should have no error field");
        }

        #[tokio::test]
        async fn test_provider_missing_id_returns_404() {
            let app = build_test_app().await;
            let (status, _body) = test_provider(&app, "no-such-provider").await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }
    }
}
