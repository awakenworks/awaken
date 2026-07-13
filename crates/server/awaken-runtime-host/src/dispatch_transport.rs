//! The worker-facing dispatch transport (cross-node, database-less worker).
//!
//! Exposes the `DispatchQueue` claim/settle surface over HTTP so a remote worker
//! that never opens the store can claim runs, renew leases, and settle them. The
//! server stays the store's owner; the shared store's atomic claim gives the same
//! single-owner-per-run guarantee whether the claimer is the co-located pool or a
//! remote worker over this transport. It is the control-plane half of the cell's
//! worker seam (the write-plane half is the commit ingest).
//!
//! Only the worker-needed verbs are exposed — `enqueue`, `claim`, `renew`,
//! `renew_owned`, `settle`. The operational verbs (reap, dead-letter, purge,
//! supersede, list) stay server-local on `durable_ops_router`; a worker never runs
//! them.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_run_ingress::{DispatchOutcome, DispatchQueue, RunExecutionRequest, SubmitOptions};

use crate::dispatch_backend::shared_durable_store;
use crate::host::{HostError, HostErrorKind, SharedHost};

/// The worker-facing dispatch transport router. Mount it alongside
/// `durable_ops_router` on a cell server; a database-less worker points its
/// `HttpDispatchQueue` at these routes.
pub fn dispatch_transport_router(host: Arc<SharedHost>) -> Router {
    Router::new()
        .route("/v1/worker/dispatch/enqueue", post(enqueue))
        .route("/v1/worker/dispatch/claim", post(claim))
        .route("/v1/worker/dispatch/renew", post(renew))
        .route("/v1/worker/dispatch/renew_owned", post(renew_owned))
        .route("/v1/worker/dispatch/settle", post(settle))
        .with_state(host)
}

fn respond(result: Result<Value, HostError>) -> (StatusCode, Json<Value>) {
    match result {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err(error) => {
            let status = match error.kind {
                HostErrorKind::BadRequest => StatusCode::BAD_REQUEST,
                HostErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (status, Json(json!({ "error": error.message })))
        }
    }
}

/// The one process-shared durable store the co-located pool also drains, so a
/// remote claim and a local claim contend on the same atomic queue.
fn store() -> Result<Arc<awaken_run_ingress::AnyDispatchStore>, HostError> {
    shared_durable_store(None)
}

#[derive(Deserialize)]
struct EnqueueReq {
    request: RunExecutionRequest,
    #[serde(default)]
    options: Option<SubmitOptions>,
}

async fn enqueue(
    State(_host): State<Arc<SharedHost>>,
    Json(req): Json<EnqueueReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        store()?
            .enqueue_with(req.request, req.options.unwrap_or_default())
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(json!({ "enqueued": true }))
    }
    .await;
    respond(result)
}

#[derive(Deserialize)]
struct ClaimReq {
    owner: String,
    lease_ms: u64,
    now_ms: u64,
}

async fn claim(
    State(_host): State<Arc<SharedHost>>,
    Json(req): Json<ClaimReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let claimed = store()?
            .claim(&req.owner, req.lease_ms, req.now_ms)
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        // `Claimed` serializes (self-contained request + lease + pending); `None`
        // means nothing runnable, so the worker backs off and polls again.
        Ok(json!({ "claimed": claimed }))
    }
    .await;
    respond(result)
}

#[derive(Deserialize)]
struct RenewReq {
    run_id: String,
    owner: String,
    lease_ms: u64,
    now_ms: u64,
}

async fn renew(
    State(_host): State<Arc<SharedHost>>,
    Json(req): Json<RenewReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let renewed = store()?
            .renew_lease(&RunId(req.run_id), &req.owner, req.lease_ms, req.now_ms)
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        // `false` = the lease was lost (stolen/settled/unknown); the worker stops.
        Ok(json!({ "renewed": renewed }))
    }
    .await;
    respond(result)
}

#[derive(Deserialize)]
struct RenewOwnedReq {
    owner: String,
    lease_ms: u64,
    now_ms: u64,
}

async fn renew_owned(
    State(_host): State<Arc<SharedHost>>,
    Json(req): Json<RenewOwnedReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let renewed = store()?
            .renew_owned_leases(&req.owner, req.lease_ms, req.now_ms)
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(json!({ "renewed": renewed }))
    }
    .await;
    respond(result)
}

#[derive(Deserialize)]
struct SettleReq {
    run_id: String,
    outcome: DispatchOutcome,
    #[serde(default)]
    consumed: Vec<String>,
}

async fn settle(
    State(_host): State<Arc<SharedHost>>,
    Json(req): Json<SettleReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        store()?
            .settle(&RunId(req.run_id), req.outcome, &req.consumed)
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(json!({ "settled": true }))
    }
    .await;
    respond(result)
}
