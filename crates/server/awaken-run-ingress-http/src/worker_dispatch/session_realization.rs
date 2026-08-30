//! Authenticated Session realization and terminal-cleanup HTTP handlers.

use super::*;

#[derive(Deserialize)]
pub(super) struct SessionResumeReq {
    claim: RunClaim,
    identity: WorkerIdentity,
    session_id: String,
}

#[derive(Deserialize)]
pub(super) struct SessionCleanupPollReq {
    identity: WorkerIdentity,
    session_id: String,
    lease: awaken_session_contract::SessionRealizationLease,
}

#[derive(Deserialize)]
pub(super) struct SessionCleanupClaimNextReq {
    identity: WorkerIdentity,
    target: awaken_session_contract::SessionRealizationTarget,
}

#[derive(Deserialize)]
pub(super) struct SessionCleanupPreparationRecordReq {
    identity: WorkerIdentity,
    lease: awaken_session_contract::SessionRealizationLease,
    preparation: awaken_session_contract::SessionCleanupPreparation,
}

#[derive(Deserialize)]
pub(super) struct SessionCleanupPreparationAuthorizeReq {
    identity: WorkerIdentity,
    effect: awaken_session_contract::SessionTerminalCleanupEffect,
}

#[derive(Deserialize)]
pub(super) struct SessionCleanupDisposalAuthorizeReq {
    identity: WorkerIdentity,
    effect: awaken_session_contract::SessionTerminalCleanupDisposalEffect,
}

#[derive(Deserialize)]
pub(super) struct SessionCleanupDisposalRecordReq {
    identity: WorkerIdentity,
    lease: awaken_session_contract::SessionRealizationLease,
    receipt: awaken_session_contract::SessionCleanupDisposalReceipt,
}

pub(super) async fn session_cleanup_claim_next(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SessionCleanupClaimNextReq>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        verify_worker_identity(&worker, &request.identity)
            .map_err(HostError::bad_request)
            .map_err(RealizationHttpError::from)?;
        let authority = claim_authority(&service, &worker, Some(&request.identity), false)
            .await
            .map_err(RealizationHttpError::from)?;
        require_terminal_cleanup_v2(&authority)?;
        let registry_expiry = authority
            .snapshot
            .as_ref()
            .ok_or_else(|| {
                RealizationHttpError::from(HostError::internal(
                    "Worker directory is required for Session cleanup recovery claims",
                ))
            })?
            .expires_at_ms;
        if request.target.reassign_existing_lease
            || request.target.owner != request.identity.worker_id
            || request.target.runtime_incarnation != request.identity.lease_owner()
            || !awaken_session_contract::realization_lease_is_live_at(
                request.target.lease_expires_at_unix_ms,
                authority.now_ms,
            )
            || request.target.lease_expires_at_unix_ms > registry_expiry
        {
            return Err(RealizationHttpError::from(HostError::bad_request(
                "Session cleanup recovery claim exceeds authenticated Worker authority",
            )));
        }
        let assignment = session_control(&service)
            .map_err(RealizationHttpError::from)?
            .claim_next_terminal_cleanup(request.target)
            .await
            .map_err(RealizationHttpError::from)?;
        Ok(json!({ "assignment": assignment }))
    }
    .await;
    respond_realization(result)
}

async fn verify_terminal_cleanup_authority(
    service: &WorkerDispatchService,
    worker: &VerifiedWorkerContext,
    identity: &WorkerIdentity,
    lease: &awaken_session_contract::SessionRealizationLease,
) -> Result<ClaimAuthority, RealizationHttpError> {
    verify_worker_identity(worker, identity)
        .map_err(HostError::bad_request)
        .map_err(RealizationHttpError::from)?;
    let authority = claim_authority(service, worker, Some(identity), false)
        .await
        .map_err(RealizationHttpError::from)?;
    require_terminal_cleanup_v2(&authority)?;
    // Terminal cleanup is allowed to outlive the ordinary realization expiry:
    // the Session fence prevents reassignment, while this exact generation and
    // the current authenticated Worker incarnation prevent a stale owner from
    // submitting effects. The Session aggregate rechecks the generation again.
    if lease.owner != identity.worker_id || lease.runtime_incarnation != identity.lease_owner() {
        return Err(RealizationHttpError::from(HostError::bad_request(
            "Session cleanup lease is not owned by the authenticated Worker incarnation",
        )));
    }
    Ok(authority)
}

