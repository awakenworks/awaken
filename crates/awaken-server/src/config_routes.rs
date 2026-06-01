use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete as delete_route, get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

#[derive(Deserialize, Default)]
struct DeleteParams {
    #[serde(default)]
    force: bool,
}

use crate::app::{AdminModuleState, ConfigRoutesState};
use crate::routes::ApiError;
use crate::services::audit_log::{AuditQuery, AuditQueryError};
use crate::services::config_service::{
    ConfigNamespace, ConfigService, ConfigServiceError, ProviderTestResult, RestoreError,
    is_overrides_not_supported_for_user_record, tool_schema_json,
};

const TOOLS_NAMESPACE: &str = "tools";

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

#[derive(Clone, Copy)]
enum RouteConfigNamespace {
    Managed(ConfigNamespace),
    Tools,
}

impl RouteConfigNamespace {
    fn parse(value: &str) -> Result<Self, ConfigServiceError> {
        if value == TOOLS_NAMESPACE {
            Ok(Self::Tools)
        } else {
            ConfigNamespace::parse(value).map(Self::Managed)
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Managed(namespace) => namespace.as_str(),
            Self::Tools => TOOLS_NAMESPACE,
        }
    }

    fn read_only_error(self) -> ConfigServiceError {
        match self {
            Self::Managed(_) => {
                ConfigServiceError::InvalidPayload("namespace is not read-only".into())
            }
            Self::Tools => ConfigServiceError::InvalidPayload(
                "tools namespace is read-only; use PATCH /v1/config/tools/:id/overrides".into(),
            ),
        }
    }
}

pub fn config_routes() -> Router<ConfigRoutesState> {
    Router::new()
        .route(
            "/v1/config/:namespace",
            get(list_config).post(create_config),
        )
        .route("/v1/config/:namespace/validate", post(validate_config))
        .route(
            "/v1/config/:namespace/:id",
            get(get_config).put(put_config).delete(delete_config),
        )
        .route("/v1/config/:namespace/:id/restore", post(restore_config))
        .route("/v1/config/:namespace/:id/meta", get(get_config_meta))
        .route("/v1/config/:namespace/meta", get(list_config_meta))
        .route("/v1/config/:namespace/$schema", get(get_schema))
        .route("/v1/config/diagnostics", get(get_config_diagnostics))
        .route(
            "/v1/config/providers/:id/removal-preview",
            get(preview_provider_removal),
        )
        .route("/v1/agents", get(list_agents))
        .route("/v1/agents/:id", get(get_agent))
        .route(
            "/v1/config/agents/:id/overrides",
            post(validate_agent_overrides_handler)
                .patch(patch_agent_overrides_handler)
                .delete(clear_agent_overrides_handler),
        )
        .route(
            "/v1/config/agents/:id/overrides/:field",
            delete_route(clear_agent_override_field_handler),
        )
        .route(
            "/v1/config/tools/:id/overrides",
            patch(patch_tool_overrides_handler).delete(clear_tool_overrides_handler),
        )
        .route(
            "/v1/config/tools/:id/overrides/:field",
            delete_route(clear_tool_override_field_handler),
        )
        .route("/v1/providers/:id/test", post(test_provider_connection))
        .route("/v1/mcp-servers/:id/status", get(get_mcp_server_status))
        .route("/v1/mcp-servers/:id/restart", post(post_mcp_server_restart))
        .route("/v1/audit-log", get(list_audit_log))
}

pub fn capabilities_routes() -> Router<ConfigRoutesState> {
    Router::new().route("/v1/capabilities", get(get_capabilities))
}

async fn get_capabilities(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state.admin, &headers)?;
    let service = ConfigService::new(&state).map_err(map_service_error)?;
    Ok(Json(
        service.capabilities().await.map_err(map_service_error)?,
    ))
}

async fn get_schema(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path(namespace): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state.admin, &headers)?;
    let namespace = RouteConfigNamespace::parse(&namespace).map_err(map_service_error)?;
    let schema = match namespace {
        RouteConfigNamespace::Managed(namespace) => namespace.schema_json(),
        RouteConfigNamespace::Tools => tool_schema_json(),
    }
    .map_err(map_service_error)?;
    Ok(Json(schema))
}

async fn get_config_diagnostics(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state.admin, &headers)?;
    let service = ConfigService::new(&state).map_err(map_service_error)?;
    let diagnostics = service.registry_diagnostics().map_err(map_service_error)?;
    Ok(Json(json!({ "diagnostics": diagnostics })))
}

async fn preview_provider_removal(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state.admin, &headers)?;
    let service = ConfigService::new(&state).map_err(map_service_error)?;
    let preview = service
        .preview_remove_provider(&id)
        .await
        .map_err(map_service_error)?;
    Ok(Json(preview))
}

