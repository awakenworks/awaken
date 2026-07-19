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
use awaken_agent_contract::stream::checkpoint::{StreamCheckpoint, StreamCheckpointStore};
use awaken_run_ingress::{
    AnyDispatchStore, Dispatch, DispatchOutcome, DispatchQueue, HttpDispatchQueue, PendingInput,
    PlacementPolicy, RunClaim, RunDispatch, SubmitOptions, WorkerDirectory, WorkerHeartbeat,
    WorkerIdentity, WorkerRegistration, WorkerSnapshot, WorkerState,
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
pub fn worker_dispatch_store_with_upstream(
    upstream: &WorkerUpstream,
    identity: WorkerIdentity,
) -> Arc<AnyDispatchStore> {
    Arc::new(AnyDispatchStore::from_dispatch(Arc::new(
        HttpDispatchQueue::new(upstream.base_url())
            .with_client(upstream.client().clone())
            .with_worker_id(upstream.worker_id())
            .with_worker_identity(identity),
    ) as Arc<dyn Dispatch>))
}

/// Explicit application service mounted by the worker HTTP adapter.
pub struct WorkerDispatchService {
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    clock: Arc<dyn WorkerClock>,
    lease_policy: Arc<dyn WorkerLeasePolicy>,
    directory: Option<Arc<dyn WorkerDirectory>>,
    placement_policy: Option<Arc<dyn PlacementPolicy>>,
    registry_ttl_ms: u64,
    checkpoint: Option<Arc<dyn StreamCheckpointStore>>,
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
            placement_policy: None,
            registry_ttl_ms: 30_000,
            checkpoint: None,
        }
    }

    #[must_use]
    pub fn with_checkpoint_store(mut self, checkpoint: Arc<dyn StreamCheckpointStore>) -> Self {
        self.checkpoint = Some(checkpoint);
        self
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

    /// Install the preference policy used for ordinary pull claims. Exact
    /// parent-mediated claims remain explicit bindings and still pass the same
    /// immutable compatibility/recovery kernel.
    #[must_use]
    pub fn with_placement_policy(mut self, policy: Arc<dyn PlacementPolicy>) -> Self {
        self.placement_policy = Some(policy);
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
}

struct ClaimAuthority {
    owner: String,
    lease_ms: u64,
    now_ms: u64,
    snapshot: Option<WorkerSnapshot>,
}

async fn claim_authority(
    service: &WorkerDispatchService,
    worker: &VerifiedWorkerContext,
    identity: Option<&WorkerIdentity>,
    require_ready: bool,
) -> Result<ClaimAuthority, HostError> {
    let now_ms = service.clock.now_ms();
    let lease_ms = service.lease_policy.lease_ms(worker);
    let Some(directory) = &service.directory else {
        return Ok(ClaimAuthority {
            owner: worker.worker_id().to_string(),
            lease_ms,
            now_ms,
            snapshot: None,
        });
    };
    let identity = identity.ok_or_else(|| {
        HostError::bad_request("registered worker identity is required for dispatch authority")
    })?;
    verify_worker_id(worker, &identity.worker_id)?;
    let record = directory
        .current(&identity.worker_id)
        .await
        .map_err(|error| HostError::internal(error.to_string()))?
        .ok_or_else(|| HostError::bad_request("worker is not registered"))?;
    if &record.snapshot.identity != identity {
        return Err(HostError::bad_request("worker incarnation is stale"));
    }
    if (require_ready && record.snapshot.state != WorkerState::Ready)
        || record.snapshot.expires_at_ms <= now_ms
        || record.snapshot.state == WorkerState::Dead
    {
        return Err(HostError::bad_request(
            "worker is not ready or its registry lease expired",
        ));
    }
    Ok(ClaimAuthority {
        owner: identity.lease_owner(),
        lease_ms,
        now_ms,
        snapshot: Some(record.snapshot),
    })
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
    dispatch_transport_router_with_directory_and_policy(
        host,
        directory,
        Arc::new(awaken_run_ingress::LeastLoadedPolicy),
    )
}

pub fn dispatch_transport_router_with_directory_and_policy(
    host: Arc<SharedHost>,
    directory: Arc<dyn WorkerDirectory>,
    policy: Arc<dyn PlacementPolicy>,
) -> Router {
    let dispatch = shared_durable_store(host.store_dir.as_deref())
        .expect("worker dispatch router requires the durable backend initialized at startup");
    let checkpoint: Arc<dyn StreamCheckpointStore> = if let Some(root) = &host.store_dir {
        Arc::new(
            awaken_store_fs::FsStreamCheckpointStore::open(root.join("worker-stream-checkpoints"))
                .expect("worker checkpoint store must open"),
        )
    } else {
        Arc::new(awaken_runtime::memory::MemoryStreamCheckpointStore::new())
    };
    dispatch_transport_router_with_service(Arc::new(
        WorkerDispatchService::local(dispatch)
            .with_worker_directory(directory, 30_000)
            .with_placement_policy(policy)
            .with_checkpoint_store(checkpoint),
    ))
}

/// Production worker transport: lifecycle/dispatch and atomic claimed commit are
/// mounted over the same durable queue, authenticator, and incarnation directory.
/// Keeping this as one composition entry prevents a server from pairing registered
/// claims with the legacy unregistered commit route.
pub fn registered_worker_transport_router(
    host: Arc<SharedHost>,
    directory: Arc<dyn WorkerDirectory>,
    policy: Arc<dyn PlacementPolicy>,
) -> Router {
    let dispatch = shared_durable_store(host.store_dir.as_deref())
        .expect("registered worker transport requires the durable backend at startup");
    let dispatch_router = dispatch_transport_router_with_directory_and_policy(
        host.clone(),
        directory.clone(),
        policy,
    );
    let commit_router = crate::commit_ingest::claimed_commit_ingest_router_with_directory(
        host,
        dispatch as Arc<dyn DispatchQueue>,
        Arc::new(HeaderWorkerAuthenticator),
        directory,
    );
    dispatch_router.merge(commit_router)
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
        .route("/v1/worker/dispatch/bind_sandbox", post(bind_sandbox))
        .route("/v1/worker/dispatch/settle", post(settle))
        .route("/v1/worker/register", post(register_worker))
        .route("/v1/worker/heartbeat", post(heartbeat_worker))
        .route("/v1/worker/drain", post(drain_worker))
        .route("/v1/worker/quiesced", post(quiesce_worker))
        .route("/v1/worker/deregister", post(deregister_worker))
        .route("/v1/worker/checkpoint/get", post(get_checkpoint))
        .route("/v1/worker/checkpoint/put", post(put_checkpoint))
        .route("/v1/worker/checkpoint/delete", post(delete_checkpoint))
        .layer(axum::middleware::from_fn_with_state(
            service.clone(),
            authenticate_worker,
        ))
        .with_state(service)
}

fn checkpoint_store(
    service: &WorkerDispatchService,
) -> Result<&Arc<dyn StreamCheckpointStore>, HostError> {
    service
        .checkpoint
        .as_ref()
        .ok_or_else(|| HostError::internal("worker checkpoint store is not configured"))
}

#[derive(Deserialize)]
struct CheckpointReq {
    claim: RunClaim,
    #[serde(default)]
    identity: Option<WorkerIdentity>,
    #[serde(default)]
    checkpoint: Option<StreamCheckpoint>,
}

#[derive(Deserialize)]
struct BindSandboxReq {
    claim: RunClaim,
    #[serde(default)]
    identity: Option<WorkerIdentity>,
    sandbox_ref: String,
}

async fn bind_sandbox(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<BindSandboxReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let authority =
            claim_authority(&service, &worker, request.identity.as_ref(), false).await?;
        if request.claim.owner != authority.owner {
            return Err(HostError::bad_request("sandbox claim owner is stale"));
        }
        if request.sandbox_ref.is_empty() {
            return Err(HostError::bad_request("sandbox reference is required"));
        }
        let outcome = service
            .dispatch
            .bind_sandbox(&request.claim, &request.sandbox_ref)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "applied": outcome.applied() }))
    }
    .await;
    respond(result)
}