fn require_terminal_cleanup_v2(authority: &ClaimAuthority) -> Result<(), RealizationHttpError> {
    let snapshot = authority.snapshot.as_ref().ok_or_else(|| {
        RealizationHttpError::from(HostError::internal(
            "Worker directory is required for terminal-cleanup transport v2",
        ))
    })?;
    if !snapshot.manifest.explicitly_supports_terminal_cleanup_v2() {
        return Err(RealizationHttpError::from(HostError::bad_request(
            "Worker manifest does not explicitly support terminal-cleanup transport v2",
        )));
    }
    Ok(())
}

pub(super) async fn session_cleanup_poll(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SessionCleanupPollReq>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        verify_terminal_cleanup_authority(&service, &worker, &request.identity, &request.lease)
            .await?;
        let work = session_control(&service)
            .map_err(RealizationHttpError::from)?
            .terminal_cleanup_work(&request.session_id, &request.lease)
            .await
            .map_err(RealizationHttpError::from)?;
        Ok(json!({ "work": work }))
    }
    .await;
    respond_realization(result)
}

pub(super) async fn authorize_session_terminal_cleanup_preparation(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SessionCleanupPreparationAuthorizeReq>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        verify_terminal_cleanup_authority(
            &service,
            &worker,
            &request.identity,
            &request.effect.lease,
        )
        .await?;
        let authorization = session_control(&service)
            .map_err(RealizationHttpError::from)?
            .authorize_terminal_cleanup_effect(&request.effect)
            .await
            .map_err(RealizationHttpError::from)?;
        Ok(json!({ "authorization": authorization }))
    }
    .await;
    respond_realization(result)
}

pub(super) async fn record_session_terminal_cleanup_preparation(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SessionCleanupPreparationRecordReq>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        verify_terminal_cleanup_authority(&service, &worker, &request.identity, &request.lease)
            .await?;
        session_control(&service)
            .map_err(RealizationHttpError::from)?
            .record_terminal_cleanup_preparation(&request.lease, request.preparation)
            .await
            .map_err(RealizationHttpError::from)?;
        Ok(json!({ "recorded": true }))
    }
    .await;
    respond_realization(result)
}

pub(super) async fn authorize_session_terminal_cleanup_disposal(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SessionCleanupDisposalAuthorizeReq>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        verify_terminal_cleanup_authority(
            &service,
            &worker,
            &request.identity,
            &request.effect.lease,
        )
        .await?;
        let workspace_id = session_control(&service)
            .map_err(RealizationHttpError::from)?
            .authorize_terminal_cleanup_disposal(&request.effect)
            .await
            .map_err(RealizationHttpError::from)?;
        Ok(json!({ "workspace_id": workspace_id }))
    }
    .await;
    respond_realization(result)
}

pub(super) async fn record_session_terminal_cleanup_disposal(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SessionCleanupDisposalRecordReq>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        verify_terminal_cleanup_authority(&service, &worker, &request.identity, &request.lease)
            .await?;
        session_control(&service)
            .map_err(RealizationHttpError::from)?
            .record_terminal_cleanup_disposal(&request.lease, request.receipt)
            .await
            .map_err(RealizationHttpError::from)?;
        Ok(json!({ "recorded": true }))
    }
    .await;
    respond_realization(result)
}

