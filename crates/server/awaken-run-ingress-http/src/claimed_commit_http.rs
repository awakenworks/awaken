//! Authenticated Coordinator HTTP adapter for claim-fenced Worker commits.

use std::sync::Arc;

use awaken_run_ingress::{
    ApplicationErrorKind, ClaimedCommitRequest, ClaimedCommitService, WorkerDirectory,
};
use awaken_worker_transport_security::{
    VerifiedWorkerContext, WorkerRequestAuthenticator, authenticate_worker_request,
    verify_current_worker_identity,
};
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::{Json, Router};
use serde_json::{Value, json};

/// Mount the sole registered-Worker claimed-commit HTTP surface.
pub struct ClaimedCommitHttpService {
    application: Arc<ClaimedCommitService>,
    directory: Arc<dyn WorkerDirectory>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
}

impl ClaimedCommitHttpService {
    #[must_use]
    pub fn new(
        application: Arc<ClaimedCommitService>,
        directory: Arc<dyn WorkerDirectory>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
    ) -> Self {
        Self {
            application,
            directory,
            authenticator,
        }
    }
}

pub fn claimed_commit_router(service: Arc<ClaimedCommitHttpService>) -> Router {
    Router::new()
        .route(
            "/v1/worker/commit-claimed",
            axum::routing::post(commit_claimed),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            service.authenticator.clone(),
            authenticate_worker_request,
        ))
        .with_state(service)
}

async fn commit_claimed(
    State(service): State<Arc<ClaimedCommitHttpService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<ClaimedCommitRequest>,
) -> (StatusCode, Json<Value>) {
    let identity = &request.identity;
    if verify_current_worker_identity(
        service.directory.as_ref(),
        &worker,
        identity,
        unix_now_ms(),
        false,
    )
    .await
    .is_err()
        || request.claim.owner != identity.lease_owner()
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "worker incarnation does not own the claim" })),
        );
    }
    match service.application.apply_claimed(request).await {
        Ok(receipt) => (
            StatusCode::OK,
            Json(serde_json::to_value(receipt).expect("CommitReceipt serializes")),
        ),
        Err(error) => {
            let status = match error.kind {
                ApplicationErrorKind::InvalidRequest => StatusCode::BAD_REQUEST,
                ApplicationErrorKind::Conflict => StatusCode::CONFLICT,
                ApplicationErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (status, Json(json!({ "error": error.message })))
        }
    }
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}
