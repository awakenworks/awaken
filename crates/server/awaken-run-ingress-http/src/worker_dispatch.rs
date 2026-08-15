//! Authenticated worker-facing dispatch transport.
//!
//! Wire callers submit only business data. Worker ownership, time, and lease
//! duration are derived from trusted server-side SPIs before the neutral
//! `DispatchQueue` is called.

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_run_ingress::{
    BindSandboxRequest as BindSandboxReq, CheckpointRequest as CheckpointReq,
    ClaimNewRunRequest as ClaimNewRunReq, ClaimRunRequest as ClaimRunReq,
    ClaimWorkerRequest as ClaimWorkerReq, CompletionSink,
    CredentialRealizationRequest as CredentialRealizationReq,
    DeliverAndClaimRequest as DeliverAndClaimReq, DispatchQueue, EnqueueRequest as EnqueueReq,
    HeartbeatWorkerRequest as HeartbeatWorkerReq, PlacementPolicy, RecoveryRequest as RecoveryReq,
    RegisterWorkerRequest as RegisterWorkerReq, RelinquishRequest as RelinquishReq,
    RenewRequest as RenewReq, RunClaim, SettleRequest as SettleReq,
    StreamEventRequest as StreamEventReq, WorkerDirectory, WorkerIdentity,
    WorkerIdentityRequest as WorkerIdentityReq, WorkerSnapshot,
};

use awaken_run_ingress::{ApplicationError as HostError, ApplicationErrorKind};
use awaken_worker_transport_security::{
    FixedWorkerLeasePolicy, HeaderWorkerAuthenticator, SystemWorkerClock, VerifiedWorkerContext,
    WorkerClock, WorkerLeasePolicy, WorkerRequestAuthenticator, authenticate_worker_request,
    verify_current_worker_identity, verify_worker_identity,
};

/// Read-only, authenticated projection of current Environment warm demand.
/// Desired state remains in `EnvironmentWarmupSource`; this adapter owns no
/// queue, cache, or receipt store.
pub fn worker_environment_warmup_router(
    source: Arc<dyn awaken_session_contract::EnvironmentWarmupSource>,
    directory: Arc<dyn WorkerDirectory>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
) -> Router {
    worker_environment_warmup_router_with_clock(
        source,
        directory,
        authenticator,
        Arc::new(SystemWorkerClock),
    )
}

