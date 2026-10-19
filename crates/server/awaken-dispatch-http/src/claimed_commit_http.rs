// @formal entrypoint ingress.commit_epoch_atomic_remote_custom
// @formal evidence session_run.stale_work_epoch_cannot_observe_authoritative_thread_commit
//! Authenticated Coordinator HTTP adapter for claim-fenced Worker commits.

use std::sync::Arc;

use awaken_dispatch::{ClaimedCommitRequest, ClaimedCommitService, WorkerDirectory};
use awaken_dispatch_contract::MAX_CLAIMED_COMMIT_BODY_BYTES;
use awaken_worker_transport_security::{
    VerifiedWorkerContext, WorkerRequestAuthenticator, authenticate_worker_request,
    verify_current_worker_identity,
};
use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::StatusCode;
use axum::{Json, Router};
use serde_json::{Value, json};

/// Mount the sole registered-Worker claimed-commit HTTP surface.
pub struct ClaimedCommitHttpService {
    application: Arc<ClaimedCommitService>,
    directory: Arc<dyn WorkerDirectory>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    /// The Coordinator's wall clock: a Worker registration's currency is
    /// judged on it.
    clock: Arc<dyn awaken_time::WallClock>,
}

impl ClaimedCommitHttpService {
    #[must_use]
    pub fn new(
        application: Arc<ClaimedCommitService>,
        directory: Arc<dyn WorkerDirectory>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            application,
            directory,
            authenticator,
            clock,
        }
    }
}

pub fn claimed_commit_router(service: Arc<ClaimedCommitHttpService>) -> Router {
    Router::new()
        .route(
            "/v1/worker/commit-claimed",
            axum::routing::post(commit_claimed),
        )
        .layer(DefaultBodyLimit::max(MAX_CLAIMED_COMMIT_BODY_BYTES))
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
        awaken_time::WallClockExt::now_ms(service.clock.as_ref()),
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
        Err(error) => crate::worker_dispatch::respond(Err(error)),
    }
}
