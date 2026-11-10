// @formal entrypoint remote_worker.execution_requires_claimed_snapshot
// @formal entrypoint remote_worker.single_live_claim_owner
// @formal entrypoint session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement
// @formal entrypoint session_execution.initial_event_path_reaches_idle_only_after_full_settlement
// @formal entrypoint session_run.stale_work_epoch_cannot_observe_authoritative_thread_commit
// @formal entrypoint session_work.release_requires_exact_owner_and_epoch
// @formal entrypoint worker.incarnation_generation_fences_stale_processes
//! Authenticated worker-facing dispatch transport.
//!
//! Wire callers submit only business data. Worker ownership, time, and lease
//! duration are derived from trusted server-side SPIs before the neutral
//! `DispatchQueue` is called.

use awaken_session_configuration_contract::resource::SessionResourceManifest;
use awaken_session_execution_contract::{ApplicationError, ApplicationErrorKind};
use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::{Json, Router, routing::post};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::Instrument;

use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::observation::Observed;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_dispatch::{
    AttemptExecutionRequest as AttemptExecutionReq, BindSandboxRequest as BindSandboxReq,
    CheckpointRequest as CheckpointReq, ClaimNewRunRequest as ClaimNewRunReq,
    ClaimRunRequest as ClaimRunReq, ClaimWorkerRequest as ClaimWorkerReq, CompletionSink,
    CredentialDeliveryRefreshRequest as CredentialDeliveryRefreshReq,
    CredentialRealizationRequest as CredentialRealizationReq,
    DeliverAndClaimRequest as DeliverAndClaimReq, DispatchSettlementObserver,
    EnqueueRequest as EnqueueReq, HeartbeatWorkerRequest as HeartbeatWorkerReq, PlacementPolicy,
    RecoveryRequest as RecoveryReq, RegisterWorkerRequest as RegisterWorkerReq,
    RenewRequest as RenewReq, RunClaim,
    SessionRunReservationResolutionRequest as ReservationResolutionReq, SettleRequest as SettleReq,
    StreamObservationRequest as StreamEventReq, WorkerDirectory, WorkerIdentity,
    WorkerIdentityRequest as WorkerIdentityReq, WorkerSnapshot,
};

use awaken_time::{WallClock, WallClockExt};
use awaken_worker_transport_security::{
    VerifiedWorkerContext, WorkerLeasePolicy, WorkerRequestAuthenticator,
    authenticate_worker_request, verify_current_worker_identity, verify_worker_identity,
};

mod registry_endpoints;
use registry_endpoints::{
    deregister_worker, drain_worker, heartbeat_worker, quiesce_worker, register_worker,
};

mod run_dispatch_endpoints;
mod session_coordination;
mod session_realization;
mod session_requests;
mod task_attention;
pub use task_attention::worker_task_control_router;
mod terminal_repository_publication;

/// Private Worker transport DTO. The retry hint is deliberately absent from
/// the public ingress and Anthropic Managed API contracts.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RelinquishReq {
    claim: RunClaim,
    #[serde(default)]
    retry_after_ms: Option<u64>,
    /// The claim ended a failed attempt: spend one attempt of this
    /// Coordinator's retry budget. Absent from older Workers, which keep the
    /// budget-free deferral.
    #[serde(default)]
    attempt_failed: bool,
    #[serde(default)]
    identity: Option<WorkerIdentity>,
}

use run_dispatch_endpoints::{
    begin_attempt, claim, claim_new_run, claim_retry_exhausted, claim_run, close_attempt,
    deliver_and_claim, enqueue, finish_attempt, handoff_attempt, record_credential_realization,
    refresh_credential_delivery, relinquish, renew, resolve_session_run_reservation,
};
use session_coordination::{
    session_agent_send, session_agent_settle, session_agents_list, session_model_request_admit,
    session_run_activity_admit,
};
use session_realization::{
    acknowledge_session_realization, activate_session_realization,
    authorize_session_environment_effect, authorize_session_terminal_cleanup_disposal,
    authorize_session_terminal_cleanup_preparation, fail_session_realization,
    persist_session_environment_receipt, record_session_terminal_cleanup_disposal,
    record_session_terminal_cleanup_preparation, renew_session_realization,
    session_cleanup_claim_next, session_cleanup_poll, session_preparation_claim_next,
    session_resume,
};
use terminal_repository_publication::{
    session_repository_publication_complete, session_repository_publication_poll,
    session_repository_publication_reject,
};