async fn list_agents(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    query: Query<ListParams>,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state.admin, &headers)?;
    list_config_inner(state, "agents".to_string(), query.0).await
}

async fn get_agent(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state.admin, &headers)?;
    get_config_inner(state, "agents".to_string(), id).await
}

async fn list_config(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path(namespace): Path<String>,
    Query(params): Query<ListParams>,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state.admin, &headers)?;
    list_config_inner(state, namespace, params).await
}

async fn list_config_inner(
    state: ConfigRoutesState,
    namespace: String,
    params: ListParams,
) -> Result<impl IntoResponse, ApiError> {
    let namespace = RouteConfigNamespace::parse(&namespace).map_err(map_service_error)?;
    let service = ConfigService::new(&state).map_err(map_service_error)?;
    let items = match namespace {
        RouteConfigNamespace::Managed(namespace) => {
            service.list(namespace, params.offset, params.limit).await
        }
        RouteConfigNamespace::Tools => service.list_tools(params.offset, params.limit).await,
    }
    .map_err(map_service_error)?;
    Ok(Json(json!({
        "namespace": namespace.as_str(),
        "items": items,
        "offset": params.offset,
        "limit": params.limit,
    })))
}

async fn create_config(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path(namespace): Path<String>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state.admin, &headers)?;
    let namespace = RouteConfigNamespace::parse(&namespace).map_err(map_service_error)?;
    let namespace = match namespace {
        RouteConfigNamespace::Managed(namespace) => namespace,
        RouteConfigNamespace::Tools => {
            return Err(map_service_error(namespace.read_only_error()));
        }
    };
    let service = ConfigService::new(&state).map_err(map_service_error)?;
    let created = service
        .create_with_headers(namespace, body, &headers)
        .await
        .map_err(map_service_error)?;
    Ok((StatusCode::CREATED, Json(created)))
}

#[derive(Deserialize, Default)]
struct ValidateParams {
    /// Optional id from query string when validating an update without
    /// going through `:id` in the path. The body must still carry an `id`.
    #[serde(default)]
    id: Option<String>,
}

async fn validate_config(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path(namespace): Path<String>,
    Query(params): Query<ValidateParams>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state.admin, &headers)?;
    let namespace = RouteConfigNamespace::parse(&namespace).map_err(map_service_error)?;
    let namespace = match namespace {
        RouteConfigNamespace::Managed(namespace) => namespace,
        RouteConfigNamespace::Tools => {
            return Err(map_service_error(namespace.read_only_error()));
        }
    };
    let service = ConfigService::new(&state).map_err(map_service_error)?;
    let normalized = service
        .validate(namespace, params.id.as_deref(), body)
        .await
        .map_err(map_service_error)?;
    Ok(Json(json!({
        "ok": true,
        "normalized": normalized,
    })))
}

async fn get_config(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path((namespace, id)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state.admin, &headers)?;
    get_config_inner(state, namespace, id).await
}

async fn get_config_inner(
    state: ConfigRoutesState,
    namespace: String,
    id: String,
) -> Result<impl IntoResponse, ApiError> {
    let namespace = RouteConfigNamespace::parse(&namespace).map_err(map_service_error)?;
    let service = ConfigService::new(&state).map_err(map_service_error)?;
    let value = match namespace {
        RouteConfigNamespace::Managed(namespace) => service.get(namespace, &id).await,
        RouteConfigNamespace::Tools => service.get_tool(&id).await,
    }
    .map_err(map_service_error)?
    .ok_or_else(|| ApiError::NotFound(format!("{}/{}", namespace.as_str(), id)))?;
    Ok(Json(value))
}

async fn get_config_meta(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path((namespace, id)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state.admin, &headers)?;
    let namespace = RouteConfigNamespace::parse(&namespace).map_err(map_service_error)?;
    let service = ConfigService::new(&state).map_err(map_service_error)?;
    let meta = match namespace {
        RouteConfigNamespace::Managed(namespace) => service.get_meta(namespace, &id).await,
        RouteConfigNamespace::Tools => service.get_tool_meta(&id).await,
    }
    .map_err(map_service_error)?
    .ok_or_else(|| ApiError::NotFound(format!("{}/{}", namespace.as_str(), id)))?;
    Ok(Json(meta))
}

async fn list_config_meta(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path(namespace): Path<String>,
    Query(params): Query<ListParams>,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state.admin, &headers)?;
    let namespace = RouteConfigNamespace::parse(&namespace).map_err(map_service_error)?;
    let service = ConfigService::new(&state).map_err(map_service_error)?;
    let items = match namespace {
        RouteConfigNamespace::Managed(namespace) => {
            service
                .list_meta(namespace, params.offset, params.limit)
                .await
        }
        RouteConfigNamespace::Tools => service.list_tool_meta(params.offset, params.limit).await,
    }
    .map_err(map_service_error)?;
    let response: Vec<_> = items
        .into_iter()
        .map(|(id, meta)| json!({ "id": id, "meta": meta }))
        .collect();
    Ok(Json(response))
}

