//! Authenticated worker-facing dispatch transport.
//!
//! Wire callers submit only business data. Worker ownership, time, and lease
//! duration are derived from trusted server-side ports before the neutral
//! `DispatchQueue` is called.

use std::sync::Arc;

use axum::extract::{Extension, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_run_ingress::{
    DispatchOutcome, DispatchQueue, PendingInput, RunClaim, RunDispatch, SubmitOptions,
};

use crate::dispatch_backend::shared_durable_store;
use crate::host::{HostError, SharedHost};
use crate::worker_http::respond;
use crate::worker_security::{
    FixedWorkerLeasePolicy, HeaderWorkerAuthenticator, SystemWorkerClock, VerifiedWorkerContext,
    WorkerClock, WorkerLeasePolicy, WorkerRequestAuthenticator,
};

/// Explicit application service mounted by the worker HTTP adapter.
pub struct WorkerDispatchService {
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    clock: Arc<dyn WorkerClock>,
    lease_policy: Arc<dyn WorkerLeasePolicy>,
}

impl WorkerDispatchService {
    #[must_use]
    pub fn new(
        dispatch: Arc<dyn DispatchQueue>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
        clock: Arc<dyn WorkerClock>,
        lease_policy: Arc<dyn WorkerLeasePolicy>,
    ) -> Self {
        Self {
            dispatch,
            authenticator,
            clock,
            lease_policy,
        }
    }

    /// Local/test composition. Managed deployments should use [`Self::new`] with
    /// their WorkerLease/mTLS authenticator.
    #[must_use]
    pub fn local(dispatch: Arc<dyn DispatchQueue>) -> Self {
        Self::new(
            dispatch,
            Arc::new(HeaderWorkerAuthenticator),
            Arc::new(SystemWorkerClock),
            Arc::new(FixedWorkerLeasePolicy::default()),
        )
    }

    fn authority<'a>(&self, worker: &'a VerifiedWorkerContext) -> (&'a str, u64, u64) {
        (
            worker.worker_id(),
            self.lease_policy.lease_ms(worker),
            self.clock.now_ms(),
        )
    }
}

/// Compatibility facade used by existing composition roots. The host parameter
/// is retained for source compatibility; new compositions inject an explicit
/// [`WorkerDispatchService`] through [`dispatch_transport_router_with_service`].
pub fn dispatch_transport_router(_host: Arc<SharedHost>) -> Router {
    let dispatch = shared_durable_store(None)
        .expect("worker dispatch router requires the durable backend initialized at startup");
    dispatch_transport_router_with_service(Arc::new(WorkerDispatchService::local(dispatch)))
}

pub fn dispatch_transport_router_with_service(service: Arc<WorkerDispatchService>) -> Router {
    Router::new()
        .route("/v1/worker/dispatch/enqueue", post(enqueue))
        .route("/v1/worker/dispatch/claim_new_run", post(claim_new_run))
        .route(
            "/v1/worker/dispatch/deliver_and_claim",
            post(deliver_and_claim),
        )
        .route("/v1/worker/dispatch/claim", post(claim))
        .route("/v1/worker/dispatch/claim_run", post(claim_run))
        .route("/v1/worker/dispatch/renew", post(renew))
        .route("/v1/worker/dispatch/renew_owned", post(renew_owned))
        .route("/v1/worker/dispatch/settle", post(settle))
        .layer(axum::middleware::from_fn_with_state(
            service.clone(),
            authenticate_worker,
        ))
        .with_state(service)
}

async fn authenticate_worker(
    State(service): State<Arc<WorkerDispatchService>>,
    request: Request,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    match service.authenticator.authenticate(&parts).await {
        Ok(worker) => {
            let mut request = Request::from_parts(parts, body);
            request.extensions_mut().insert(worker);
            next.run(request).await
        }
        Err(error) => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct EnqueueReq {
    request: RunDispatch,
    #[serde(default)]
    options: Option<SubmitOptions>,
}

async fn enqueue(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(_worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<EnqueueReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        service
            .dispatch
            .enqueue_with(request.request, request.options.unwrap_or_default())
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "enqueued": true }))
    }
    .await;
    respond(result)
}

#[derive(Deserialize)]
struct ClaimNewRunReq {
    request: RunDispatch,
}

async fn claim_new_run(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<ClaimNewRunReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let (owner, lease_ms, now_ms) = service.authority(&worker);
        let claimed = service
            .dispatch
            .claim_new_run(request.request, owner, lease_ms, now_ms)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "claimed": claimed }))
    }
    .await;
    respond(result)
}

#[derive(Deserialize)]
struct DeliverAndClaimReq {
    input: PendingInput,
}

async fn deliver_and_claim(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<DeliverAndClaimReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let (owner, lease_ms, now_ms) = service.authority(&worker);
        let claimed = service
            .dispatch
            .deliver_and_claim(request.input, owner, lease_ms, now_ms)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "claimed": claimed }))
    }
    .await;
    respond(result)
}

async fn claim(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let (owner, lease_ms, now_ms) = service.authority(&worker);
        let claimed = service
            .dispatch
            .claim(owner, lease_ms, now_ms)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "claimed": claimed }))
    }
    .await;
    respond(result)
}

#[derive(Deserialize)]
struct ClaimRunReq {
    run_id: String,
}

async fn claim_run(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<ClaimRunReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let (owner, lease_ms, now_ms) = service.authority(&worker);
        let claimed = service
            .dispatch
            .claim_run(&RunId(request.run_id), owner, lease_ms, now_ms)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "claimed": claimed }))
    }
    .await;
    respond(result)
}

#[derive(Deserialize)]
struct RenewReq {
    run_id: String,
}

async fn renew(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<RenewReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let (owner, lease_ms, now_ms) = service.authority(&worker);
        let renewed = service
            .dispatch
            .renew_lease(&RunId(request.run_id), owner, lease_ms, now_ms)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "renewed": renewed }))
    }
    .await;
    respond(result)
}

async fn renew_owned(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let (owner, lease_ms, now_ms) = service.authority(&worker);
        let renewed = service
            .dispatch
            .renew_owned_leases(owner, lease_ms, now_ms)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "renewed": renewed }))
    }
    .await;
    respond(result)
}

#[derive(Deserialize)]
struct SettleReq {
    run_id: String,
    #[serde(default)]
    epoch: u64,
    outcome: DispatchOutcome,
    #[serde(default)]
    consumed: Vec<String>,
}

async fn settle(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SettleReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        // Bind the epoch to the authenticated owner before settling. Dropping the
        // guard before `settle` is safe: any intervening re-owner increments epoch,
        // causing the subsequent settle to be fenced.
        let claim = RunClaim {
            run_id: RunId(request.run_id.clone()),
            owner: worker.worker_id().to_string(),
            epoch: request.epoch,
        };
        let authorized = service
            .dispatch
            .lock_commit_epoch(&claim)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        let Some(guard) = authorized else {
            return Ok(json!({ "settled": false }));
        };
        drop(guard);
        let outcome = service
            .dispatch
            .settle(
                &claim.run_id,
                claim.epoch,
                request.outcome,
                &request.consumed,
            )
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "settled": outcome.applied() }))
    }
    .await;
    respond(result)
}
