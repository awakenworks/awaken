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
    FixedWorkerLeasePolicy, HeaderWorkerAuthenticator, VerifiedWorkerContext, WorkerLeasePolicy,
    WorkerRequestAuthenticator, authenticate_worker_request, verify_current_worker_identity,
    verify_worker_identity,
};

mod registry_endpoints;
use registry_endpoints::{
    deregister_worker, drain_worker, heartbeat_worker, quiesce_worker, register_worker,
};

mod run_dispatch_endpoints;
mod session_coordination;
mod session_realization;
mod session_requests;
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
            .map_err(ApplicationError::invalid)?;
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
    source: Arc<dyn awaken_session_realization_contract::WorkerSourceRetentionSubjects>,
    directory: Arc<dyn WorkerDirectory>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    clock: Arc<dyn WallClock>,
) -> Router {
    #[derive(Clone)]
    struct RetentionState {
        source: Arc<dyn awaken_session_realization_contract::WorkerSourceRetentionSubjects>,
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
            .map_err(ApplicationError::invalid)?;
            let subjects = state
                .source
                .source_retention_subjects(&request.identity)
                .await
                .map_err(|error| match error.kind {
                    awaken_session_types::RunErrorKind::BadRequest => {
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
            awaken_session_realization_contract::WORKER_SOURCE_RETENTION_SUBJECTS_PATH,
            post(subjects),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.authenticator.clone(),
            authenticate_worker_request,
        ))
        .with_state(state)
}

/// Explicit application service mounted by the worker HTTP adapter.
pub struct WorkerDispatchService {
    dispatch: Arc<dyn WorkerDispatchAuthority>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    clock: Arc<dyn WallClock>,
    lease_policy: Arc<dyn WorkerLeasePolicy>,
    directory: Option<Arc<dyn WorkerDirectory>>,
    placement_policy: Option<Arc<dyn PlacementPolicy>>,
    registry_ttl_ms: u64,
    checkpoint: Option<Arc<dyn StreamCheckpointStore>>,
    recovery: Option<Arc<dyn RunRecoverySource>>,
    completion: Option<Arc<dyn CompletionSink>>,
    terminal_observer: Option<Arc<dyn DispatchSettlementObserver>>,
    stream_sink: Option<Arc<dyn StreamSink>>,
    session_control:
        Option<Arc<dyn awaken_session_realization_contract::SessionRealizationControl>>,
    session_preparation_control:
        Option<Arc<dyn awaken_session_realization_contract::SessionPreparationControl>>,
    terminal_cleanup_control:
        Option<Arc<dyn awaken_session_realization_contract::SessionTerminalCleanupControl>>,
    session_environment_bindings:
        Option<Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>>,
    session_coordination:
        Option<Arc<dyn awaken_session_execution_contract::SessionAgentCoordination>>,
    session_work: Option<Arc<dyn awaken_session_application_contract::SessionWorkerLeaseAuthority>>,
    local_credential_capabilities: awaken_credential_contract::CredentialRealizationCapabilities,
    credential_deliveries:
        Option<Arc<dyn awaken_credential_contract::AttemptCredentialDeliverySource>>,
    max_attempts: u64,
    execution_bindings: Option<Arc<dyn awaken_dispatch::WorkerExecutionBindingSource>>,
    execution_stops: Option<Arc<dyn awaken_dispatch::WorkerExecutionStopSource>>,
}

impl WorkerDispatchService {
    #[must_use]
    pub fn new(
        dispatch: Arc<dyn WorkerDispatchAuthority>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
        clock: Arc<dyn WallClock>,
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
            terminal_observer: None,
            stream_sink: None,
            session_control: None,
            session_preparation_control: None,
            terminal_cleanup_control: None,
            session_environment_bindings: None,
            session_coordination: None,
            session_work: None,
            local_credential_capabilities: Default::default(),
            credential_deliveries: None,
            max_attempts: 5,
            execution_bindings: None,
            execution_stops: None,
        }
    }

    #[must_use]
    pub fn with_execution_binding_source(
        mut self,
        source: Arc<dyn awaken_dispatch::WorkerExecutionBindingSource>,
    ) -> Self {
        self.execution_bindings = Some(source);
        self
    }

    #[must_use]
    pub fn with_execution_stop_source(
        mut self,
        source: Arc<dyn awaken_dispatch::WorkerExecutionStopSource>,
    ) -> Self {
        self.execution_stops = Some(source);
        self
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

    /// Install the Coordinator-owned post-commit terminal publisher. It runs
    /// under the exact dispatch guard before a remote Done settlement can remove
    /// replay evidence or release Session Work.
    #[must_use]
    pub fn with_terminal_observer(mut self, observer: Arc<dyn DispatchSettlementObserver>) -> Self {
        self.terminal_observer = Some(observer);
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
        control: Arc<dyn awaken_session_realization_contract::SessionRealizationControl>,
    ) -> Self {
        self.session_control = Some(control);
        self
    }

    #[must_use]
    pub fn with_session_preparation_control(
        mut self,
        control: Arc<dyn awaken_session_realization_contract::SessionPreparationControl>,
    ) -> Self {
        self.session_preparation_control = Some(control);
        self
    }

    /// Install the independent terminal-cleanup projection and receipt role.
    #[must_use]
    pub fn with_terminal_cleanup_control(
        mut self,
        control: Arc<dyn awaken_session_realization_contract::SessionTerminalCleanupControl>,
    ) -> Self {
        self.terminal_cleanup_control = Some(control);
        self
    }

    /// Install the same Session root authority used by local Runtime Hosts.
    /// The Worker transport authenticates the realization lease; this port then
    /// performs the aggregate's one pre-effect authorization and receipt CAS.
    #[must_use]
    pub fn with_session_environment_bindings(
        mut self,
        bindings: Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    ) -> Self {
        self.session_environment_bindings = Some(bindings);
        self
    }

    /// Install another port view of the same Coordinator Session application
    /// for claim-fenced Agent coordination and settlement commands.
    #[must_use]
    pub fn with_session_coordination(
        mut self,
        coordination: Arc<dyn awaken_session_execution_contract::SessionAgentCoordination>,
    ) -> Self {
        self.session_coordination = Some(coordination);
        self
    }

    #[must_use]
    pub fn with_session_work_authority(
        mut self,
        authority: Arc<dyn awaken_session_application_contract::SessionWorkerLeaseAuthority>,
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
        capabilities: awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> Self {
        self.local_credential_capabilities = capabilities;
        self
    }

    /// Install the one Control-owned, claim-scoped credential transport source.
    /// It issues no authority: the durable binding, holder and epoch are already
    /// fixed before this port is called.
    #[must_use]
    pub fn with_credential_delivery_source(
        mut self,
        source: Arc<dyn awaken_credential_contract::AttemptCredentialDeliverySource>,
    ) -> Self {
        self.credential_deliveries = Some(source);
        self
    }

    /// Set the control-side crash-retry budget used by the dedicated terminal
    /// claim. The control plane, not the remote worker, owns this policy.
    #[must_use]
    pub fn with_max_attempts(mut self, max_attempts: u64) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }

    /// Local/test composition. Managed deployments should use [`Self::new`] with
    /// their WorkerLease/mTLS authenticator.
    #[must_use]
    pub fn local(dispatch: Arc<dyn WorkerDispatchAuthority>, clock: Arc<dyn WallClock>) -> Self {
        Self::new(
            dispatch,
            Arc::new(HeaderWorkerAuthenticator),
            clock,
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
) -> Result<ClaimAuthority, ApplicationError> {
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
        ApplicationError::invalid("registered worker identity is required for dispatch authority")
    })?;
    let snapshot =
        verify_current_worker_identity(directory.as_ref(), worker, identity, now_ms, require_ready)
            .await
            .map_err(ApplicationError::invalid)?;
    if !snapshot.manifest.supports_current_runtime_protocol() {
        return Err(ApplicationError::invalid(
            "Worker manifest does not explicitly support the current runtime protocol",
        ));
    }
    Ok(ClaimAuthority {
        owner: identity.lease_owner(),
        lease_ms,
        now_ms,
        snapshot: Some(snapshot),
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
            && !awaken_session_realization_contract::realization_lease_is_live_at(
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
        authority.now_ms,
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
    let Some(source) = &service.execution_stops else {
        return Ok(None);
    };
    let recovery = service
        .recovery
        .as_deref()
        .map(awaken_dispatch::PhysicalAttemptRecoveryRead::new);
    tokio::time::timeout(
        std::time::Duration::from_millis(request_budget_ms.max(1)),
        awaken_dispatch::stop_expired_predecessor(
            service.dispatch.as_ref(),
            directory(service)?.as_ref(),
            source.as_ref(),
            recovery.as_ref(),
            successor,
            service.clock.now_ms(),
        ),
    )
    .await
    .map_err(|_| ApplicationError::unavailable("predecessor stop observation timed out"))?
}

pub struct RegisteredDispatchDependencies {
    pub execution_bindings: Option<Arc<dyn awaken_dispatch::WorkerExecutionBindingSource>>,
    pub execution_stops: Option<Arc<dyn awaken_dispatch::WorkerExecutionStopSource>>,
    pub dispatch: Arc<dyn WorkerDispatchAuthority>,
    pub checkpoint: Arc<dyn StreamCheckpointStore>,
    pub directory: Arc<dyn WorkerDirectory>,
    pub policy: Arc<dyn PlacementPolicy>,
    pub realizations: Arc<dyn awaken_session_realization_contract::SessionRealizationControl>,
    pub terminal_cleanup:
        Arc<dyn awaken_session_realization_contract::SessionTerminalCleanupControl>,
    pub session_preparation:
        Arc<dyn awaken_session_realization_contract::SessionPreparationControl>,
    pub session_environment_bindings:
        Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    pub coordination: Arc<dyn awaken_session_execution_contract::SessionAgentCoordination>,
    pub session_work: Arc<dyn awaken_session_application_contract::SessionWorkerLeaseAuthority>,
    pub authenticator: Arc<dyn WorkerRequestAuthenticator>,
    pub recovery: Arc<dyn RunRecoverySource>,
    pub completion: Arc<dyn CompletionSink>,
    pub terminal_observer: Arc<dyn DispatchSettlementObserver>,
    pub stream_sink: Arc<dyn StreamSink>,
    pub credential_deliveries:
        Option<Arc<dyn awaken_credential_contract::AttemptCredentialDeliverySource>>,
    /// The Coordinator's one wall clock: claim leases, registry freshness and
    /// Worker assertions are judged on the timeline every other owner uses.
    pub clock: Arc<dyn WallClock>,
}

pub fn registered_dispatch_router(dependencies: RegisteredDispatchDependencies) -> Router {
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
    } = dependencies;
    let service = WorkerDispatchService::new(
        dispatch,
        authenticator,
        clock,
        Arc::new(FixedWorkerLeasePolicy::default()),
    )
    .with_worker_directory(directory, 30_000)
    .with_placement_policy(policy)
    .with_checkpoint_store(checkpoint)
    .with_recovery_source(recovery)
    .with_completion_sink(completion)
    .with_terminal_observer(terminal_observer)
    .with_stream_sink(stream_sink)
    .with_session_control(realizations)
    .with_session_preparation_control(session_preparation)
    .with_terminal_cleanup_control(terminal_cleanup)
    .with_session_environment_bindings(session_environment_bindings)
    .with_session_coordination(coordination)
    .with_session_work_authority(session_work);
    let service = match credential_deliveries {
        Some(source) => service.with_credential_delivery_source(source),
        None => service,
    };
    let service = match execution_bindings {
        Some(source) => service.with_execution_binding_source(source),
        None => service,
    };
    let service = match execution_stops {
        Some(source) => service.with_execution_stop_source(source),
        None => service,
    };
    dispatch_transport_router_with_service(Arc::new(service))
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
    Control(awaken_session_realization_contract::SessionRealizationControlFailure),
    Binding(awaken_session_types::RunError),
}

impl From<ApplicationError> for RealizationHttpError {
    fn from(error: ApplicationError) -> Self {
        Self::Boundary(error)
    }
}

impl From<awaken_session_realization_contract::SessionRealizationControlFailure>
    for RealizationHttpError
{
    fn from(error: awaken_session_realization_contract::SessionRealizationControlFailure) -> Self {
        Self::Control(error)
    }
}

impl From<awaken_session_types::RunError> for RealizationHttpError {
    fn from(error: awaken_session_types::RunError) -> Self {
        Self::Binding(error)
    }
}

fn respond_realization(result: Result<Value, RealizationHttpError>) -> (StatusCode, Json<Value>) {
    match result {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err(RealizationHttpError::Boundary(error)) => respond(Err(error)),
        Err(RealizationHttpError::Binding(error)) => {
            let status = match error.kind {
                awaken_session_types::RunErrorKind::BadRequest => StatusCode::BAD_REQUEST,
                awaken_session_types::RunErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
                awaken_session_types::RunErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
                awaken_session_types::RunErrorKind::Conflict => StatusCode::CONFLICT,
            };
            (
                status,
                Json(json!({ "error": error.message, "code": error.code })),
            )
        }
        Err(RealizationHttpError::Control(error)) => {
            use awaken_session_realization_contract::SessionRealizationControlFailure;
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

fn checkpoint_store(
    service: &WorkerDispatchService,
) -> Result<&Arc<dyn StreamCheckpointStore>, ApplicationError> {
    service
        .checkpoint
        .as_ref()
        .ok_or_else(|| ApplicationError::internal("worker checkpoint store is not configured"))
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
    let work = service
        .session_work
        .as_ref()
        .ok_or_else(|| ApplicationError::internal("Session Work authority is not configured"))?;
    let receipt = awaken_session_application_contract::acquire_session_work_receipt(
        work.as_ref(),
        &awaken_session_types::SessionId::from(session_id),
        &authority.owner,
        authority.now_ms,
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
                let work = service.session_work.as_ref().ok_or_else(|| {
                    ApplicationError::internal("Session Work authority is not configured")
                })?;
                let observed = work
                    .acquire_session_work(
                        &awaken_session_types::SessionId::from(&session.0),
                        &authority.owner,
                        authority.now_ms,
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
        let source = service
            .recovery
            .as_ref()
            .ok_or_else(|| ApplicationError::internal("worker recovery source is not configured"))?;
        let snapshot = source
            .recovery_snapshot_in_session(
                dispatch.session_thread_id(),
                dispatch.thread_id(),
                dispatch.run_id(),
            )
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
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
        match checkpoint_store(&service)?
            .get(&request.claim.run_id.0)
            .await
        {
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
        checkpoint_store(&service)?
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
        checkpoint_store(&service)?
            .delete(&request.claim.run_id.0)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
        Ok(json!({ "applied": true }))
    }
    .await;
    respond(result)
}

fn directory(
    service: &WorkerDispatchService,
) -> Result<&Arc<dyn WorkerDirectory>, ApplicationError> {
    service
        .directory
        .as_ref()
        .ok_or_else(|| ApplicationError::internal("worker directory is not configured"))
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

/// Closure and final physical admission share the Registry exclusion. A closed
/// exact incarnation cannot acquire another slot after this authoritative read.
async fn release_closed_idle_worker_evidence(
    service: &WorkerDispatchService,
    identity: &WorkerIdentity,
) -> Result<(), ApplicationError> {
    let Some(source) = &service.execution_stops else {
        return Ok(());
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        awaken_dispatch::release_closed_idle_worker_evidence(
            service.dispatch.as_ref(),
            directory(service)?.as_ref(),
            source.as_ref(),
            identity,
            service.clock.now_ms(),
        ),
    )
    .await
    .map_err(|_| ApplicationError::unavailable("idle Worker evidence release timed out"))?
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
        let terminal_observer = if request.outcome == awaken_dispatch::DispatchOutcome::Done {
            service.terminal_observer.as_ref()
        } else {
            None
        };
        let coordinated_dispatch = guard.request().session_activity_epoch.is_some()
            && service.session_coordination.is_some()
            && service.recovery.is_some();
        // Save the exact guarded prefix once. Terminal publication and foreground
        // completion must not perform a second, post-settle recovery read that can
        // cross a lease epoch or lose the child Thread's physical Session owner.
        let committed_state = if terminal_observer.is_some()
            || service.completion.is_some()
            || coordinated_dispatch
        {
            if let Some(recovery) = service.recovery.as_ref() {
                match recovery
                    .recovery_snapshot_in_session(&session_thread_id, &thread_id, &claim.run_id)
                    .await
                {
                    Ok(snapshot)
                        if snapshot.thread_id == thread_id
                            && snapshot.claimed_run_id == claim.run_id =>
                    {
                        snapshot
                            .runs
                            .iter()
                            .find(|record| {
                                record.id == claim.run_id && record.thread_id == thread_id
                            })
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
                }
            } else if terminal_observer.is_some() {
                return Err(RealizationHttpError::from(ApplicationError::internal(
                    "terminal settlement recovery source is not configured",
                )));
            } else {
                None
            }
        } else {
            None
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
        if owns_session_work && let Some(session_work) = service.session_work.as_ref() {
            let lease = match session_work
                .acquire_session_work(
                    &awaken_session_types::SessionId::from(session_thread_id.0),
                    &claim.owner,
                    authority.now_ms,
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
            && let Some(completion) = &service.completion
            && let Some(state) = committed_state.as_ref()
        {
            completion.settled(&claim.run_id, state);
        }
        if outcome.applied()
            && let (Some(command), Some(coordination)) =
                (session_boundary, service.session_coordination.as_ref())
        {
            coordination
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
        if let Some(sink) = &service.stream_sink {
            let _ = sink
                .send_observation(awaken_agent_contract::stream::event::Observation {
                    event: request.request.event,
                    assistant_response: request.assistant_response,
                })
                .await;
        }
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