/// Clock-injected form used by the same deterministic Worker transport tests as
/// registration and dispatch. Production calls [`worker_environment_warmup_router`].
pub fn worker_environment_warmup_router_with_clock(
    source: Arc<dyn awaken_session_contract::EnvironmentWarmupSource>,
    directory: Arc<dyn WorkerDirectory>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    clock: Arc<dyn WorkerClock>,
) -> Router {
    #[derive(Clone)]
    struct WarmupState {
        source: Arc<dyn awaken_session_contract::EnvironmentWarmupSource>,
        directory: Arc<dyn WorkerDirectory>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
        clock: Arc<dyn WorkerClock>,
    }

    async fn current(
        State(state): State<WarmupState>,
        Extension(worker): Extension<VerifiedWorkerContext>,
        Json(request): Json<WorkerIdentityReq>,
    ) -> (StatusCode, Json<Value>) {
        let result = async {
            verify_current_worker_identity(
                state.directory.as_ref(),
                &worker,
                &request.identity,
                state.clock.now_ms(),
                false,
            )
            .await
            .map_err(HostError::bad_request)?;
            let warmups = state
                .source
                .current_environment_warmups()
                .await
                .map_err(HostError::internal)?;
            Ok(json!({ "warmups": warmups }))
        }
        .await;
        respond(result)
    }

    let state = WarmupState {
        source,
        directory,
        authenticator,
        clock,
    };
    Router::new()
        .route("/v1/worker/environment/warmups", post(current))
        .route_layer(axum::middleware::from_fn_with_state(
            state.authenticator.clone(),
            authenticate_worker_request,
        ))
        .with_state(state)
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
    recovery: Option<Arc<dyn RunRecoverySource>>,
    completion: Option<Arc<dyn CompletionSink>>,
    stream_sink: Option<Arc<dyn StreamSink>>,
    session_control: Option<Arc<dyn awaken_session_contract::SessionRealizationControl>>,
    session_work: Option<Arc<dyn awaken_session_contract::work_queue::SessionWorkLeaseAuthority>>,
    local_credential_capabilities: awaken_runtime_contract::CredentialRealizationCapabilities,
    max_attempts: u64,
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
            recovery: None,
            completion: None,
            stream_sink: None,
            session_control: None,
            session_work: None,
            local_credential_capabilities: Default::default(),
            max_attempts: 5,
        }
    }

    #[must_use]
    pub fn with_checkpoint_store(mut self, checkpoint: Arc<dyn StreamCheckpointStore>) -> Self {
        self.checkpoint = Some(checkpoint);
        self
    }

    #[must_use]
    pub fn with_recovery_source(mut self, recovery: Arc<dyn RunRecoverySource>) -> Self {
        self.recovery = Some(recovery);
        self
    }

    /// Project an applied remote settlement into the foreground wakeup channel.
    /// The exact Run state is re-read from committed recovery truth; this sink is
    /// never a dispatch or lifecycle authority.
    #[must_use]
    pub fn with_completion_sink(mut self, completion: Arc<dyn CompletionSink>) -> Self {
        self.completion = Some(completion);
        self
    }

    /// Install the Coordinator's one foreground live observation registry.
    #[must_use]
    pub fn with_stream_sink(mut self, stream_sink: Arc<dyn StreamSink>) -> Self {
        self.stream_sink = Some(stream_sink);
        self
    }

    /// Install the Coordinator-owned Session application service behind the same
    /// authenticated Worker and exact-claim boundary as dispatch/recovery.
    #[must_use]
    pub fn with_session_control(
        mut self,
        control: Arc<dyn awaken_session_contract::SessionRealizationControl>,
    ) -> Self {
        self.session_control = Some(control);
        self
    }

    #[must_use]
    pub fn with_session_work_authority(
        mut self,
        authority: Arc<dyn awaken_session_contract::work_queue::SessionWorkLeaseAuthority>,
    ) -> Self {
        self.session_work = Some(authority);
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

    #[must_use]
    pub fn with_local_credential_capabilities(
        mut self,
        capabilities: awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Self {
        self.local_credential_capabilities = capabilities;
        self
    }

    /// Set the control-side crash-retry budget enforced immediately before
    /// every worker claim. The control plane, not the remote worker, owns this
    /// scheduling policy.
    #[must_use]
    pub fn with_max_attempts(mut self, max_attempts: u64) -> Self {
        self.max_attempts = max_attempts.max(1);
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
    let snapshot =
        verify_current_worker_identity(directory.as_ref(), worker, identity, now_ms, require_ready)
            .await
            .map_err(HostError::bad_request)?;
    Ok(ClaimAuthority {
        owner: identity.lease_owner(),
        lease_ms,
        now_ms,
        snapshot: Some(snapshot),
    })
}

pub struct RegisteredDispatchDependencies {
    pub dispatch: Arc<dyn DispatchQueue>,
    pub checkpoint: Arc<dyn StreamCheckpointStore>,
    pub directory: Arc<dyn WorkerDirectory>,
    pub policy: Arc<dyn PlacementPolicy>,
    pub sessions: Arc<dyn awaken_session_contract::SessionRealizationControl>,
    pub session_work: Arc<dyn awaken_session_contract::work_queue::SessionWorkLeaseAuthority>,
    pub authenticator: Arc<dyn WorkerRequestAuthenticator>,
    pub recovery: Arc<dyn RunRecoverySource>,
    pub completion: Arc<dyn CompletionSink>,
    pub stream_sink: Arc<dyn StreamSink>,
}

pub fn registered_dispatch_router(dependencies: RegisteredDispatchDependencies) -> Router {
    let RegisteredDispatchDependencies {
        dispatch,
        checkpoint,
        directory,
        policy,
        sessions,
        session_work,
        authenticator,
        recovery,
        completion,
        stream_sink,
    } = dependencies;
    dispatch_transport_router_with_service(Arc::new(
        WorkerDispatchService::new(
            dispatch,
            authenticator,
            Arc::new(SystemWorkerClock),
            Arc::new(FixedWorkerLeasePolicy::default()),
        )
        .with_worker_directory(directory, 30_000)
        .with_placement_policy(policy)
        .with_checkpoint_store(checkpoint)
        .with_recovery_source(recovery)
        .with_completion_sink(completion)
        .with_stream_sink(stream_sink)
        .with_session_control(sessions)
        .with_session_work_authority(session_work),
    ))
}

/// Compose the complete registered-Worker transport from already-configured
/// application services.
///
/// Embedding Coordinator processes use this entry after selecting their authoritative
/// dispatch, commit, directory, authentication, recovery, and checkpoint SPIs.
/// Keeping the merge here prevents a product composition root from mounting
/// claims without the matching claim-fenced commit surface.
pub fn registered_worker_transport_router_with_services(
    dispatch_router: Router,
    resource_router: Router,
    commit_service: Arc<crate::ClaimedCommitHttpService>,
) -> Router {
    dispatch_router
        .merge(resource_router)
        .merge(crate::claimed_commit_router(commit_service))
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
        .route("/v1/worker/dispatch/relinquish", post(relinquish))
        .route("/v1/worker/dispatch/bind_sandbox", post(bind_sandbox))
        .route(
            "/v1/worker/dispatch/claim_is_current",
            post(claim_is_current),
        )
        .route(
            "/v1/worker/dispatch/credential_realization",
            post(record_credential_realization),
        )
        .route("/v1/worker/dispatch/settle", post(settle))
        .route("/v1/worker/dispatch/stream", post(stream_event))
        .route("/v1/worker/register", post(register_worker))
        .route("/v1/worker/heartbeat", post(heartbeat_worker))
        .route("/v1/worker/drain", post(drain_worker))
        .route("/v1/worker/quiesced", post(quiesce_worker))
        .route("/v1/worker/deregister", post(deregister_worker))
        .route("/v1/worker/checkpoint/get", post(get_checkpoint))
        .route("/v1/worker/checkpoint/put", post(put_checkpoint))
        .route("/v1/worker/checkpoint/delete", post(delete_checkpoint))
        .route("/v1/worker/recovery/snapshot", post(recovery_snapshot))
        .route("/v1/worker/session/resume", post(session_resume))
        .route(
            "/v1/worker/session/realization/begin",
            post(begin_session_realization),
        )
        .route(
            "/v1/worker/session/realization/activate",
            post(activate_session_realization),
        )
        .route(
            "/v1/worker/session/realization/acknowledge",
            post(acknowledge_session_realization),
        )
        .route(
            "/v1/worker/session/realization/fail",
            post(fail_session_realization),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            service.authenticator.clone(),
            authenticate_worker_request,
        ))
        .with_state(service)
}

fn respond(result: Result<Value, HostError>) -> (StatusCode, Json<Value>) {
    match result {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err(error) => {
            let status = match error.kind {
                ApplicationErrorKind::InvalidRequest => StatusCode::BAD_REQUEST,
                ApplicationErrorKind::Conflict => StatusCode::CONFLICT,
                ApplicationErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
                ApplicationErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            };
            (status, Json(json!({ "error": error.message })))
        }
    }
}

enum RealizationHttpError {
    Boundary(HostError),
    Control(awaken_session_contract::SessionRealizationControlFailure),
}

impl From<HostError> for RealizationHttpError {
    fn from(error: HostError) -> Self {
        Self::Boundary(error)
    }
}

impl From<awaken_session_contract::SessionRealizationControlFailure> for RealizationHttpError {
    fn from(error: awaken_session_contract::SessionRealizationControlFailure) -> Self {
        Self::Control(error)
    }
}

fn respond_realization(result: Result<Value, RealizationHttpError>) -> (StatusCode, Json<Value>) {
    match result {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err(RealizationHttpError::Boundary(error)) => respond(Err(error)),
        Err(RealizationHttpError::Control(error)) => {
            use awaken_session_contract::SessionRealizationControlFailure;
            let status = match &error {
                SessionRealizationControlFailure::NotFound => StatusCode::NOT_FOUND,
                SessionRealizationControlFailure::NotReady
                | SessionRealizationControlFailure::Terminal
                | SessionRealizationControlFailure::StaleOwnership
                | SessionRealizationControlFailure::Conflict => StatusCode::CONFLICT,
                SessionRealizationControlFailure::Invalid(_) => StatusCode::BAD_REQUEST,
                SessionRealizationControlFailure::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            };
            (
                status,
                Json(json!({
                    "error": error.to_string(),
                    "realization_error": error,
                })),
            )
        }
    }
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
struct SessionResumeReq {
    claim: RunClaim,
    identity: WorkerIdentity,
    session_id: String,
}

async fn session_resume(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SessionResumeReq>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        let authority = claim_authority(&service, &worker, Some(&request.identity), false)
            .await
            .map_err(RealizationHttpError::from)?;
        if authority.owner != request.claim.owner {
            return Err(RealizationHttpError::from(HostError::bad_request(
                "authenticated worker does not own the Session resume claim",
            )));
        }
        let guard = service
            .dispatch
            .lock_commit_epoch(&request.claim)
            .await
            .map_err(|error| HostError::internal(error.to_string()))
            .map_err(RealizationHttpError::from)?
            .ok_or_else(|| {
                RealizationHttpError::from(HostError::bad_request("Session resume claim is stale"))
            })?;
        if !guard.is_live_at(authority.now_ms) {
            return Err(RealizationHttpError::from(HostError::bad_request(
                "Session resume claim lease has expired",
            )));
        }
        let dispatch = guard.request();
        if dispatch.run_id() != &request.claim.run_id {
            return Err(RealizationHttpError::from(HostError::bad_request(
                "guarded dispatch does not match the Session resume claim",
            )));
        }
        if dispatch.session_thread_id().0 != request.session_id {
            return Err(RealizationHttpError::from(HostError::bad_request(
                "Session resume target does not match the claimed Run",
            )));
        }
        acquire_session_work_owner(
            &service,
            &request.session_id,
            &request.identity.lease_owner(),
            authority.now_ms,
            awaken_session_contract::work_queue::SessionWorkAcquisition::ClaimedRun,
        )
        .await?;
        let control = service.session_control.as_ref().ok_or_else(|| {
            RealizationHttpError::from(HostError::internal("Session control is not configured"))
        })?;
        let registry_expiry = authority
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.expires_at_ms)
            .unwrap_or(u64::MAX);
        let realization_expiry = authority
            .now_ms
            .saturating_add(authority.lease_ms)
            .min(registry_expiry);
        let realization = match control
            .begin_session_realization(awaken_session_contract::BeginSessionRealization {
                session_id: request.session_id,
                target: awaken_session_contract::SessionRealizationTarget {
                    owner: request.identity.worker_id.clone(),
                    runtime_incarnation: request.identity.lease_owner(),
                    lease_expires_at_unix_ms: realization_expiry,
                    renew_existing_lease: false,
                    // The exact live dispatch guard above proves this Worker is
                    // the sole executor for the Session thread. It may therefore
                    // fence a predecessor Worker's longer-lived projection lease
                    // without waiting for an unrelated timeout.
                    reassign_existing_lease: true,
                },
            })
            .await
        {
            Ok(realization) => Some(realization),
            Err(awaken_session_contract::SessionRealizationControlFailure::NotReady) => {
                return Err(RealizationHttpError::Control(
                    awaken_session_contract::SessionRealizationControlFailure::NotReady,
                ));
            }
            Err(error) => return Err(RealizationHttpError::Control(error)),
        };
        if !guard.is_live_at(service.clock.now_ms()) {
            return Err(RealizationHttpError::from(HostError::bad_request(
                "Session resume claim expired before realization assignment",
            )));
        }
        drop(guard);
        Ok(json!({ "realization": realization }))
    }
    .await;
    respond_realization(result)
}