async fn put_config(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path((namespace, id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state.admin, &headers)?;
    let namespace = RouteConfigNamespace::parse(&namespace).map_err(map_service_error)?;
    let namespace = match namespace {
        RouteConfigNamespace::Managed(namespace) => namespace,
        RouteConfigNamespace::Tools => {
            return Err(map_service_error(namespace.read_only_error()));
        }
    };
    let service = ConfigService::new(&state).map_err(map_service_error)?;
    let updated = service
        .update_with_headers(namespace, &id, body, &headers)
        .await
        .map_err(map_service_error)?;
    Ok(Json(updated))
}

async fn delete_config(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path((namespace, id)): Path<(String, String)>,
    Query(params): Query<DeleteParams>,
) -> Response {
    if let Err(err) = ensure_admin_auth(&state.admin, &headers) {
        return err.into_response();
    }
    let namespace = match RouteConfigNamespace::parse(&namespace) {
        Ok(ns) => ns,
        Err(e) => return map_service_error(e).into_response(),
    };
    let namespace = match namespace {
        RouteConfigNamespace::Managed(namespace) => namespace,
        RouteConfigNamespace::Tools => {
            return map_service_error(namespace.read_only_error()).into_response();
        }
    };
    let service = match ConfigService::new(&state) {
        Ok(s) => s,
        Err(e) => return map_service_error(e).into_response(),
    };
    let blockers = match delete_blockers_for_route(&service, namespace, &id, params.force).await {
        Ok(blockers) => blockers,
        Err(e) => return map_service_error(e).into_response(),
    };
    if !blockers.is_empty() {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "cannot delete: other records depend on this resource",
                "used_by": blockers,
            })),
        )
            .into_response();
    }
    match service
        .delete_with_options(namespace, &id, params.force, &headers)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => map_service_error(e).into_response(),
    }
}

async fn delete_blockers_for_route(
    service: &ConfigService,
    namespace: ConfigNamespace,
    id: &str,
    force: bool,
) -> Result<Vec<crate::services::config_service::DependentRef>, ConfigServiceError> {
    let provider_force = force && matches!(namespace, ConfigNamespace::Providers);
    if !provider_force {
        return service.find_dependents(namespace, id).await;
    }

    let provider_models = service
        .find_dependents(ConfigNamespace::Providers, id)
        .await?;
    let mut agent_blockers = Vec::new();
    for model_ref in provider_models {
        agent_blockers.extend(
            service
                .find_dependents(ConfigNamespace::Models, &model_ref.id)
                .await?,
        );
    }
    Ok(agent_blockers)
}

/// Body accepted by `POST /v1/config/:namespace/:id/restore`.
#[derive(Deserialize)]
struct RestoreBody {
    version: String,
}

async fn restore_config(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path((namespace, id)): Path<(String, String)>,
    Json(body): Json<RestoreBody>,
) -> Response {
    if let Err(err) = ensure_admin_auth(&state.admin, &headers) {
        return err.into_response();
    }
    let namespace = match RouteConfigNamespace::parse(&namespace) {
        Ok(ns) => ns,
        Err(e) => return map_service_error(e).into_response(),
    };
    let namespace = match namespace {
        RouteConfigNamespace::Managed(namespace) => namespace,
        RouteConfigNamespace::Tools => {
            return map_service_error(namespace.read_only_error()).into_response();
        }
    };
    let service = match ConfigService::new(&state) {
        Ok(s) => s,
        Err(e) => return map_service_error(e).into_response(),
    };
    match service
        .restore(namespace, &id, &body.version, &headers)
        .await
    {
        Ok(result) => Json(result).into_response(),
        Err(RestoreError::VersionNotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "version not found", "reason": "unknown"})),
        )
            .into_response(),
        Err(RestoreError::AuditNotConfigured) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "audit log is not configured"})),
        )
            .into_response(),
        Err(RestoreError::ResourceMismatch { .. }) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "cross-resource restore not allowed"})),
        )
            .into_response(),
        Err(RestoreError::NotRestorable) | Err(RestoreError::NoPayload(_)) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "this event type is not restorable"})),
        )
            .into_response(),
        Err(RestoreError::Service(ConfigServiceError::InvalidPayload(msg))) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": msg})),
        )
            .into_response(),
        Err(RestoreError::Service(e)) => map_service_error(e).into_response(),
        Err(RestoreError::Storage(e)) => ApiError::Internal(e.to_string()).into_response(),
    }
}

async fn test_provider_connection(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state.admin, &headers)?;
    let service = ConfigService::new(&state).map_err(map_service_error)?;
    let result: ProviderTestResult = service
        .test_provider(&id)
        .await
        .map_err(map_service_error)?;
    Ok(Json(result))
}

