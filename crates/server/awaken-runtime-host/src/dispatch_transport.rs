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
    AnyDispatchStore, Dispatch, DispatchOutcome, DispatchQueue, HttpDispatchQueue, PendingInput,
    RunClaim, RunDispatch, SubmitOptions, WorkerDirectory, WorkerHeartbeat, WorkerIdentity,
    WorkerRegistration,
};

use crate::dispatch_backend::shared_durable_store;
use crate::host::{HostError, SharedHost};
use crate::worker_http::respond;
use crate::worker_security::{
    FixedWorkerLeasePolicy, HeaderWorkerAuthenticator, SystemWorkerClock, VerifiedWorkerContext,
    WorkerClock, WorkerLeasePolicy, WorkerRequestAuthenticator, WorkerUpstream,
};

/// Build the database-less worker's dispatch store from the same authenticated
/// upstream configuration used by its commit clients.
pub fn worker_dispatch_store_with_upstream(upstream: &WorkerUpstream) -> Arc<AnyDispatchStore> {
    Arc::new(AnyDispatchStore::from_dispatch(Arc::new(
        HttpDispatchQueue::new(upstream.base_url())
            .with_client(upstream.client().clone())
            .with_worker_id(upstream.worker_id()),
    ) as Arc<dyn Dispatch>))
}

/// Explicit application service mounted by the worker HTTP adapter.
pub struct WorkerDispatchService {
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    clock: Arc<dyn WorkerClock>,
    lease_policy: Arc<dyn WorkerLeasePolicy>,
    directory: Option<Arc<dyn WorkerDirectory>>,
    registry_ttl_ms: u64,
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
            directory: None,
            registry_ttl_ms: 30_000,
        }
    }

    #[must_use]
    pub fn with_worker_directory(
        mut self,
        directory: Arc<dyn WorkerDirectory>,
        ttl_ms: u64,
    ) -> Self {
        self.directory = Some(directory);
        self.registry_ttl_ms = ttl_ms.max(1);
        self
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
pub fn dispatch_transport_router(host: Arc<SharedHost>) -> Router {
    let dispatch = shared_durable_store(host.store_dir.as_deref())
        .expect("worker dispatch router requires the durable backend initialized at startup");
    dispatch_transport_router_with_service(Arc::new(WorkerDispatchService::local(dispatch)))
}

pub fn dispatch_transport_router_with_directory(
    host: Arc<SharedHost>,
    directory: Arc<dyn WorkerDirectory>,
) -> Router {
    let dispatch = shared_durable_store(host.store_dir.as_deref())
        .expect("worker dispatch router requires the durable backend initialized at startup");
    dispatch_transport_router_with_service(Arc::new(
        WorkerDispatchService::local(dispatch).with_worker_directory(directory, 30_000),
    ))
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
        .route("/v1/worker/register", post(register_worker))
        .route("/v1/worker/heartbeat", post(heartbeat_worker))
        .route("/v1/worker/drain", post(drain_worker))
        .route("/v1/worker/quiesced", post(quiesce_worker))
        .route("/v1/worker/deregister", post(deregister_worker))
        .layer(axum::middleware::from_fn_with_state(
            service.clone(),
            authenticate_worker,
        ))
        .with_state(service)
}

fn directory(service: &WorkerDispatchService) -> Result<&Arc<dyn WorkerDirectory>, HostError> {
    service
        .directory
        .as_ref()
        .ok_or_else(|| HostError::internal("worker directory is not configured"))
}

fn verify_worker_id(worker: &VerifiedWorkerContext, worker_id: &str) -> Result<(), HostError> {
    if worker.worker_id() != worker_id {
        return Err(HostError::bad_request(
            "authenticated worker id does not match request identity",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
struct RegisterWorkerReq {
    registration: WorkerRegistration,
}

async fn register_worker(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<RegisterWorkerReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        verify_worker_id(&worker, &request.registration.worker_id)?;
        let record = directory(&service)?
            .register(
                request.registration,
                service.clock.now_ms(),
                service.registry_ttl_ms,
            )
            .await
            .map_err(|error| HostError::bad_request(error.to_string()))?;
        Ok(json!({ "worker": record }))
    }
    .await;
    respond(result)
}

#[derive(Deserialize)]
struct HeartbeatWorkerReq {
    identity: WorkerIdentity,
    heartbeat: WorkerHeartbeat,
}

async fn heartbeat_worker(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<HeartbeatWorkerReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        verify_worker_id(&worker, &request.identity.worker_id)?;
        let mutation = directory(&service)?
            .heartbeat(
                &request.identity,
                request.heartbeat,
                service.clock.now_ms(),
                service.registry_ttl_ms,
            )
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "mutation": mutation }))
    }
    .await;
    respond(result)
}

#[derive(Deserialize)]
struct WorkerIdentityReq {
    identity: WorkerIdentity,
    #[serde(default)]
    deadline_ms: Option<u64>,
}

async fn drain_worker(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<WorkerIdentityReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        verify_worker_id(&worker, &request.identity.worker_id)?;
        let deadline = request.deadline_ms.unwrap_or_else(|| {
            service
                .clock
                .now_ms()
                .saturating_add(service.registry_ttl_ms)
        });
        let mutation = directory(&service)?
            .begin_drain(&request.identity, deadline)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "mutation": mutation }))
    }
    .await;
    respond(result)
}

async fn quiesce_worker(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<WorkerIdentityReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        verify_worker_id(&worker, &request.identity.worker_id)?;
        let mutation = directory(&service)?
            .mark_quiesced(&request.identity)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "mutation": mutation }))
    }
    .await;
    respond(result)
}

async fn deregister_worker(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<WorkerIdentityReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        verify_worker_id(&worker, &request.identity.worker_id)?;
        let mutation = directory(&service)?
            .deregister(&request.identity)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "mutation": mutation }))
    }
    .await;
    respond(result)
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