/// Read-only, authenticated projection of current Environment warm demand.
/// Desired state remains in `EnvironmentWarmupSource`; this adapter owns no
/// queue, cache, or receipt store. `clock` judges the Worker's registered
/// incarnation and is the Coordinator's one clock (production and tests alike
/// inject it; there is no defaulted variant).
pub fn worker_environment_warmup_router(
    source: Arc<dyn awaken_session_configuration_contract::EnvironmentWarmupSource>,
    directory: Arc<dyn WorkerDirectory>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    clock: Arc<dyn WallClock>,
) -> Router {
    #[derive(Clone)]
    struct WarmupState {
        source: Arc<dyn awaken_session_configuration_contract::EnvironmentWarmupSource>,
        directory: Arc<dyn WorkerDirectory>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
        clock: Arc<dyn WallClock>,
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
            .map_err(worker_identity_failure)?;
            let warmups = state
                .source
                .current_environment_warmups()
                .await
                .map_err(ApplicationError::unavailable)?;
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

/// Authenticated read of the original Session sources a draining Worker
/// observes and reports retained. `source` is the Coordinator's Worker
/// occupancy read; this adapter verifies the exact current incarnation and
/// owns no report, cache or decision.
pub fn worker_source_retention_router(
    source: Arc<dyn awaken_session_execution_contract::WorkerSourceRetentionSubjects>,
    directory: Arc<dyn WorkerDirectory>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    clock: Arc<dyn WallClock>,
) -> Router {
    #[derive(Clone)]
    struct RetentionState {
        source: Arc<dyn awaken_session_execution_contract::WorkerSourceRetentionSubjects>,
        directory: Arc<dyn WorkerDirectory>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
        clock: Arc<dyn WallClock>,
    }

    async fn subjects(
        State(state): State<RetentionState>,
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
            .map_err(worker_identity_failure)?;
            let subjects = state
                .source
                .source_retention_subjects(&request.identity)
                .await
                .map_err(|error| match error.kind {
                    awaken_runtime_contract::runs::RunErrorKind::BadRequest => {
                        ApplicationError::invalid(error.to_string())
                    }
                    _ => ApplicationError::unavailable(error.to_string()),
                })?;
            Ok(json!({ "subjects": subjects }))
        }
        .await;
        respond(result)
    }

    let state = RetentionState {
        source,
        directory,
        authenticator,
        clock,
    };
    Router::new()
        .route(
            awaken_session_execution_contract::WORKER_SOURCE_RETENTION_SUBJECTS_PATH,
            post(subjects),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.authenticator.clone(),
            authenticate_worker_request,
        ))
        .with_state(state)
}

/// Explicit application service mounted by the worker HTTP adapter.
///
/// Every collaborator the Coordinator composition always supplies is a required
/// field; [`WorkerDispatchService::new`] is the only constructor. The three
/// `Option`s are provider-dependent and their absence fails closed (see
/// [`RegisteredDispatchDependencies`]).
pub struct WorkerDispatchService {
    dispatch: Arc<dyn WorkerDispatchAuthority>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    clock: Arc<dyn WallClock>,
    lease_policy: Arc<dyn WorkerLeasePolicy>,
    directory: Arc<dyn WorkerDirectory>,
    placement_policy: Arc<dyn PlacementPolicy>,
    registry_ttl_ms: u64,
    checkpoint: Arc<dyn StreamCheckpointStore>,
    recovery: Arc<dyn RunRecoverySource>,
    completion: Arc<dyn CompletionSink>,
    terminal_observer: Arc<dyn DispatchSettlementObserver>,
    stream_sink: Arc<dyn StreamSink>,
    session_control: Arc<dyn awaken_session_execution_contract::SessionRealizationControl>,
    session_preparation_control:
        Arc<dyn awaken_session_execution_contract::SessionPreparationControl>,
    terminal_cleanup_control:
        Arc<dyn awaken_session_execution_contract::SessionTerminalCleanupControl>,
    session_environment_bindings:
        Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    session_coordination: Arc<dyn awaken_session_execution_contract::SessionAgentCoordination>,
    session_work: Arc<dyn awaken_session_application_contract::SessionWorkerLeaseAuthority>,
    credential_deliveries:
        Option<Arc<dyn awaken_credential_contract::AttemptCredentialDeliverySource>>,
    max_attempts: u64,
    execution_bindings: Option<Arc<dyn awaken_dispatch::WorkerExecutionBindingSource>>,
    execution_stops: Option<Arc<dyn awaken_dispatch::WorkerExecutionStopSource>>,
}

impl WorkerDispatchService {
    /// The one construction path, shared by the Coordinator composition root
    /// ([`registered_dispatch_router`]) and the test fixture.
    #[must_use]
    pub fn new(dependencies: RegisteredDispatchDependencies) -> Self {
        let RegisteredDispatchDependencies {
            execution_bindings,
            execution_stops,
            dispatch,
            checkpoint,
            directory,
            policy,
            realizations,
            terminal_cleanup,
            session_preparation,
            session_environment_bindings,
            coordination,
            session_work,
            authenticator,
            recovery,
            completion,
            terminal_observer,
            stream_sink,
            credential_deliveries,
            clock,
            lease_policy,
            registry_ttl_ms,
            max_attempts,
        } = dependencies;
        Self {
            dispatch,
            authenticator,
            clock,
            lease_policy,
            directory,
            placement_policy: policy,
            registry_ttl_ms: registry_ttl_ms.max(1),
            checkpoint,
            recovery,
            completion,
            terminal_observer,
            stream_sink,
            session_control: realizations,
            session_preparation_control: session_preparation,
            terminal_cleanup_control: terminal_cleanup,
            session_environment_bindings,
            session_coordination: coordination,
            session_work,
            credential_deliveries,
            max_attempts: max_attempts.max(1),
            execution_bindings,
            execution_stops,
        }
    }
}

/// HTTP projection of the security owner's already-classified failure. The
/// verifier alone decides identity/currentness; no transport repeats its read.
pub(crate) fn worker_identity_failure(
    error: awaken_worker_transport_security::WorkerAuthError,
) -> ApplicationError {
    use awaken_worker_transport_security::WorkerAuthError;
    match error {
        WorkerAuthError::Unavailable(detail) => ApplicationError::unavailable(detail),
        WorkerAuthError::Invalid(detail) => ApplicationError::invalid(detail),
        WorkerAuthError::Missing => ApplicationError::invalid("worker authentication is missing"),
    }
}

/// Claim authority is always derived from the current registered incarnation:
/// there is no directory-less mode that grants `owner = worker_id`.
struct ClaimAuthority {
    owner: String,
    lease_ms: u64,
    now_ms: u64,
    snapshot: WorkerSnapshot,
}