#[derive(Deserialize)]
struct SessionRealizationReq<T> {
    identity: WorkerIdentity,
    command: T,
}

async fn verify_session_realization_authority(
    service: &WorkerDispatchService,
    worker: &VerifiedWorkerContext,
    identity: &WorkerIdentity,
    session_id: &str,
    lease: &awaken_session_contract::SessionRealizationLease,
) -> Result<(), RealizationHttpError> {
    verify_worker_identity(worker, identity)
        .map_err(HostError::bad_request)
        .map_err(RealizationHttpError::from)?;
    let authority = claim_authority(service, worker, Some(identity), false)
        .await
        .map_err(RealizationHttpError::from)?;
    if lease.owner != identity.worker_id
        || lease.runtime_incarnation != identity.lease_owner()
        || !awaken_session_contract::realization_lease_is_live_at(
            lease.expires_at_unix_ms,
            authority.now_ms,
        )
    {
        return Err(RealizationHttpError::from(HostError::bad_request(
            "Session realization lease is not owned by the authenticated Worker incarnation",
        )));
    }
    acquire_session_work_owner(
        service,
        session_id,
        &identity.lease_owner(),
        authority.now_ms,
        awaken_session_contract::work_queue::SessionWorkAcquisition::RealizationRenewal,
    )
    .await?;
    Ok(())
}

