//! Admin route groups with concrete module/service state.

use awaken_ext_observability::runtime_stats::parse_window_str;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::app::{AdminRunRoutesState, ConfigRoutesState};
use crate::error::ApiError;

pub(crate) fn admin_run_routes() -> Router<AdminRunRoutesState> {
    Router::new()
        .route("/v1/agents/:id/runtime-stats", get(get_agent_runtime_stats))
        .route("/v1/agents/runtime-stats", get(list_agents_runtime_stats))
}

pub(crate) fn config_admin_routes() -> Router<ConfigRoutesState> {
    // permission-preview is always registered so the handler returns 503
    // when the `permission` feature is absent (a 404 would be ambiguous
    // with "agent not found"). Matches `runtime-stats` / trace conventions.
    Router::new()
        .route(
            "/v1/agents/:id/permission-preview",
            get(get_agent_permission_preview),
        )
        .route(
            "/v1/admin/assistant/config",
            get(get_admin_assistant_config).put(put_admin_assistant_config),
        )
}

#[tracing::instrument(skip(state))]
async fn get_admin_assistant_config(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
) -> Response {
    if let Err(err) = crate::config_routes::ensure_admin_auth(&state.admin, &headers) {
        return err.into_response();
    }
    match crate::admin_assistant::load_config(&state.config).await {
        Ok(config) => Json(config).into_response(),
        Err(err) => ApiError::Internal(err.to_string()).into_response(),
    }
}

#[tracing::instrument(skip(state, body))]
async fn put_admin_assistant_config(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Json(body): Json<crate::admin_assistant::AdminAssistantConfig>,
) -> Response {
    if let Err(err) = crate::config_routes::ensure_admin_auth(&state.admin, &headers) {
        return err.into_response();
    }
    match crate::admin_assistant::save_config(&state.config, body).await {
        Ok(config) => Json(config).into_response(),
        Err(err) => ApiError::BadRequest(err.to_string()).into_response(),
    }
}

#[tracing::instrument(skip(state))]
async fn get_agent_permission_preview(
    State(state): State<ConfigRoutesState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = crate::config_routes::ensure_admin_auth(&state.admin, &headers) {
        return err.into_response();
    }
    #[cfg(feature = "permission")]
    {
        let _ = &id;
        match crate::services::permission_preview::preview_agent_permissions(&state, &id).await {
            Ok(preview) => Json(preview).into_response(),
            Err(err) => map_permission_preview_error(err).into_response(),
        }
    }
    #[cfg(not(feature = "permission"))]
    {
        let _ = (state, id);
        ApiError::ServiceUnavailable(
            "permission feature not compiled into this server build".to_string(),
        )
        .into_response()
    }
}

#[cfg(feature = "permission")]
fn map_permission_preview_error(
    err: crate::services::permission_preview::PermissionPreviewError,
) -> ApiError {
    use crate::services::permission_preview::PermissionPreviewError as PE;
    match err {
        PE::AgentNotFound(id) => ApiError::NotFound(format!("agent not found: {id}")),
        PE::InvalidSpec(msg) | PE::InvalidPermissionConfig { reason: msg, .. } => {
            ApiError::BadRequest(msg)
        }
        PE::RegistryUnavailable => ApiError::Internal("runtime registry unavailable".into()),
        PE::Config(err) => ApiError::Internal(err.to_string()),
    }
}

// ── Agent runtime-stats endpoints ───────────────────────────────────

/// Query params for `GET /v1/agents/:id/runtime-stats`.
#[derive(Debug, Deserialize, Default)]
struct RuntimeStatsQuery {
    /// Optional time window, e.g. `1h`, `24h`, `7d`, `3600s`, `90`.
    window: Option<String>,
}

/// `GET /v1/agents/:id/runtime-stats` — return the agent's rolling-window
/// snapshot, or 404 when the agent has not been seen by the registry, or
/// 503 when the registry is not configured on this server.
///
/// Accepts an optional `?window=` query parameter (e.g. `1h`, `24h`, `7d`)
/// to restrict the snapshot to a shorter sub-window of the registry's full
/// history.  An invalid format returns 400.
#[tracing::instrument(skip(state))]
async fn get_agent_runtime_stats(
    State(state): State<AdminRunRoutesState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<RuntimeStatsQuery>,
) -> Response {
    if let Err(err) = crate::config_routes::ensure_admin_auth(&state.admin, &headers) {
        return err.into_response();
    }
    let Some(registry) = state.run.runtime_stats.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "runtime_stats registry not configured" })),
        )
            .into_response();
    };

    let window = match params.window.as_deref() {
        None => None,
        Some(s) => match parse_window_str(s) {
            Ok(d) => Some(d),
            Err(msg) => {
                return (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response();
            }
        },
    };

    match registry.snapshot_for_window(&id, window) {
        Some(snapshot) => Json(snapshot).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("agent not found in runtime stats: {id}") })),
        )
            .into_response(),
    }
}

/// `GET /v1/agents/runtime-stats` — return one snapshot per known agent,
/// sorted by `agent_id`. Returns `{"agents":[...]}` (or 503 when the
/// registry is missing).
#[tracing::instrument(skip(state))]
async fn list_agents_runtime_stats(
    State(state): State<AdminRunRoutesState>,
    headers: HeaderMap,
) -> Response {
    if let Err(err) = crate::config_routes::ensure_admin_auth(&state.admin, &headers) {
        return err.into_response();
    }
    let Some(registry) = state.run.runtime_stats.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "runtime_stats registry not configured" })),
        )
            .into_response();
    };
    let snapshots: Vec<_> = registry
        .known_agents()
        .into_iter()
        .filter_map(|id| registry.snapshot_for(&id))
        .collect();
    Json(json!({ "agents": snapshots })).into_response()
}