async fn get_mcp_server_status(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state.admin, &headers)?;

    // 503 when no MCP runtime is configured.
    let manager = &state.config.runtime_manager;

    // 404 when the id is unknown to the active registry.
    let status = manager
        .mcp_server_status(&id)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("mcp-server/{id}")))?;

    Ok(Json(json!({
        "connected": status.connected,
        "last_error": status.last_error,
        "tools": status.tools.iter().map(|t| json!({
            "name": t.name,
            "description": t.description,
        })).collect::<Vec<_>>(),
        "consecutive_failures": status.consecutive_failures,
        "last_attempt_at": status.last_attempt_at.and_then(systime_to_secs),
        "last_success_at": status.last_success_at.and_then(systime_to_secs),
        "reconnecting": status.reconnecting,
        "permanently_failed": status.permanently_failed,
        "session_generation": status.session_generation,
        "transport_reconnect_count": status.transport_reconnect_count,
        "last_init_at": status.last_init_at.and_then(systime_to_secs),
    })))
}

fn systime_to_secs(t: std::time::SystemTime) -> Option<u64> {
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

async fn post_mcp_server_restart(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state.admin, &headers)?;

    // 503 when no MCP runtime is configured.
    let manager = &state.config.runtime_manager;

    manager.mcp_server_reconnect(&id).await.map_err(|e| {
        // Map unknown-server errors (surfaced as InvalidConfig wrapping UnknownServer) to 404.
        let msg = e.to_string();
        if msg.contains("unknown server") || msg.contains("no MCP registry is active") {
            if msg.contains("no MCP registry") {
                ApiError::ServiceUnavailable(msg)
            } else {
                ApiError::NotFound(format!("mcp-server/{id}"))
            }
        } else {
            ApiError::Internal(msg)
        }
    })?;

    // Emit audit event after successful restart.
    if let Some(audit) = state.config.audit_log.clone() {
        let resource = format!("mcp-servers/{id}");
        audit
            .emit(
                awaken_server_contract::AuditAction::Restart,
                &resource,
                None,
                None,
                &headers,
            )
            .await;
    }

    Ok(StatusCode::ACCEPTED)
}

async fn list_audit_log(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Query(query): Query<AuditQuery>,
) -> Result<impl IntoResponse, ApiError> {
    ensure_admin_auth(&state.admin, &headers)?;
    let audit = state
        .config
        .audit_log
        .clone()
        .ok_or_else(|| ApiError::ServiceUnavailable("audit log is not configured".into()))?;
    let mut effective_query = query;
    effective_query.limit = effective_query.limit.clamp(1, 1000);
    let page = audit.query(effective_query).await.map_err(|e| match e {
        AuditQueryError::InvalidCursor => ApiError::BadRequest("invalid cursor".into()),
        AuditQueryError::Storage(storage_err) => ApiError::Internal(storage_err.to_string()),
    })?;
    Ok(Json(page))
}

pub(crate) fn ensure_admin_auth(
    admin: &AdminModuleState,
    headers: &HeaderMap,
) -> Result<(), ApiError> {
    ensure_admin_auth_for_token(admin.admin_api_config.bearer_token.as_ref(), headers)
}

fn ensure_admin_auth_for_token(
    expected: Option<&awaken_server_contract::RedactedString>,
    headers: &HeaderMap,
) -> Result<(), ApiError> {
    let Some(expected) = expected else {
        return Err(ApiError::Unauthorized(
            "admin authentication is not configured".into(),
        ));
    };
    let mut auth_values = headers.get_all(axum::http::header::AUTHORIZATION).iter();
    let Some(auth) = auth_values.next() else {
        return Err(ApiError::Unauthorized(
            "admin authentication required".into(),
        ));
    };
    if auth_values.next().is_some() {
        return Err(ApiError::Unauthorized(
            "multiple Authorization headers are not allowed".into(),
        ));
    }
    let auth = auth
        .to_str()
        .map_err(|_| ApiError::Unauthorized("invalid Authorization header".into()))?;
    let Some(token) = crate::auth::strip_bearer_prefix(auth) else {
        return Err(ApiError::Unauthorized(
            "Authorization header must use Bearer authentication".into(),
        ));
    };
    if token
        .as_bytes()
        .ct_eq(expected.expose_secret().as_bytes())
        .unwrap_u8()
        != 1
    {
        return Err(ApiError::Unauthorized("invalid admin bearer token".into()));
    }
    Ok(())
}