async fn acquire_session_work_owner(
    service: &WorkerDispatchService,
    session_id: &str,
    worker_owner: &str,
    now_ms: u64,
    acquisition: awaken_session_contract::work_queue::SessionWorkAcquisition,
) -> Result<(), RealizationHttpError> {
    use awaken_session_contract::work_queue::SessionWorkOwnership;

    match session_work_ownership(service, session_id, worker_owner, now_ms, acquisition)
        .await
        .map_err(RealizationHttpError::from)?
    {
        SessionWorkOwnership::NotRequired => Ok(()),
        SessionWorkOwnership::Leased(lease) if lease.owner == worker_owner => Ok(()),
        SessionWorkOwnership::Unowned | SessionWorkOwnership::Leased(_) => {
            Err(RealizationHttpError::Control(
                awaken_session_contract::SessionRealizationControlFailure::NotReady,
            ))
        }
    }
}

async fn session_work_ownership(
    service: &WorkerDispatchService,
    session_id: &str,
    worker_owner: &str,
    now_ms: u64,
    acquisition: awaken_session_contract::work_queue::SessionWorkAcquisition,
) -> Result<awaken_session_contract::work_queue::SessionWorkOwnership, HostError> {
    let authority = service
        .session_work
        .as_ref()
        .ok_or_else(|| HostError::internal("Session Work authority is not configured"))?;
    authority
        .acquire_session_work(session_id, worker_owner, now_ms, acquisition)
        .await
        .map_err(|error| HostError::internal(error.to_string()))
}

