use awaken_server_contract::ScopeContext;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde_json::json;

use crate::app::SystemRoutesState;

pub(crate) fn system_routes() -> Router<SystemRoutesState> {
    Router::new()
        .route("/v1/system/info", get(system_info))
        .route("/v1/system/modules", get(system_modules))
}

#[tracing::instrument(skip(state))]
async fn system_info(
    State(state): State<SystemRoutesState>,
    Extension(scope): Extension<ScopeContext>,
    headers: HeaderMap,
) -> Response {
    if let Err(err) = crate::config_routes::ensure_admin_auth(&state.admin, &headers) {
        return err.into_response();
    }
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "scope_id": scope.scope_id.as_str(),
        "uptime_seconds": state.admin.started_at.elapsed().as_secs(),
        "config_store_enabled": state.config_store_enabled,
        "audit_log_enabled": state.audit_log_enabled,
        "runtime_stats_enabled": state.runtime_stats_enabled,
    }))
    .into_response()
}

#[tracing::instrument(skip(state))]
async fn system_modules(State(state): State<SystemRoutesState>, headers: HeaderMap) -> Response {
    if let Err(err) = crate::config_routes::ensure_admin_auth(&state.admin, &headers) {
        return err.into_response();
    }
    Json(json!({ "modules": state.mounted_modules })).into_response()
}