async fn patch_agent_overrides_handler(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Err(err) = ensure_admin_auth(&state.admin, &headers) {
        return err.into_response();
    }
    let service = match ConfigService::new(&state) {
        Ok(s) => s,
        Err(e) => return map_service_error(e).into_response(),
    };
    match service.patch_agent_overrides(&id, body, &headers).await {
        Ok(spec) => Json(spec).into_response(),
        Err(e) if is_overrides_not_supported_for_user_record(&e) => unprocessable_error(e),
        Err(e) => map_service_error(e).into_response(),
    }
}

async fn validate_agent_overrides_handler(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Err(err) = ensure_admin_auth(&state.admin, &headers) {
        return err.into_response();
    }
    let service = match ConfigService::new(&state) {
        Ok(s) => s,
        Err(e) => return map_service_error(e).into_response(),
    };
    match service.validate_agent_overrides(&id, body).await {
        Ok(spec) => Json(spec).into_response(),
        Err(e) if is_overrides_not_supported_for_user_record(&e) => unprocessable_error(e),
        Err(e) => map_service_error(e).into_response(),
    }
}

async fn clear_agent_overrides_handler(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = ensure_admin_auth(&state.admin, &headers) {
        return err.into_response();
    }
    let service = match ConfigService::new(&state) {
        Ok(s) => s,
        Err(e) => return map_service_error(e).into_response(),
    };
    match service.clear_agent_overrides(&id, &headers).await {
        Ok(spec) => Json(spec).into_response(),
        Err(e) if is_overrides_not_supported_for_user_record(&e) => unprocessable_error(e),
        Err(e) => map_service_error(e).into_response(),
    }
}

async fn clear_agent_override_field_handler(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path((id, field)): Path<(String, String)>,
) -> Response {
    if let Err(err) = ensure_admin_auth(&state.admin, &headers) {
        return err.into_response();
    }
    let service = match ConfigService::new(&state) {
        Ok(s) => s,
        Err(e) => return map_service_error(e).into_response(),
    };
    match service
        .clear_agent_override_field(&id, &field, &headers)
        .await
    {
        Ok(spec) => Json(spec).into_response(),
        Err(e) if is_overrides_not_supported_for_user_record(&e) => unprocessable_error(e),
        Err(e) => map_service_error(e).into_response(),
    }
}

async fn patch_tool_overrides_handler(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Err(err) = ensure_admin_auth(&state.admin, &headers) {
        return err.into_response();
    }
    let service = match ConfigService::new(&state) {
        Ok(s) => s,
        Err(e) => return map_service_error(e).into_response(),
    };
    match service.patch_tool_overrides(&id, body, &headers).await {
        Ok(spec) => Json(spec).into_response(),
        Err(e) if is_overrides_not_supported_for_user_record(&e) => unprocessable_error(e),
        Err(e) => map_service_error(e).into_response(),
    }
}

async fn clear_tool_overrides_handler(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = ensure_admin_auth(&state.admin, &headers) {
        return err.into_response();
    }
    let service = match ConfigService::new(&state) {
        Ok(s) => s,
        Err(e) => return map_service_error(e).into_response(),
    };
    match service.clear_tool_overrides(&id, &headers).await {
        Ok(spec) => Json(spec).into_response(),
        Err(e) if is_overrides_not_supported_for_user_record(&e) => unprocessable_error(e),
        Err(e) => map_service_error(e).into_response(),
    }
}

async fn clear_tool_override_field_handler(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path((id, field)): Path<(String, String)>,
) -> Response {
    if let Err(err) = ensure_admin_auth(&state.admin, &headers) {
        return err.into_response();
    }
    let service = match ConfigService::new(&state) {
        Ok(s) => s,
        Err(e) => return map_service_error(e).into_response(),
    };
    match service
        .clear_tool_override_field(&id, &field, &headers)
        .await
    {
        Ok(spec) => Json(spec).into_response(),
        Err(e) if is_overrides_not_supported_for_user_record(&e) => unprocessable_error(e),
        Err(e) => map_service_error(e).into_response(),
    }
}

fn unprocessable_error(error: ConfigServiceError) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({ "error": error.to_string() })),
    )
        .into_response()
}