fn session_control(
    service: &WorkerDispatchService,
) -> Result<&Arc<dyn awaken_session_contract::SessionRealizationControl>, HostError> {
    service
        .session_control
        .as_ref()
        .ok_or_else(|| HostError::internal("Session control is not configured"))
}

async fn begin_session_realization(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SessionRealizationReq<awaken_session_contract::BeginSessionRealization>>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        verify_worker_identity(&worker, &request.identity)
            .map_err(HostError::bad_request)
            .map_err(RealizationHttpError::from)?;
        let authority = claim_authority(&service, &worker, Some(&request.identity), false)
            .await
            .map_err(RealizationHttpError::from)?;
        let registry_expiry = authority
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.expires_at_ms)
            .unwrap_or(u64::MAX);
        let target = &request.command.target;
        if !target.renew_existing_lease
            || target.reassign_existing_lease
            || target.owner != request.identity.worker_id
            || target.runtime_incarnation != request.identity.lease_owner()
            || !awaken_session_contract::realization_lease_is_live_at(
                target.lease_expires_at_unix_ms,
                authority.now_ms,
            )
            || target.lease_expires_at_unix_ms > registry_expiry
        {
            return Err(RealizationHttpError::from(HostError::bad_request(
                "Session realization renewal exceeds authenticated Worker authority",
            )));
        }
        use awaken_session_contract::work_queue::SessionWorkOwnership;
        match session_work_ownership(
            &service,
            &request.command.session_id,
            &request.identity.lease_owner(),
            authority.now_ms,
            awaken_session_contract::work_queue::SessionWorkAcquisition::RealizationRenewal,
        )
        .await
        .map_err(RealizationHttpError::from)?
        {
            SessionWorkOwnership::NotRequired => {}
            SessionWorkOwnership::Leased(lease)
                if lease.owner == request.identity.lease_owner() => {}
            // A settled Run retires its self-hosted Session Work before the
            // longer-lived local realization lease next comes due. That is a
            // normal retirement proof, not evidence that another Worker stole
            // authority. Preserve the typed lifecycle result so the Worker
            // quietly revokes only its stale process-local projection.
            SessionWorkOwnership::Unowned => {
                return Err(RealizationHttpError::Control(
                    awaken_session_contract::SessionRealizationControlFailure::NotReady,
                ));
            }
            SessionWorkOwnership::Leased(_) => {
                return Err(RealizationHttpError::Control(
                    awaken_session_contract::SessionRealizationControlFailure::StaleOwnership,
                ));
            }
        }
        let realization = session_control(&service)
            .map_err(RealizationHttpError::from)?
            .begin_session_realization(request.command)
            .await
            .map_err(RealizationHttpError::from)?;
        Ok(json!({ "realization": realization }))
    }
    .await;
    respond_realization(result)
}