async fn checkpoint_authority(
    service: &WorkerDispatchService,
    worker: &VerifiedWorkerContext,
    request: &CheckpointReq,
) -> Result<(), HostError> {
    let authority = claim_authority(service, worker, request.identity.as_ref(), false).await?;
    if request.claim.owner != authority.owner {
        return Err(HostError::bad_request("checkpoint claim owner is stale"));
    }
    Ok(())
}

async fn get_checkpoint(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<CheckpointReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        checkpoint_authority(&service, &worker, &request).await?;
        let Some(_guard) = service
            .dispatch
            .lock_commit_epoch(&request.claim)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?
        else {
            return Ok(json!({ "checkpoint": null }));
        };
        let checkpoint = checkpoint_store(&service)?
            .get(&request.claim.run_id.0)
            .await;
        Ok(json!({ "checkpoint": checkpoint }))
    }
    .await;
    respond(result)
}

async fn put_checkpoint(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<CheckpointReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        checkpoint_authority(&service, &worker, &request).await?;
        let checkpoint = request
            .checkpoint
            .ok_or_else(|| HostError::bad_request("checkpoint payload is required"))?;
        if checkpoint.run_id != request.claim.run_id.0 {
            return Err(HostError::bad_request(
                "checkpoint run id does not match claim",
            ));
        }
        let Some(_guard) = service
            .dispatch
            .lock_commit_epoch(&request.claim)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?
        else {
            return Ok(json!({ "applied": false }));
        };
        checkpoint_store(&service)?.put(checkpoint).await;
        Ok(json!({ "applied": true }))
    }
    .await;
    respond(result)
}