fn map_service_error(error: ConfigServiceError) -> ApiError {
    match error {
        ConfigServiceError::NotEnabled | ConfigServiceError::InvalidPayload(_) => {
            ApiError::BadRequest(error.to_string())
        }
        ConfigServiceError::UnknownNamespace(_)
        | ConfigServiceError::NotFound(_)
        | ConfigServiceError::Storage(
            awaken_server_contract::contract::storage::StorageError::NotFound(_),
        ) => ApiError::NotFound(error.to_string()),
        ConfigServiceError::MissingId => ApiError::BadRequest(error.to_string()),
        ConfigServiceError::Conflict(_) => ApiError::Conflict(error.to_string()),
        ConfigServiceError::Serialization(_)
        | ConfigServiceError::Apply(_)
        | ConfigServiceError::Storage(_) => ApiError::Internal(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_server_contract::RedactedString;
    use axum::http::{HeaderMap, HeaderValue, header};
    #[test]
    fn admin_auth_rejects_when_token_not_configured() {
        let headers = HeaderMap::new();
        let err = ensure_admin_auth_for_token(None, &headers).unwrap_err();
        assert!(matches!(
            err,
            ApiError::Unauthorized(message) if message.contains("not configured")
        ));
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
    #[test]
    fn admin_auth_rejects_ambiguous_or_malformed_bearer_headers_without_leaking_secret() {
        let expected = RedactedString::from("secret-value-that-must-not-leak");
        let cases = [
            "Bearer",
            "Bearer ",
            "Bearer\tsecret-value-that-must-not-leak",
            "Token secret-value-that-must-not-leak",
            "bearer wrong-token",
            "Bearer secret-value-that-must-not-leak extra",
        ];
        for value in cases {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(value).expect("valid header value"),
            );
            let err = ensure_admin_auth_for_token(Some(&expected), &headers).unwrap_err();
            let ApiError::Unauthorized(message) = &err else {
                panic!("expected Unauthorized for {value:?}, got {err:?}");
            };
            assert!(
                !message.contains(expected.expose_secret()),
                "unauthorized error leaked secret for {value:?}: {message}"
            );
            let response = err.into_response();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "case {value:?}"
            );
        }

        let mut headers = HeaderMap::new();
        headers.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret-value-that-must-not-leak"),
        );
        headers.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer different"),
        );
        let err = ensure_admin_auth_for_token(Some(&expected), &headers).unwrap_err();
        let ApiError::Unauthorized(message) = &err else {
            panic!("expected Unauthorized for duplicate headers, got {err:?}");
        };
        assert!(
            !message.contains(expected.expose_secret()),
            "duplicate-header error leaked secret: {message}"
        );
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    // ── delete 409 / force integration tests ──────────────────────────────

    mod delete_integration {
        use crate::app::{ConfigModuleState, ServerConfig, ServerState};
        use crate::mailbox::{Mailbox, MailboxConfig};
        use crate::routes::build_router;
        use crate::services::config_runtime::{ConfigRuntimeManager, ProviderExecutorFactory};
        use async_trait::async_trait;
        use awaken_runtime::builder::AgentRuntimeBuilder;
        use awaken_server_contract::contract::executor::{
            InferenceExecutionError, InferenceRequest, LlmExecutor,
        };
        use awaken_server_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
        use awaken_server_contract::{
            AgentSpec, BuiltinSeedSet, BuiltinSpec, ModelSpec, ProviderSpec,
        };
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use http_body_util::BodyExt;
        use serde_json::Value;
        use std::sync::Arc;
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
        const TEST_ADMIN_TOKEN: &str = "test-admin-token";

        fn build_authorized_test_router(state: &ServerState) -> axum::Router {
            use axum::http::{HeaderValue, header};
            use axum::middleware::{self, Next};
            build_router(state).layer(middleware::from_fn(
                |mut req: Request<Body>, next: Next| async move {
                    req.headers_mut().insert(
                        header::AUTHORIZATION,
                        HeaderValue::from_static("Bearer test-admin-token"),
                    );
                    next.run(req).await
                },
            ))
        }
        async fn build_test_app() -> axum::Router {
            let config_store = Arc::new(awaken_stores::InMemoryStore::new());
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
            let resolver = runtime.resolver_arc();
            let mailbox = Arc::new(Mailbox::new(
                runtime.clone(),
                Arc::new(awaken_stores::InMemoryMailboxStore::new()),
                thread_store.clone(),
                "route-test".into(),
                MailboxConfig::default(),
            ));
            let mut state = ServerState::new(
                runtime,
                mailbox,
                thread_store,
                resolver,
                ServerConfig::default(),
            );
            state.config = Some(ConfigModuleState::new(config_store, manager));
            state.admin.admin_api_config.bearer_token = Some(TEST_ADMIN_TOKEN.into());
            build_authorized_test_router(&state)
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
        async fn validate_record(
            app: &axum::Router,
            namespace: &str,
            body: &str,
        ) -> (StatusCode, Value) {
            let req = Request::builder()
                .method("POST")
                .uri(format!("/v1/config/{namespace}/validate"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
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
        async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
            let req = Request::builder()
                .method("GET")
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
        async fn validate_returns_normalized_payload_without_persisting() {
            let app = build_test_app().await;
            let (status, body) =
                validate_record(&app, "providers", r#"{"id":"draft-prov","adapter":"stub"}"#).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["ok"], Value::Bool(true));
            assert_eq!(body["normalized"]["id"], Value::String("draft-prov".into()));
            assert!(
                body["normalized"].get("created_at").is_none(),
                "normalized spec should not inject runtime metadata fields"
            );
            // Confirm nothing was persisted: GET should 404.
            let get_req = Request::builder()
                .method("GET")
                .uri("/v1/config/providers/draft-prov")
                .body(Body::empty())
                .unwrap();
            let get_resp = app.clone().oneshot(get_req).await.unwrap();
            assert_eq!(get_resp.status(), StatusCode::NOT_FOUND);
        }
        #[tokio::test]
        async fn validate_rejects_missing_id() {
            let app = build_test_app().await;
            let (status, _) = validate_record(&app, "providers", r#"{"adapter":"stub"}"#).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
        #[tokio::test]
        async fn validate_rejects_unknown_provider_and_empty_model_fields() {
            let app = build_test_app().await;
            let (status, body) = validate_record(
                &app,
                "providers",
                r#"{"id":"draft-prov","adapter":"stub","future_top_level":true}"#,
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(body["error"].as_str().unwrap().contains("future_top_level"));
            let (status, body) = validate_record(
                &app,
                "models",
                r#"{"id":"draft-model","provider_id":"","upstream_model":"gpt-4"}"#,
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(body["error"].as_str().unwrap().contains("provider_id"));
        }
        #[tokio::test]
        async fn validate_agent_overrides_does_not_persist_patch() {
            let app = build_test_app().await;
            let req = Request::builder()
                .method("POST")
                .uri("/v1/config/agents/bootstrap/overrides")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"system_prompt":"patched"}"#))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["ok"], Value::Bool(true));
            assert_eq!(body["normalized"]["system_prompt"], "patched");
            let (status, meta) = get_json(&app, "/v1/config/agents/bootstrap/meta").await;
            assert_eq!(status, StatusCode::OK);
            assert!(meta["user_overrides"].is_null());
        }

        #[tokio::test]
        async fn validate_agent_overrides_rejects_unknown_fields() {
            let app = build_test_app().await;
            let req = Request::builder()
                .method("POST")
                .uri("/v1/config/agents/bootstrap/overrides")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"unknown_field":true}"#))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        }

        #[tokio::test]
        async fn provider_removal_preview_reports_model_and_agent_impact() {
            let app = build_test_app().await;

            assert_eq!(
                create_record(
                    &app,
                    "providers",
                    r#"{"id":"prov-preview","adapter":"stub"}"#
                )
                .await,
                StatusCode::CREATED
            );
            assert_eq!(
                create_record(
                    &app,
                    "models",
                    r#"{"id":"model-preview","provider_id":"prov-preview","upstream_model":"gpt-4"}"#
                )
                .await,
                StatusCode::CREATED
            );
            assert_eq!(
                create_record(
                    &app,
                    "agents",
                    r#"{"id":"agent-preview","model_id":"model-preview","system_prompt":"hi","max_rounds":1}"#
                )
                .await,
                StatusCode::CREATED
            );

            let (status, body) =
                get_json(&app, "/v1/config/providers/prov-preview/removal-preview").await;
            assert_eq!(status, StatusCode::OK, "body: {body}");
            assert_eq!(body["provider_id"], "prov-preview");
            assert_eq!(body["model_ids"], serde_json::json!(["model-preview"]));
            assert_eq!(body["agent_ids"], serde_json::json!(["agent-preview"]));
            assert_eq!(body["block_if_referenced_allowed"], false);
            assert_eq!(body["cascade_unused_models_allowed"], false);
        }

        #[tokio::test]
        async fn provider_removal_preview_returns_404_for_missing_provider() {
            let app = build_test_app().await;
            let (status, _) = get_json(&app, "/v1/config/providers/no-such/removal-preview").await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }

        #[tokio::test]
        async fn config_diagnostics_returns_serializable_registry_diagnostics() {
            let app = build_test_app().await;

            let (status, body) = get_json(&app, "/v1/config/diagnostics").await;
            assert_eq!(status, StatusCode::OK, "body: {body}");
            assert!(
                body["diagnostics"].as_array().is_some(),
                "diagnostics should be an array"
            );
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
        async fn delete_provider_with_force_true_cascades_unused_models() {
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
        async fn delete_model_with_force_true_still_reports_agent_blockers() {
            let app = build_test_app().await;

            let (status, body) = delete_record(&app, "models", "bootstrap", true).await;

            assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
            let used_by = body["used_by"].as_array().expect("used_by array");
            assert!(
                used_by
                    .iter()
                    .any(|record| record["namespace"] == "agents" && record["id"] == "bootstrap"),
                "should report the agent that keeps the model in use: {body}"
            );
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
            assert_eq!(
                body["network_tested"], false,
                "build-time failures do not reach the network"
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
                    .with_model(ModelSpec::new("bootstrap", "bootstrap", "bootstrap-model"))
                    .with_agent_spec(bootstrap_agent())
                    .with_in_memory_thread_run_store(thread_store.clone())
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
            awaken_server_contract::contract::config_store::ConfigStore::put(
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
            let mut state = ServerState::new(
                runtime,
                mailbox,
                thread_store,
                resolver,
                ServerConfig::default(),
            );
            state.config = Some(ConfigModuleState::new(config_store, manager));
            state.admin.admin_api_config.bearer_token = Some(TEST_ADMIN_TOKEN.into());
            let app = build_authorized_test_router(&state);

            let (status, body) = test_provider(&app, "prov-openai").await;
            assert_eq!(status, StatusCode::OK, "body: {body}");
            assert_eq!(body["ok"], true, "expected ok=true for openai adapter");
            assert_eq!(
                body["network_tested"], false,
                "bearer/env provider probe validates config only"
            );
            assert!(body.get("error").is_none(), "should have no error field");
        }

        #[tokio::test]
        async fn test_provider_missing_id_returns_404() {
            let app = build_test_app().await;
            let (status, _body) = test_provider(&app, "no-such-provider").await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }

        struct StubTool {
            id: String,
            desc: String,
        }
        #[async_trait]
        impl awaken_server_contract::contract::tool::Tool for StubTool {
            fn descriptor(&self) -> awaken_server_contract::contract::tool::ToolDescriptor {
                awaken_server_contract::contract::tool::ToolDescriptor::new(
                    self.id.clone(),
                    self.id.clone(),
                    self.desc.clone(),
                )
            }
            async fn execute(
                &self,
                _args: serde_json::Value,
                _ctx: &awaken_server_contract::contract::tool::ToolCallContext,
            ) -> Result<
                awaken_server_contract::contract::tool::ToolOutput,
                awaken_server_contract::contract::tool::ToolError,
            > {
                Ok(awaken_server_contract::contract::tool::ToolResult::success(
                    &self.id,
                    serde_json::json!({}),
                )
                .into())
            }
        }

        async fn build_test_app_with_tool(id: &str, description: &str) -> axum::Router {
            use awaken_server_contract::ToolSpec;

            let config_store = Arc::new(awaken_stores::InMemoryStore::new());
            let thread_store = Arc::new(awaken_stores::InMemoryStore::new());
            let runtime = Arc::new(
                AgentRuntimeBuilder::new()
                    .with_provider("bootstrap", Arc::new(ImmediateExecutor))
                    .with_tool(
                        id,
                        Arc::new(StubTool {
                            id: id.into(),
                            desc: description.into(),
                        }),
                    )
                    .with_in_memory_thread_run_store(thread_store.clone())
                    .build()
                    .expect("build runtime"),
            );

            let manager = Arc::new(
                ConfigRuntimeManager::new(runtime.clone(), config_store.clone())
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
                    BuiltinSpec::model(ModelSpec::new("bootstrap", "bootstrap", "bootstrap-model")),
                    BuiltinSpec::agent(bootstrap_agent()),
                    BuiltinSpec::tool(ToolSpec {
                        id: id.into(),
                        name: id.into(),
                        description: description.into(),
                        ..Default::default()
                    }),
                ],
            };
            manager.apply_seed(&seed).await.expect("apply_seed");
            manager.apply().await.expect("publish config");

            let resolver = runtime.resolver_arc();
            let mailbox = Arc::new(Mailbox::new(
                runtime.clone(),
                Arc::new(awaken_stores::InMemoryMailboxStore::new()),
                thread_store.clone(),
                "route-test-tool".into(),
                MailboxConfig::default(),
            ));
            let mut state = ServerState::new(
                runtime,
                mailbox,
                thread_store,
                resolver,
                ServerConfig::default(),
            );
            state.config = Some(ConfigModuleState::new(config_store, manager));
            state.admin.admin_api_config.bearer_token = Some(TEST_ADMIN_TOKEN.into());

            build_authorized_test_router(&state)
        }

        #[tokio::test]
        async fn patch_tool_overrides_route_applies_description() {
            let app = build_test_app_with_tool("echo", "stock").await;
            let req = Request::builder()
                .method("PATCH")
                .uri("/v1/config/tools/echo/overrides")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"description":"patched"}"#))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["description"], "patched");
        }

        #[tokio::test]
        async fn patch_tool_overrides_route_404_for_unknown_id() {
            let app = build_test_app().await;
            let req = Request::builder()
                .method("PATCH")
                .uri("/v1/config/tools/nope/overrides")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"description":"x"}"#))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        }

        #[tokio::test]
        async fn create_tool_route_returns_422() {
            let app = build_test_app().await;
            let req = Request::builder()
                .method("POST")
                .uri("/v1/config/tools")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"id":"x","name":"x","description":"x"}"#))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            // ConfigServiceError::InvalidPayload maps to 400 BAD_REQUEST.
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        }
    }
}