async fn activate_session_realization(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SessionRealizationReq<awaken_session_contract::ActivateSessionRealization>>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        verify_session_realization_authority(
            &service,
            &worker,
            &request.identity,
            &request.command.session_id,
            &request.command.lease,
        )
        .await
        .map_err(RealizationHttpError::from)?;
        let realization = session_control(&service)
            .map_err(RealizationHttpError::from)?
            .activate_session_realization(request.command)
            .await
            .map_err(RealizationHttpError::from)?;
        Ok(json!({ "realization": realization }))
    }
    .await;
    respond_realization(result)
}

async fn acknowledge_session_realization(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<
        SessionRealizationReq<awaken_session_contract::AcknowledgeSessionRealization>,
    >,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        verify_session_realization_authority(
            &service,
            &worker,
            &request.identity,
            &request.command.session_id,
            &request.command.lease,
        )
        .await
        .map_err(RealizationHttpError::from)?;
        let realization = session_control(&service)
            .map_err(RealizationHttpError::from)?
            .acknowledge_session_realization(request.command)
            .await
            .map_err(RealizationHttpError::from)?;
        Ok(json!({ "realization": realization }))
    }
    .await;
    respond_realization(result)
}

async fn fail_session_realization(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SessionRealizationReq<awaken_session_contract::FailSessionRealization>>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        verify_session_realization_authority(
            &service,
            &worker,
            &request.identity,
            &request.command.session_id,
            &request.command.lease,
        )
        .await
        .map_err(RealizationHttpError::from)?;
        session_control(&service)
            .map_err(RealizationHttpError::from)?
            .fail_session_realization(request.command)
            .await
            .map_err(RealizationHttpError::from)?;
        Ok(json!({ "failed": true }))
    }
    .await;
    respond_realization(result)
}