async fn claim_authority(
    service: &WorkerDispatchService,
    worker: &VerifiedWorkerContext,
    identity: Option<&WorkerIdentity>,
    require_ready: bool,
) -> Result<ClaimAuthority, ApplicationError> {
    let now_ms = service.clock.now_ms();
    let lease_ms = service.lease_policy.lease_ms(worker);
    let identity = identity.ok_or_else(|| {
        ApplicationError::invalid("registered worker identity is required for dispatch authority")
    })?;
    let snapshot = verify_current_worker_identity(
        service.directory.as_ref(),
        worker,
        identity,
        now_ms,
        require_ready,
    )
    .await
    .map_err(worker_identity_failure)?;
    if !snapshot.manifest.supports_current_runtime_protocol() {
        return Err(ApplicationError::invalid(
            "Worker manifest does not explicitly support the current runtime protocol",
        ));
    }
    Ok(ClaimAuthority {
        owner: identity.lease_owner(),
        lease_ms,
        now_ms,
        snapshot,
    })
}

/// Distinguish the authority assertion from the immutable Environment operation
/// coordinates without creating another authentication path.
#[derive(Clone, Copy)]
enum RealizationAuthorityPurpose {
    /// Environment intents/receipts retain their original lease coordinates.
    /// The repository-backed Session Root applies the current exact/live,
    /// nonregressed renewal law; this boundary must not decide that law again.
    EnvironmentEffect,
    /// Ordinary control commands still require a live asserted lease.
    ActiveControl,
}

async fn verify_session_realization_authority(
    service: &WorkerDispatchService,
    worker: &VerifiedWorkerContext,
    identity: &WorkerIdentity,
    session_id: &str,
    lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
    purpose: RealizationAuthorityPurpose,
) -> Result<(), RealizationHttpError> {
    verify_worker_identity(worker, identity)
        .map_err(ApplicationError::invalid)
        .map_err(RealizationHttpError::from)?;
    let authority = claim_authority(service, worker, Some(identity), false).await?;
    if lease.owner != identity.worker_id
        || lease.runtime_incarnation != identity.lease_owner()
        || (matches!(purpose, RealizationAuthorityPurpose::ActiveControl)
            && !awaken_session_configuration_contract::resource::realization_lease_is_live_at(
                lease.expires_at_unix_ms,
                authority.now_ms,
            ))
    {
        return Err(RealizationHttpError::from(ApplicationError::invalid(
            "Session realization lease is not owned by the authenticated Worker incarnation",
        )));
    }
    session_realization::acquire_session_work_owner(
        service,
        session_id,
        &identity.lease_owner(),
        awaken_session_application_contract::SessionWorkAcquisition::RealizationRenewal,
    )
    .await?;
    Ok(())
}

/// Progress one exact blocked claim through the canonical independent-stop
/// application service. Physical admission, resolution backoff and terminal
/// recovery share this helper instead of growing separate stop protocols.
async fn observe_expired_predecessor(
    service: &WorkerDispatchService,
    successor: &RunClaim,
    request_budget_ms: u64,
) -> Result<Option<awaken_dispatch::WorkerProcessStopStatus>, ApplicationError> {
    // Absent only where no provider observes Worker processes; a blocked
    // successor then stays blocked (no stop is ever inferred).
    let Some(source) = &service.execution_stops else {
        return Ok(None);
    };
    let recovery = awaken_dispatch::PhysicalAttemptRecoveryRead::new(service.recovery.as_ref());
    tokio::time::timeout(
        std::time::Duration::from_millis(request_budget_ms.max(1)),
        awaken_dispatch::stop_expired_predecessor(
            service.dispatch.as_ref(),
            service.directory.as_ref(),
            source.as_ref(),
            Some(&recovery),
            successor,
            service.clock.now_ms(),
        ),
    )
    .await
    .map_err(|_| ApplicationError::unavailable("predecessor stop observation timed out"))?
}

/// Everything [`WorkerDispatchService::new`] consumes. Every collaborator is
/// required except the three whose provider exists only in some deployments.
pub struct RegisteredDispatchDependencies {
    /// Present only where a provider (systemd user units, Kubernetes Pods)
    /// observes Worker processes. Without it the unbound transport (the
    /// header-authenticated co-located Worker) freezes no physical binding;
    /// attempts are still admitted only to the current registered incarnation,
    /// under its Registry exclusion, exactly as bound attempts are.
    pub execution_bindings: Option<Arc<dyn awaken_dispatch::WorkerExecutionBindingSource>>,
    /// Present with the same providers. Without it no predecessor stop is ever
    /// observed, so a blocked successor stays blocked and closed-idle evidence
    /// is retained rather than released.
    pub execution_stops: Option<Arc<dyn awaken_dispatch::WorkerExecutionStopSource>>,
    pub dispatch: Arc<dyn WorkerDispatchAuthority>,
    pub checkpoint: Arc<dyn StreamCheckpointStore>,
    pub directory: Arc<dyn WorkerDirectory>,
    pub policy: Arc<dyn PlacementPolicy>,
    pub realizations: Arc<dyn awaken_session_execution_contract::SessionRealizationControl>,
    pub terminal_cleanup: Arc<dyn awaken_session_execution_contract::SessionTerminalCleanupControl>,
    pub session_preparation: Arc<dyn awaken_session_execution_contract::SessionPreparationControl>,
    pub session_environment_bindings:
        Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    pub coordination: Arc<dyn awaken_session_execution_contract::SessionAgentCoordination>,
    pub session_work: Arc<dyn awaken_session_application_contract::SessionWorkerLeaseAuthority>,
    pub authenticator: Arc<dyn WorkerRequestAuthenticator>,
    pub recovery: Arc<dyn RunRecoverySource>,
    pub completion: Arc<dyn CompletionSink>,
    pub terminal_observer: Arc<dyn DispatchSettlementObserver>,
    pub stream_sink: Arc<dyn StreamSink>,
    /// Present only in the cloud plane, which signs claim-scoped carriers.
    /// Without it a claim carries only its frozen envelope: an expired envelope
    /// and every refresh request are refused as unavailable.
    pub credential_deliveries:
        Option<Arc<dyn awaken_credential_contract::AttemptCredentialDeliverySource>>,
    /// The Coordinator's one wall clock: claim leases, registry freshness and
    /// Worker assertions are judged on the timeline every other owner uses.
    pub clock: Arc<dyn WallClock>,
    /// Claim lease duration granted to an authenticated Worker.
    pub lease_policy: Arc<dyn WorkerLeasePolicy>,
    /// Registry lease granted by registration and heartbeat.
    pub registry_ttl_ms: u64,
    /// The control-side crash-retry budget used by the dedicated terminal
    /// claim. The control plane, not the remote worker, owns this policy.
    pub max_attempts: u64,
}

