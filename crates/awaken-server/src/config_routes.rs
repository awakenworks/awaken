use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::AppState;
use crate::routes::ApiError;
use crate::services::config_service::{ConfigNamespace, ConfigService, ConfigServiceError};

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
}

async fn get_capabilities(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
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
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state, &headers)?;
    let namespace = ConfigNamespace::parse(&namespace).map_err(map_service_error)?;
    Ok(Json(namespace.schema_json().map_err(map_service_error)?))
}

async fn list_agents(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Query<ListParams>,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state, &headers)?;
    list_config_inner(state, "agents".to_string(), query.0).await
}

async fn get_agent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state, &headers)?;
    get_config_inner(state, "agents".to_string(), id).await
}

async fn list_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(namespace): Path<String>,
    Query(params): Query<ListParams>,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state, &headers)?;
    list_config_inner(state, namespace, params).await
}

async fn list_config_inner(
    state: AppState,
    namespace: String,
    params: ListParams,
) -> Result<impl IntoResponse, ApiError> {
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
) -> Result<impl IntoResponse, ApiError> {
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
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state, &headers)?;
    get_config_inner(state, namespace, id).await
}

async fn get_config_inner(
    state: AppState,
    namespace: String,
    id: String,
) -> Result<impl IntoResponse, ApiError> {
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
) -> Result<impl IntoResponse, ApiError> {
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
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state, &headers)?;
    let namespace = ConfigNamespace::parse(&namespace).map_err(map_service_error)?;
    let service = ConfigService::new(&state).map_err(map_service_error)?;
    service
        .delete(namespace, &id)
        .await
        .map_err(map_service_error)?;
    Ok(StatusCode::NO_CONTENT)
}

fn ensure_admin_auth(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(expected) = state.config.admin_api_bearer_token.as_deref() else {
        return Ok(());
    };
    let Some(auth) = headers.get(axum::http::header::AUTHORIZATION) else {
        return Err(ApiError::Unauthorized(
            "admin authentication required".into(),
        ));
    };
    let auth = auth
        .to_str()
        .map_err(|_| ApiError::Unauthorized("invalid Authorization header".into()))?;
    let Some(token) = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))
    else {
        return Err(ApiError::Unauthorized(
            "Authorization header must use Bearer authentication".into(),
        ));
    };
    if token != expected {
        return Err(ApiError::Unauthorized("invalid admin bearer token".into()));
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
    }
}