async fn claim_is_current(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<RecoveryReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let authority =
            claim_authority(&service, &worker, request.identity.as_ref(), false).await?;
        if authority.owner != request.claim.owner {
            return Err(HostError::bad_request(
                "authenticated worker does not own the claim",
            ));
        }
        let guard = service
            .dispatch
            .lock_commit_epoch(&request.claim)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        let Some(guard) = guard else {
            return Ok(json!({ "current": false }));
        };
        if !guard.is_live_at(authority.now_ms) {
            return Ok(json!({ "current": false }));
        }
        let dispatch = guard.request();
        if dispatch.run_id() != &request.claim.run_id {
            return Err(HostError::bad_request(
                "guarded dispatch does not match the claim",
            ));
        }
        // This endpoint answers only the DispatchQueue claim question. Session
        // Work admission belongs to `session_resume`; mutating it from every
        // ownership probe creates a second scheduler and turns ordinary
        // pre-execution checks into a claim/relinquish hot loop.
        Ok(json!({ "current": true }))
    }
    .await;
    respond(result)
}

async fn record_credential_realization(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<CredentialRealizationReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let authority =
            claim_authority(&service, &worker, request.identity.as_ref(), false).await?;
        if authority.owner != request.claim.owner {
            return Err(HostError::bad_request(
                "authenticated worker does not own the credential realization claim",
            ));
        }
        let outcome = service
            .dispatch
            .record_credential_realization(&request.claim, request.receipt)
            .await
            .map_err(|error| HostError::bad_request(error.to_string()))?;
        Ok(json!({ "applied": outcome.applied() }))
    }
    .await;
    respond(result)
}

async fn recovery_snapshot(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<RecoveryReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let authority =
            claim_authority(&service, &worker, request.identity.as_ref(), false).await?;
        if authority.owner != request.claim.owner {
            return Err(HostError::bad_request(
                "authenticated worker does not own the recovery claim",
            ));
        }
        let guard = service
            .dispatch
            .lock_commit_epoch(&request.claim)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?
            .ok_or_else(|| HostError::bad_request("run claim is stale"))?;
        let dispatch = guard.request();
        if dispatch.run_id() != &request.claim.run_id {
            return Err(HostError::bad_request(
                "guarded dispatch does not match the recovery claim",
            ));
        }
        let source = service
            .recovery
            .as_ref()
            .ok_or_else(|| HostError::internal("worker recovery source is not configured"))?;
        let snapshot = source
            .recovery_snapshot(dispatch.thread_id(), dispatch.run_id())
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "snapshot": snapshot }))
    }
    .await;
    respond(result)
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

async fn register_worker(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<RegisterWorkerReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        verify_worker_id(&worker, &request.registration.worker_id)?;
        if worker.identity().is_some() {
            return Err(HostError::bad_request(
                "worker registration requires a bootstrap credential",
            ));
        }
        let record = match directory(&service)?
            .register(
                request.registration,
                service.clock.now_ms(),
                service.registry_ttl_ms,
            )
            .await
        {
            Ok(record) => record,
            Err(awaken_run_ingress::RegistryError::SlotOccupied {
                worker_id,
                generation,
            }) => {
                return Err(HostError::conflict(format!(
                    "worker slot {worker_id} is occupied by generation {generation}"
                )));
            }
            Err(error) => return Err(HostError::bad_request(error.to_string())),
        };
        Ok(json!({ "worker": record }))
    }
    .await;
    respond(result)
}