/// Registry lease the Coordinator grants a Worker per registration/heartbeat.
pub const WORKER_REGISTRY_TTL_MS: u64 = 30_000;
/// Crash-retry budget the Coordinator applies before the terminal claim.
pub const WORKER_DISPATCH_MAX_ATTEMPTS: u64 = 5;

pub fn registered_dispatch_router(dependencies: RegisteredDispatchDependencies) -> Router {
    dispatch_transport_router_with_service(Arc::new(WorkerDispatchService::new(dependencies)))
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
        .route(
            "/v1/worker/dispatch/claim_retry_exhausted",
            post(claim_retry_exhausted),
        )
        .route("/v1/worker/dispatch/claim_run", post(claim_run))
        .route("/v1/worker/dispatch/renew", post(renew))
        .route("/v1/worker/dispatch/attempt/begin", post(begin_attempt))
        .route("/v1/worker/dispatch/attempt/handoff", post(handoff_attempt))
        .route("/v1/worker/dispatch/attempt/finish", post(finish_attempt))
        .route("/v1/worker/dispatch/attempt/close", post(close_attempt))
        .route("/v1/worker/dispatch/relinquish", post(relinquish))
        .route(
            "/v1/worker/dispatch/reservation/resolve",
            post(resolve_session_run_reservation),
        )
        .route("/v1/worker/dispatch/bind_sandbox", post(bind_sandbox))
        .route(
            "/v1/worker/dispatch/claim_is_current",
            post(claim_is_current),
        )
        .route(
            "/v1/worker/dispatch/credential_realization",
            post(record_credential_realization),
        )
        .route(
            "/v1/worker/dispatch/credential_delivery/refresh",
            post(refresh_credential_delivery),
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
            "/v1/worker/session/preparation/claim-next",
            post(session_preparation_claim_next),
        )
        .route(
            "/v1/worker/session/environment/authorize",
            post(authorize_session_environment_effect),
        )
        .route(
            "/v1/worker/session/environment/persist",
            post(persist_session_environment_receipt),
        )
        .route(
            "/v1/worker/session/cleanup/claim-next",
            post(session_cleanup_claim_next),
        )
        .route(
            "/v1/worker/session/cleanup/poll",
            post(session_cleanup_poll),
        )
        .route(
            "/v1/worker/session/cleanup/preparation/authorize",
            post(authorize_session_terminal_cleanup_preparation),
        )
        .route(
            "/v1/worker/session/cleanup/preparation/record",
            post(record_session_terminal_cleanup_preparation),
        )
        .route(
            "/v1/worker/session/cleanup/disposal/authorize",
            post(authorize_session_terminal_cleanup_disposal),
        )
        .route(
            "/v1/worker/session/cleanup/disposal/record",
            post(record_session_terminal_cleanup_disposal),
        )
        .route(
            "/v1/worker/session/cleanup/repository-publication/poll",
            post(session_repository_publication_poll),
        )
        .route(
            "/v1/worker/session/cleanup/repository-publication/complete",
            post(session_repository_publication_complete),
        )
        .route(
            "/v1/worker/session/cleanup/repository-publication/reject",
            post(session_repository_publication_reject),
        )
        .route("/v1/worker/session/agents/list", post(session_agents_list))
        .route(
            "/v1/worker/session/model-request/admit",
            post(session_model_request_admit),
        )
        .route(
            "/v1/worker/session/run-activity/admit",
            post(session_run_activity_admit),
        )
        .route("/v1/worker/session/agents/send", post(session_agent_send))
        .route(
            "/v1/worker/session/agents/settle",
            post(session_agent_settle),
        )
        .route(
            "/v1/worker/session/realization/renew",
            post(renew_session_realization),
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

pub fn respond(result: Result<Value, ApplicationError>) -> (StatusCode, Json<Value>) {
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
    Boundary(ApplicationError),
    Control(awaken_session_execution_contract::SessionRealizationControlFailure),
    Binding(awaken_runtime_contract::runs::RunError),
}

impl From<ApplicationError> for RealizationHttpError {
    fn from(error: ApplicationError) -> Self {
        Self::Boundary(error)
    }
}

impl From<awaken_session_execution_contract::SessionRealizationControlFailure>
    for RealizationHttpError
{
    fn from(error: awaken_session_execution_contract::SessionRealizationControlFailure) -> Self {
        Self::Control(error)
    }
}

impl From<awaken_runtime_contract::runs::RunError> for RealizationHttpError {
    fn from(error: awaken_runtime_contract::runs::RunError) -> Self {
        Self::Binding(error)
    }
}

fn respond_realization(result: Result<Value, RealizationHttpError>) -> (StatusCode, Json<Value>) {
    match result {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err(RealizationHttpError::Boundary(error)) => respond(Err(error)),
        Err(RealizationHttpError::Binding(error)) => {
            let status = match error.kind {
                awaken_runtime_contract::runs::RunErrorKind::BadRequest => StatusCode::BAD_REQUEST,
                awaken_runtime_contract::runs::RunErrorKind::Unavailable => {
                    StatusCode::SERVICE_UNAVAILABLE
                }
                awaken_runtime_contract::runs::RunErrorKind::Internal => {
                    StatusCode::INTERNAL_SERVER_ERROR
                }
                awaken_runtime_contract::runs::RunErrorKind::Conflict => StatusCode::CONFLICT,
            };
            (
                status,
                Json(json!({ "error": error.message, "code": error.code })),
            )
        }
        Err(RealizationHttpError::Control(error)) => {
            use awaken_session_execution_contract::SessionRealizationControlFailure;
            let status = match &error {
                SessionRealizationControlFailure::NotFound => StatusCode::NOT_FOUND,
                SessionRealizationControlFailure::NotReady
                | SessionRealizationControlFailure::Retired
                | SessionRealizationControlFailure::Terminal
                | SessionRealizationControlFailure::StaleOwnership
                | SessionRealizationControlFailure::Conflict => StatusCode::CONFLICT,
                SessionRealizationControlFailure::Invalid(_) => StatusCode::BAD_REQUEST,
                SessionRealizationControlFailure::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
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

// Work ownership can outlive a Run claim. Delay only the blocked Dispatch long
// enough for the pool to see independent Sessions; the store owns the deadline.
const SESSION_WORK_BUSY_RETRY_MS: u64 = 5_000;

/// Attach the exact subordinate Work authority to a newly claimed Session Run.
/// This is the only production issuer: it consumes the result of the canonical
/// acquisition port and never accepts caller-supplied Work coordinates.
async fn bind_claimed_session_work_authority(
    service: &WorkerDispatchService,
    authority: &ClaimAuthority,
    mut claimed: Option<awaken_dispatch::Claimed>,
) -> Result<Option<awaken_dispatch::Claimed>, ApplicationError> {
    let Some(claimed_run) = claimed.as_mut() else {
        return Ok(None);
    };
    if !claimed_run.request.requires_session_work_authority() {
        return Ok(claimed);
    }
    let session_id = claimed_run.request.session_thread_id().0.as_str();
    let receipt = awaken_session_application_contract::acquire_session_work_receipt(
        service.session_work.as_ref(),
        &awaken_session_types::SessionId::from(session_id),
        &authority.owner,
        awaken_session_application_contract::SessionWorkAcquisition::ClaimedRun,
    )
    .await;
    let receipt = match receipt {
        Ok(Some(receipt)) => receipt,
        Ok(None) => {
            let claim = RunClaim::from(&claimed_run.lease);
            service
                .dispatch
                .defer_claim(&claim, SESSION_WORK_BUSY_RETRY_MS)
                .await
                .map_err(|error| ApplicationError::unavailable(error.to_string()))?;
            tracing::debug!(run_id = %claim.run_id.0, session_id,
                retry_after_ms = SESSION_WORK_BUSY_RETRY_MS,
                "Session Work is held by another Worker; deferred only this Dispatch");
            return Ok(None);
        }
        Err(error) => {
            let claim = RunClaim::from(&claimed_run.lease);
            service
                .dispatch
                .relinquish_claim(&claim)
                .await
                .map_err(|rollback| ApplicationError::internal(rollback.to_string()))?;
            return Err(ApplicationError::unavailable(error.to_string()));
        }
    };
    claimed_run.session_work_authority = Some(receipt);
    Ok(claimed)
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
            return Err(ApplicationError::invalid(
                "authenticated worker does not own the claim",
            ));
        }
        let guard = service
            .dispatch
            .lock_commit_epoch(&request.claim)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
        let Some(guard) = guard else {
            return Ok(json!({ "current": false }));
        };
        if !guard.is_live_at(authority.now_ms) {
            return Ok(json!({ "current": false }));
        }
        let dispatch = guard.request();
        if dispatch.run_id() != &request.claim.run_id {
            return Err(ApplicationError::invalid(
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

async fn recovery_snapshot(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<RecoveryReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let authority =
            claim_authority(&service, &worker, request.identity.as_ref(), false).await?;
        if authority.owner != request.claim.owner {
            return Err(ApplicationError::invalid(
                "authenticated worker does not own the recovery claim",
            ));
        }
        let guard = service
            .dispatch
            .lock_commit_epoch(&request.claim)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?
            .ok_or_else(|| ApplicationError::conflict("run claim is stale"))?;
        let dispatch = guard.request();
        if dispatch.run_id() != &request.claim.run_id {
            return Err(ApplicationError::invalid(
                "guarded dispatch does not match the recovery claim",
            ));
        }
        let session = dispatch.session_thread_id();
        match (
            dispatch.requires_session_work_authority(),
            request.session_work_authority.as_ref(),
        ) {
            (false, None) => {}
            (false, Some(_)) => {
                return Err(ApplicationError::invalid(
                    "ordinary recovery cannot present Session Work authority",
                ));
            }
            (true, None) => {
                return Err(ApplicationError::invalid(
                    "recovery requires exact Session Work authority",
                ));
            }
            (true, Some(receipt)) => {
                if receipt.session_id().as_str() != session.0
                    || receipt.worker_owner() != authority.owner
                {
                    return Err(ApplicationError::invalid(
                        "Session recovery Work authority identity does not match the claim",
                    ));
                }
                let observed = service
                    .session_work
                    .acquire_session_work(
                        &awaken_session_types::SessionId::from(&session.0),
                        &authority.owner,
                        awaken_session_application_contract::SessionWorkAcquisition::RealizationRenewal,
                    )
                    .await
                    .map_err(|error| ApplicationError::unavailable(error.to_string()))?;
                if !receipt.matches_observation(&observed) {
                    return Err(ApplicationError::invalid(
                        "Session recovery Work authority is stale",
                    ));
                }
            }
        }
        let recovery = service
            .recovery
            .load_recovery(
                dispatch.session_thread_id(),
                dispatch.thread_id(),
                dispatch.run_id(),
                request.cached_prefix,
            )
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
        serde_json::to_value(recovery)
            .map_err(|error| ApplicationError::internal(error.to_string()))
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
            return Err(ApplicationError::invalid("sandbox claim owner is stale"));
        }
        if request.sandbox_ref.is_empty() {
            return Err(ApplicationError::invalid("sandbox reference is required"));
        }
        let outcome = service
            .dispatch
            .bind_sandbox(&request.claim, &request.sandbox_ref)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
        Ok(json!({ "applied": outcome.applied() }))
    }
    .await;
    respond(result)
}

async fn checkpoint_authority(
    service: &WorkerDispatchService,
    worker: &VerifiedWorkerContext,
    request: &CheckpointReq,
) -> Result<(), ApplicationError> {
    let authority = claim_authority(service, worker, request.identity.as_ref(), false).await?;
    if request.claim.owner != authority.owner {
        return Err(ApplicationError::invalid("checkpoint claim owner is stale"));
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
            .map_err(|error| ApplicationError::internal(error.to_string()))?
        else {
            return Ok(json!({ "checkpoint": null }));
        };
        // The wire carries only present-or-absent. An observation that
        // established nothing must therefore leave as a transport failure, so a
        // remote worker never decodes ignorance as a `null` checkpoint.
        match service.checkpoint.get(&request.claim.run_id.0).await {
            Observed::Present(checkpoint) => Ok(json!({ "checkpoint": checkpoint })),
            Observed::Absent => Ok(json!({ "checkpoint": null })),
            Observed::Unknown(cause) => Err(ApplicationError::internal(cause.to_string())),
        }
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
            .ok_or_else(|| ApplicationError::invalid("checkpoint payload is required"))?;
        if checkpoint.run_id != request.claim.run_id.0 {
            return Err(ApplicationError::invalid(
                "checkpoint run id does not match claim",
            ));
        }
        let Some(_guard) = service
            .dispatch
            .lock_commit_epoch(&request.claim)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?
        else {
            return Ok(json!({ "applied": false }));
        };
        service
            .checkpoint
            .put(checkpoint)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
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
            .map_err(|error| ApplicationError::internal(error.to_string()))?
        else {
            return Ok(json!({ "applied": false }));
        };
        service
            .checkpoint
            .delete(&request.claim.run_id.0)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
        Ok(json!({ "applied": true }))
    }
    .await;
    respond(result)
}

fn verify_worker_id(
    worker: &VerifiedWorkerContext,
    worker_id: &str,
) -> Result<(), ApplicationError> {
    if worker.worker_id() != worker_id {
        return Err(ApplicationError::invalid(
            "authenticated worker id does not match request identity",
        ));
    }
    Ok(())
}

/// The Coordinator's Worker Registry commands over this service's ports.
fn registry_commands(
    service: &WorkerDispatchService,
) -> awaken_dispatch::WorkerRegistryCommands<'_> {
    awaken_dispatch::WorkerRegistryCommands {
        directory: service.directory.as_ref(),
        dispatch: service.dispatch.as_ref(),
        bindings: service.execution_bindings.as_deref(),
        stops: service.execution_stops.as_deref(),
        clock: service.clock.as_ref(),
        ttl_ms: service.registry_ttl_ms,
    }
}

/// Closure and final physical admission share the Registry exclusion. A closed
/// exact incarnation cannot acquire another slot after this authoritative read.
async fn release_closed_idle_worker_evidence(
    service: &WorkerDispatchService,
    identity: &WorkerIdentity,
) -> Result<(), ApplicationError> {
    // Without a stop source the evidence is retained, never released.
    if service.execution_stops.is_none() {
        return Ok(());
    }
    registry_commands(service)
        .release_closed_idle_evidence(identity)
        .await
}

async fn settle(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SettleReq>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
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
            .map_err(|error| {
                RealizationHttpError::from(ApplicationError::internal(error.to_string()))
            })?;
        let Some(guard) = authorized else {
            return Ok(json!({ "settled": false }));
        };
        // No observer, Memory extraction, or Work release may run merely
        // because logical Ended truth exists while a predecessor still runs.
        if !guard.permits_settlement() {
            guard.release().await.map_err(|error| {
                RealizationHttpError::from(ApplicationError::unavailable(error))
            })?;
            // A committed-terminal recovery claim never re-enters physical
            // admission. Give it the same independent predecessor-stop
            // progression as a blocked begin, then require the ordinary settle
            // retry to reacquire and observe the exact slot as empty.
            let observation = observe_expired_predecessor(&service, &claim, authority.lease_ms)
                .await
                .map_err(RealizationHttpError::from)?;
            tracing::debug!(
                ?observation,
                "blocked settlement predecessor stop observation"
            );
            return Ok(json!({ "settled": false }));
        }
        let thread_id = guard.request().thread_id().clone();
        let session_thread_id = guard.request().session_thread_id().clone();
        let owns_session_work = thread_id == session_thread_id;
        let terminal_observer = (request.outcome == awaken_dispatch::DispatchOutcome::Done)
            .then_some(&service.terminal_observer);
        let coordinated_dispatch = guard.request().session_activity_epoch.is_some();
        // Save the exact guarded prefix once. Terminal publication and foreground
        // completion must not perform a second, post-settle recovery read that can
        // cross a lease epoch or lose the child Thread's physical Session owner.
        let committed_state = match service
            .recovery
            .recovery_snapshot_in_session(&session_thread_id, &thread_id, &claim.run_id)
            .await
        {
            Ok(snapshot)
                if snapshot.thread_id == thread_id && snapshot.claimed_run_id == claim.run_id =>
            {
                snapshot
                    .runs
                    .iter()
                    .find(|record| record.id == claim.run_id && record.thread_id == thread_id)
                    .map(|record| record.state.clone())
            }
            Ok(_) if terminal_observer.is_some() => {
                return Err(RealizationHttpError::from(ApplicationError::invalid(
                    "terminal settlement recovery does not match the guarded dispatch",
                )));
            }
            Ok(_) => None,
            Err(error) if terminal_observer.is_some() => {
                return Err(RealizationHttpError::from(ApplicationError::unavailable(
                    format!("read committed terminal settlement state: {error}"),
                )));
            }
            Err(_) => None,
        };
        if let Some(observer) = terminal_observer {
            let committed_state = committed_state.as_ref().ok_or_else(|| {
                RealizationHttpError::from(ApplicationError::invalid(
                    "terminal settlement recovery has no matching committed Run",
                ))
            })?;
            if !matches!(committed_state, RunState::Ended(_)) {
                return Err(RealizationHttpError::from(ApplicationError::invalid(
                    "Done settlement requires matching committed Ended truth",
                )));
            }
            // The settle HTTP request is transport activity and may carry no
            // trace context (or an unrelated retry context). The epoch-guarded
            // RunDispatch is the durable admission provenance, so rebuild its
            // canonical relay only around the post-commit observer that can
            // detach background work.
            let observation = awaken_observability::span_with_remote_parent(
                tracing::info_span!(
                    parent: None,
                    "dispatch.settlement.observe",
                    otel.kind = "internal"
                ),
                guard.request().traceparent.as_deref(),
            );
            observer
                .before_settle(
                    guard.request(),
                    &claim,
                    committed_state,
                    guard.cancellation_requested(),
                )
                .instrument(observation)
                .await
                .map_err(|error| {
                    RealizationHttpError::from(ApplicationError::unavailable(error.to_string()))
                })?;
        }
        // Work retirement revokes execution admission, not an already committed
        // terminal fact. Terminal cleanup retires Work before waiting for this
        // dispatch to quiesce; requiring the retired lease here would deadlock
        // both owners. Only exact claim-guarded Ended truth permits this closure.
        let committed_terminal = request.outcome == awaken_dispatch::DispatchOutcome::Done
            && matches!(committed_state.as_ref(), Some(RunState::Ended(_)));
        let session_boundary = if coordinated_dispatch {
            if let Some(session_activity_epoch) = guard.request().session_activity_epoch {
                let state = committed_state.as_ref().ok_or_else(|| {
                    RealizationHttpError::from(ApplicationError::invalid(
                        "coordinated settlement has no matching committed Run",
                    ))
                })?;
                let state_matches = matches!(
                    (request.outcome, state),
                    (
                        awaken_dispatch::DispatchOutcome::Awaiting,
                        RunState::Awaiting
                    ) | (awaken_dispatch::DispatchOutcome::Done, RunState::Ended(_))
                );
                if !state_matches {
                    return Err(RealizationHttpError::from(ApplicationError::invalid(
                        "coordinated settlement does not match committed Session activity truth",
                    )));
                }
                awaken_session_execution_contract::SessionAgentBoundaryCommand::from_claimed_boundary(
                    Some(session_activity_epoch),
                    Some(&session_thread_id),
                    (
                        guard.request().thread_id(),
                        guard.request().run_id(),
                        &guard.request().activation.snapshot.root_agent_id.0,
                    ),
                    &claim.run_id,
                    state,
                    guard.cancellation_requested(),
                )
                .map_err(|_| RealizationHttpError::from(ApplicationError::invalid(
                    "coordinated settlement does not match committed Session activity truth",
                )))?
            } else {
                None
            }
        } else {
            None
        };
        drop(guard);
        // Cause graph: a Work item is the Environment's single active ownership
        // fence, not a permanent reservation for every idle Session. Keeping a
        // root Session's Work active after its Run settles starves every queued
        // Session in that Environment. Release only the root after all fenced
        // commits have landed; a later activity revives and reacquires its stable
        // Work item. Child Runs borrow the parent's fence and must retain it.
        if owns_session_work {
            let session_work = &service.session_work;
            let lease = match session_work
                .acquire_session_work(
                    &awaken_session_types::SessionId::from(session_thread_id.0),
                    &claim.owner,
                    awaken_session_application_contract::SessionWorkAcquisition::RealizationRenewal,
                )
                .await
                .map_err(|error| {
                    RealizationHttpError::from(ApplicationError::internal(error.to_string()))
                })? {
                awaken_session_application_contract::SessionWorkOwnership::NotRequired => None,
                awaken_session_application_contract::SessionWorkOwnership::Leased(lease)
                    if lease.owner() == claim.owner =>
                {
                    Some(lease)
                }
                awaken_session_application_contract::SessionWorkOwnership::Unowned
                | awaken_session_application_contract::SessionWorkOwnership::Leased(_)
                    if committed_terminal =>
                {
                    // No Work mutation is authorized by another owner's lease.
                    // The subsequent Dispatch settle still checks its own epoch.
                    None
                }
                awaken_session_application_contract::SessionWorkOwnership::Unowned
                | awaken_session_application_contract::SessionWorkOwnership::Leased(_) => {
                    return Err(RealizationHttpError::from(ApplicationError::invalid(
                        "Session Work ownership was lost before Run settlement",
                    )));
                }
            };
            let released = match lease.as_ref() {
                Some(lease) => session_work
                    .release_session_work(lease)
                    .await
                    .map_err(|error| {
                        RealizationHttpError::from(ApplicationError::internal(error.to_string()))
                    })?,
                None => true,
            };
            if !released && !committed_terminal {
                return Err(RealizationHttpError::from(ApplicationError::invalid(
                    "Session Work ownership was lost before Run settlement",
                )));
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
            .map_err(|error| {
                RealizationHttpError::from(ApplicationError::internal(error.to_string()))
            })?;
        if outcome.applied()
            && let Some(state) = committed_state.as_ref()
        {
            service.completion.settled(&claim.run_id, state);
        }
        if outcome.applied()
            && let Some(command) = session_boundary
        {
            service
                .session_coordination
                .finalize_session_agent_boundary(command)
                .await
                .map_err(|error| {
                    RealizationHttpError::from(ApplicationError::unavailable(error.to_string()))
                })?;
        }
        Ok(json!({ "settled": outcome.applied() }))
    }
    .await;
    respond_realization(result)
}

async fn stream_event(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<StreamEventReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let stream_request = &request.request;
        let authority =
            claim_authority(&service, &worker, Some(&stream_request.identity), false).await?;
        if stream_request.claim.owner != authority.owner
            || stream_request.event.run_id != stream_request.claim.run_id
        {
            return Err(ApplicationError::invalid(
                "live Worker event does not match its authenticated claim",
            ));
        }
        if !awaken_agent_contract::event::classify(&stream_request.event.kind).live {
            return Err(ApplicationError::invalid(
                "Worker transport accepts only live-classified Agent events",
            ));
        }
        let current = service
            .dispatch
            .lock_commit_epoch(&stream_request.claim)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
        let Some(guard) = current else {
            return Ok(json!({ "accepted": false }));
        };
        let committed_thread = guard.request().thread_id();
        let observed_thread = request
            .assistant_response
            .as_ref()
            .map(|coordinate| &coordinate.thread_id);
        let provided_thread_mismatch =
            observed_thread.is_some_and(|thread_id| thread_id != committed_thread);
        if provided_thread_mismatch {
            return Err(ApplicationError::invalid(
                "live Worker event does not match its claim's logical Thread",
            ));
        }
        let _ = service
            .stream_sink
            .send_observation(awaken_agent_contract::stream::event::Observation {
                event: request.request.event,
                assistant_response: request.assistant_response,
            })
            .await;
        Ok(json!({ "accepted": true }))
    }
    .await;
    respond(result)
}

use awaken_dispatch_contract::{
    DispatchClaims, DispatchControlClaims, DispatchEnqueue, DispatchSettlement,
    SessionReservationAuthority,
};
/// Required server-side authority for authenticated Worker dispatch endpoints.
/// Administration, queue maintenance, inbox writes and reservation mutations are excluded.
pub trait WorkerDispatchAuthority:
    awaken_dispatch::AttemptControlAuthority
    + awaken_dispatch::PhysicalAttemptReads
    + DispatchEnqueue
    + DispatchClaims
    + DispatchSettlement
    + DispatchControlClaims
    + SessionReservationAuthority
{
}
impl<
    T: awaken_dispatch::AttemptControlAuthority
        + awaken_dispatch::PhysicalAttemptReads
        + DispatchEnqueue
        + DispatchClaims
        + DispatchSettlement
        + DispatchControlClaims
        + SessionReservationAuthority,
> WorkerDispatchAuthority for T
{
}

#[cfg(test)]
mod application_response_tests {
    use super::respond;
    use awaken_session_execution_contract::ApplicationError;
    use axum::{Json, http::StatusCode};
    use serde_json::json;

    #[test]
    fn application_response_preserves_original_success_and_each_error_cause() {
        // Cause/effect table for the one projection shared by Worker, Durable
        // and the Claimed Commit error arm. C1=Success or one of four error
        // kinds; C2=the message contains misleading kind-like words and quotes.
        // R0 Success => 200 with the entire original Value unchanged.
        // R1 InvalidRequest => 400; R2 Conflict => 409; R3 Internal => 500;
        // R4 Unavailable => 503. R1-R4 all preserve the exact message as the
        // single error field, regardless of C2. Strict object equality rejects
        // extra code/retry fields. The existing Durable/Worker/Claimed Commit
        // route tests retain authentication, no-mutation and receipt behavior.
        let original = json!({ "items": [1, { "ok": true }], "error": "business data" });
        let (status, Json(value)) = respond(Ok(original.clone()));
        assert_eq!(status, StatusCode::OK, "R0");
        assert_eq!(value, original, "R0 complete success value");
        const MESSAGE: &str = r#"invalid/conflict/internal/unavailable: keep "quotes" and bytes"#;
        for (rule, error, expected_status) in [
            (
                "R1",
                ApplicationError::invalid(MESSAGE),
                StatusCode::BAD_REQUEST,
            ),
            (
                "R2",
                ApplicationError::conflict(MESSAGE),
                StatusCode::CONFLICT,
            ),
            (
                "R3",
                ApplicationError::internal(MESSAGE),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                "R4",
                ApplicationError::unavailable(MESSAGE),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ] {
            let (status, Json(value)) = respond(Err(error));
            assert_eq!(status, expected_status, "{rule}");
            assert_eq!(
                value,
                json!({ "error": MESSAGE }),
                "{rule} exact one-field error"
            );
        }
    }
}

#[cfg(test)]
mod error_classification_tests;