pub(super) async fn session_resume(
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
pub(super) struct SessionRealizationReq<T> {
    identity: WorkerIdentity,
    command: T,
}

#[derive(Deserialize)]
pub(super) struct SessionEnvironmentIntentReq {
    identity: WorkerIdentity,
    intent: awaken_session_contract::SessionEnvironmentEffectIntent,
}

#[derive(Deserialize)]
pub(super) struct SessionEnvironmentReceiptReq {
    identity: WorkerIdentity,
    receipt: awaken_session_contract::SessionEnvironmentReceipt,
}

pub(super) async fn authorize_session_environment_effect(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SessionEnvironmentIntentReq>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        let lease = request.intent.realization().ok_or_else(|| {
            RealizationHttpError::from(HostError::bad_request(
                "remote Session Environment effects require a realization lease",
            ))
        })?;
        verify_session_realization_authority(
            &service,
            &worker,
            &request.identity,
            request.intent.session_id(),
            lease,
        )
        .await?;
        let authorization = service
            .session_environment_bindings
            .as_ref()
            .ok_or_else(|| {
                RealizationHttpError::from(HostError::internal(
                    "Session Environment binding authority is not configured",
                ))
            })?
            .authorize(&request.intent)
            .await
            .map_err(RealizationHttpError::from)?;
        if matches!(
            authorization,
            awaken_session_contract::SessionEnvironmentEffectAuthorization::Unowned
        ) {
            return Err(RealizationHttpError::from(
                awaken_session_contract::RunError::classified(
                    "session_environment_unowned",
                    "authenticated Worker cannot realize a Session without a durable root",
                ),
            ));
        }
        Ok(json!({ "authorization": authorization }))
    }
    .await;
    respond_realization(result)
}

pub(super) async fn persist_session_environment_receipt(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SessionEnvironmentReceiptReq>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        let lease = request.receipt.realization.as_ref().ok_or_else(|| {
            RealizationHttpError::from(HostError::bad_request(
                "remote Session Environment receipts require a realization lease",
            ))
        })?;
        verify_session_realization_authority(
            &service,
            &worker,
            &request.identity,
            &request.receipt.session_id,
            lease,
        )
        .await?;
        let environment = service
            .session_environment_bindings
            .as_ref()
            .ok_or_else(|| {
                RealizationHttpError::from(HostError::internal(
                    "Session Environment binding authority is not configured",
                ))
            })?
            .persist(request.receipt)
            .await
            .map_err(RealizationHttpError::from)?;
        Ok(json!({ "environment": environment }))
    }
    .await;
    respond_realization(result)
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
    let authority = claim_authority(service, worker, Some(identity), false).await?;
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

pub(super) async fn renew_session_realization(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SessionRealizationReq<awaken_session_contract::RenewSessionRealization>>,
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
        let mut command = request.command;
        let asserted = &command.asserted_lease;
        if asserted.owner != request.identity.worker_id
            || asserted.runtime_incarnation != request.identity.lease_owner()
            || !awaken_session_contract::realization_lease_is_live_at(
                asserted.expires_at_unix_ms,
                authority.now_ms,
            )
            || !awaken_session_contract::realization_lease_is_live_at(
                command.requested_expires_at_unix_ms,
                authority.now_ms,
            )
        {
            return Err(RealizationHttpError::from(HostError::bad_request(
                "Session realization renewal does not match authenticated Worker authority",
            )));
        }
        // The registry is the authority boundary, while the Worker request is a
        // desired retention window. Network/SQLite delay between an independent
        // heartbeat and renewal must not turn a live Worker into a false loss of
        // authority. Preserve shorter requests and cap only their upper bound.
        command.requested_expires_at_unix_ms =
            command.requested_expires_at_unix_ms.min(registry_expiry);
        if command.requested_expires_at_unix_ms < command.asserted_lease.expires_at_unix_ms {
            return Err(RealizationHttpError::from(HostError::bad_request(
                "Worker registry authority cannot extend the asserted Session realization lease",
            )));
        }
        use awaken_session_contract::work_queue::SessionWorkOwnership;
        match session_work_ownership(
            &service,
            &command.session_id,
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
                    awaken_session_contract::SessionRealizationControlFailure::Retired,
                ));
            }
            SessionWorkOwnership::Leased(_) => {
                return Err(RealizationHttpError::Control(
                    awaken_session_contract::SessionRealizationControlFailure::StaleOwnership,
                ));
            }
        }
        let lease = session_control(&service)
            .map_err(RealizationHttpError::from)?
            .renew_session_realization(command)
            .await
            .map_err(RealizationHttpError::from)?;
        Ok(json!({ "lease": lease }))
    }
    .await;
    respond_realization(result)
}

pub(super) async fn activate_session_realization(
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
        .await?;
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

pub(super) async fn acknowledge_session_realization(
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
        .await?;
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

pub(super) async fn fail_session_realization(
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
        .await?;
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