async fn heartbeat_worker(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<HeartbeatWorkerReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        verify_worker_identity(&worker, &request.identity).map_err(HostError::bad_request)?;
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

async fn drain_worker(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<WorkerIdentityReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        verify_worker_identity(&worker, &request.identity).map_err(HostError::bad_request)?;
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
        verify_worker_identity(&worker, &request.identity).map_err(HostError::bad_request)?;
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
        verify_worker_identity(&worker, &request.identity).map_err(HostError::bad_request)?;
        if let Some(session_work) = service.session_work.as_ref() {
            session_work
                .release_worker_session_work(&request.identity.lease_owner())
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
        }
        let mutation = directory(&service)?
            .deregister(&request.identity)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "mutation": mutation }))
    }
    .await;
    respond(result)
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
                    &service.local_credential_capabilities,
                )
                .await
        }
        .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "claimed": claimed }))
    }
    .await;
    respond(result)
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
                    &service.local_credential_capabilities,
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
        service
            .dispatch
            .reap(service.max_attempts, authority.now_ms)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
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
                .claim(
                    &authority.owner,
                    authority.lease_ms,
                    authority.now_ms,
                    &service.local_credential_capabilities,
                )
                .await
        }
        .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "claimed": claimed }))
    }
    .await;
    respond(result)
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
                    &service.local_credential_capabilities,
                )
                .await
        }
        .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "claimed": claimed }))
    }
    .await;
    respond(result)
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

async fn relinquish(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<RelinquishReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let authority =
            claim_authority(&service, &worker, request.identity.as_ref(), false).await?;
        if authority.owner != request.claim.owner {
            return Err(HostError::bad_request(
                "authenticated worker does not own the relinquished claim",
            ));
        }
        let relinquished = service
            .dispatch
            .relinquish_claim(&request.claim)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?
            .applied();
        Ok(json!({ "relinquished": relinquished }))
    }
    .await;
    respond(result)
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
        let thread_id = guard.request().thread_id().clone();
        let session_thread_id = guard.request().session_thread_id().clone();
        let owns_session_work = thread_id == session_thread_id;
        drop(guard);
        // When configured, registered-Worker Session Work is the outer ownership
        // fence. Release it only after every claim-fenced commit has completed
        // and immediately before the subordinate Run delivery settles. Generic
        // Run-only compositions have no synthetic Work item to acquire or clear.
        if owns_session_work && let Some(session_work) = service.session_work.as_ref() {
            let released = session_work
                .release_session_work(&session_thread_id.0, &claim.owner, authority.now_ms)
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
            if !released {
                return Err(HostError::bad_request(
                    "Session Work ownership was lost before Run settlement",
                ));
            }
        }
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
        if outcome.applied()
            && let (Some(completion), Some(recovery)) = (&service.completion, &service.recovery)
            && let Ok(snapshot) = recovery.recovery_snapshot(&thread_id, &claim.run_id).await
            && let Some(record) = snapshot
                .runs
                .iter()
                .find(|record| record.id == claim.run_id)
        {
            completion.settled(&claim.run_id, &record.state);
        }
        Ok(json!({ "settled": outcome.applied() }))
    }
    .await;
    respond(result)
}

async fn stream_event(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<StreamEventReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let authority = claim_authority(&service, &worker, Some(&request.identity), false).await?;
        if request.claim.owner != authority.owner || request.event.run_id != request.claim.run_id {
            return Err(HostError::bad_request(
                "live Worker event does not match its authenticated claim",
            ));
        }
        if !awaken_agent_contract::event::classify(&request.event.kind).live {
            return Err(HostError::bad_request(
                "Worker transport accepts only live-classified Agent events",
            ));
        }
        let current = service
            .dispatch
            .lock_commit_epoch(&request.claim)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        let Some(_guard) = current else {
            return Ok(json!({ "accepted": false }));
        };
        if let Some(sink) = &service.stream_sink {
            let _ = sink.send(request.event).await;
        }
        Ok(json!({ "accepted": true }))
    }
    .await;
    respond(result)
}
