// @formal surface requirement=protocol.adapters.adapter_projection
// @formal reason: Native scope/path/error projection only; Session application and Dispatch own all reads and admission evidence.
//! Native-only operational read over the Session application's narrow port.
use crate::common::{ExtensionRejection, RequiredWorkspaceScope, extension_rejection};
use awaken_session_application_contract::SessionRunDispatchObservation;
use awaken_session_application_contract::SessionRunDispatchQueries;
use awaken_session_types::RunErrorClassification;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use std::sync::Arc;

pub fn session_run_dispatch_router(reads: Arc<dyn SessionRunDispatchQueries>) -> Router {
    Router::new()
        .route("/v1/awaken/sessions/{id}/runs/{run}/dispatch", get(observe))
        .with_state(reads)
}
async fn observe(
    State(reads): State<Arc<dyn SessionRunDispatchQueries>>,
    RequiredWorkspaceScope(owner): RequiredWorkspaceScope,
    Path((session, run)): Path<(String, String)>,
) -> Result<Json<SessionRunDispatchObservation>, ExtensionRejection> {
    reads
        .observe_for_owner(
            &owner,
            &awaken_session_types::SessionId::from(session),
            &awaken_agent_contract::agent::run::Id(run),
        )
        .await
        .map(Json)
        .map_err(|error| {
            let status = match error.classification() {
                RunErrorClassification::NotFound => StatusCode::NOT_FOUND,
                RunErrorClassification::InvalidRequest
                | RunErrorClassification::UnsupportedOutputModes => StatusCode::BAD_REQUEST,
                RunErrorClassification::Conflict => StatusCode::CONFLICT,
                RunErrorClassification::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            extension_rejection(
                status,
                if status == StatusCode::NOT_FOUND {
                    "not_found_error"
                } else if status.is_client_error() {
                    "invalid_request_error"
                } else {
                    "api_error"
                },
                error.to_string(),
            )
        })
}