async fn delete_checkpoint(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<CheckpointReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        checkpoint_authority(&service, &worker, &request).await?;
        let Some(_guard) = service
            .dispatch
            .lock_commit_epoch(&request.claim)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?
        else {
            return Ok(json!({ "applied": false }));
        };
        checkpoint_store(&service)?
            .delete(&request.claim.run_id.0)
            .await;
        Ok(json!({ "applied": true }))
    }
    .await;
    respond(result)
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
    #[serde(default)]
    identity: Option<WorkerIdentity>,
}

async fn claim_new_run(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<ClaimNewRunReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let authority = claim_authority(&service, &worker, request.identity.as_ref(), true).await?;
        let claimed = if let Some(snapshot) = &authority.snapshot {
            service
                .dispatch
                .claim_new_run_compatible(
                    request.request,
                    snapshot,
                    authority.lease_ms,
                    authority.now_ms,
                )
                .await
        } else {
            service
                .dispatch
                .claim_new_run(
                    request.request,
                    &authority.owner,
                    authority.lease_ms,
                    authority.now_ms,
                )
                .await
        }
        .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "claimed": claimed }))
    }
    .await;
    respond(result)
}

#[derive(Deserialize)]
struct DeliverAndClaimReq {
    input: PendingInput,
    #[serde(default)]
    identity: Option<WorkerIdentity>,
}

async fn deliver_and_claim(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<DeliverAndClaimReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let authority = claim_authority(&service, &worker, request.identity.as_ref(), true).await?;
        let claimed = if let Some(snapshot) = &authority.snapshot {
            service
                .dispatch
                .deliver_and_claim_compatible(
                    request.input,
                    snapshot,
                    authority.lease_ms,
                    authority.now_ms,
                )
                .await
        } else {
            service
                .dispatch
                .deliver_and_claim(
                    request.input,
                    &authority.owner,
                    authority.lease_ms,
                    authority.now_ms,
                )
                .await
        }
        .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "claimed": claimed }))
    }
    .await;
    respond(result)
}

async fn claim(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<ClaimWorkerReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let authority = claim_authority(&service, &worker, request.identity.as_ref(), true).await?;
        let claimed = if let Some(snapshot) = &authority.snapshot {
            if let Some(policy) = &service.placement_policy {
                let workers = directory(&service)?
                    .list()
                    .await
                    .map_err(|error| HostError::internal(error.to_string()))?
                    .into_iter()
                    .map(|record| record.snapshot)
                    .collect();
                service
                    .dispatch
                    .claim_placed(
                        snapshot,
                        workers,
                        policy.clone(),
                        authority.lease_ms,
                        authority.now_ms,
                    )
                    .await
            } else {
                service
                    .dispatch
                    .claim_compatible(snapshot, authority.lease_ms, authority.now_ms)
                    .await
            }
        } else {
            service
                .dispatch
                .claim(&authority.owner, authority.lease_ms, authority.now_ms)
                .await
        }
        .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "claimed": claimed }))
    }
    .await;
    respond(result)
}

#[derive(Default, Deserialize)]
struct ClaimWorkerReq {
    #[serde(default)]
    identity: Option<WorkerIdentity>,
}

#[derive(Deserialize)]
struct ClaimRunReq {
    run_id: String,
    #[serde(default)]
    identity: Option<WorkerIdentity>,
}

async fn claim_run(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<ClaimRunReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let authority = claim_authority(&service, &worker, request.identity.as_ref(), true).await?;
        let run_id = RunId(request.run_id);
        let claimed = if let Some(snapshot) = &authority.snapshot {
            service
                .dispatch
                .claim_run_compatible(&run_id, snapshot, authority.lease_ms, authority.now_ms)
                .await
        } else {
            service
                .dispatch
                .claim_run(
                    &run_id,
                    &authority.owner,
                    authority.lease_ms,
                    authority.now_ms,
                )
                .await
        }
        .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "claimed": claimed }))
    }
    .await;
    respond(result)
}

#[derive(Deserialize)]
struct RenewReq {
    run_id: String,
    #[serde(default)]
    identity: Option<WorkerIdentity>,
}

async fn renew(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<RenewReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let authority =
            claim_authority(&service, &worker, request.identity.as_ref(), false).await?;
        let renewed = service
            .dispatch
            .renew_lease(
                &RunId(request.run_id),
                &authority.owner,
                authority.lease_ms,
                authority.now_ms,
            )
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
    Json(request): Json<ClaimWorkerReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let authority =
            claim_authority(&service, &worker, request.identity.as_ref(), false).await?;
        let renewed = service
            .dispatch
            .renew_owned_leases(&authority.owner, authority.lease_ms, authority.now_ms)
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
    #[serde(default)]
    identity: Option<WorkerIdentity>,
}

async fn settle(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SettleReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let authority =
            claim_authority(&service, &worker, request.identity.as_ref(), false).await?;
        // Bind the epoch to the authenticated owner before settling. Dropping the
        // guard before `settle` is safe: any intervening re-owner increments epoch,
        // causing the subsequent settle to be fenced.
        let claim = RunClaim {
            run_id: RunId(request.run_id.clone()),
            owner: authority.owner,
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
