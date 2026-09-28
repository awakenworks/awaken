// @formal entrypoint session_run.execution_requires_dispatch_work_and_realization_owner
// @formal entrypoint session_run.stale_work_epoch_cannot_observe_authoritative_thread_commit
//! Authenticated Session realization and terminal-cleanup HTTP handlers.

use super::*;

#[derive(Deserialize)]
pub(super) struct SessionResumeReq {
    claim: RunClaim,
    identity: WorkerIdentity,
    session_id: String,
    session_work_authority: Option<awaken_session_work_contract::SessionWorkAuthorityReceipt>,
    continuation: Option<awaken_session_resource_contract::SessionRealizationLease>,
}

#[derive(Deserialize)]
pub(super) struct SessionCleanupPollReq {
    pub(super) identity: WorkerIdentity,
    pub(super) session_id: String,
    pub(super) lease: awaken_session_resource_contract::SessionRealizationLease,
}

#[derive(Deserialize)]
pub(super) struct SessionCleanupClaimNextReq {
    identity: WorkerIdentity,
    target: awaken_session_realization_contract::SessionRealizationTarget,
}

#[derive(Deserialize)]
pub(super) struct SessionCleanupPreparationRecordReq {
    identity: WorkerIdentity,
    lease: awaken_session_resource_contract::SessionRealizationLease,
    preparation: awaken_session_state_contract::SessionCleanupPreparation,
}

#[derive(Deserialize)]
pub(super) struct SessionCleanupPreparationAuthorizeReq {
    identity: WorkerIdentity,
    effect: awaken_session_state_contract::SessionTerminalCleanupEffect,
}

#[derive(Deserialize)]
pub(super) struct SessionCleanupDisposalAuthorizeReq {
    identity: WorkerIdentity,
    effect: awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
}

#[derive(Deserialize)]
pub(super) struct SessionCleanupDisposalRecordReq {
    identity: WorkerIdentity,
    lease: awaken_session_resource_contract::SessionRealizationLease,
    receipt: awaken_session_state_contract::SessionCleanupDisposalReceipt,
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
        let registry_expiry = terminal_cleanup_snapshot(&authority)?.expires_at_ms;
        if request.target.reassign_existing_lease
            || request.target.owner != request.identity.worker_id
            || request.target.runtime_incarnation != request.identity.lease_owner()
            || !awaken_session_realization_contract::realization_lease_is_live_at(
                request.target.lease_expires_at_unix_ms,
                authority.now_ms,
            )
        {
            return Err(RealizationHttpError::from(HostError::bad_request(
                "Session cleanup recovery claim exceeds authenticated Worker authority",
            )));
        }
        // The Worker knows its desired Session proof window but not the exact
        // server-side registry expiry after heartbeat transport latency. Reuse
        // the same authority projection as ordinary realization renewal: the
        // authenticated boundary narrows the proposal before the aggregate sees
        // it. Rejecting an otherwise exact proposal here creates a periodic
        // recovery blackout near every registry-renewal edge.
        let mut target = request.target;
        target.lease_expires_at_unix_ms = target.lease_expires_at_unix_ms.min(registry_expiry);
        let assignment = terminal_cleanup_control(&service)
            .map_err(RealizationHttpError::from)?
            .claim_next_terminal_cleanup(target)
            .await
            .map_err(RealizationHttpError::from)?;
        Ok(json!({ "assignment": assignment }))
    }
    .await;
    respond_realization(result)
}

pub(super) async fn verify_terminal_cleanup_authority(
    service: &WorkerDispatchService,
    worker: &VerifiedWorkerContext,
    identity: &WorkerIdentity,
    lease: &awaken_session_resource_contract::SessionRealizationLease,
) -> Result<ClaimAuthority, RealizationHttpError> {
    verify_worker_identity(worker, identity)
        .map_err(HostError::bad_request)
        .map_err(RealizationHttpError::from)?;
    let authority = claim_authority(service, worker, Some(identity), false)
        .await
        .map_err(RealizationHttpError::from)?;
    terminal_cleanup_snapshot(&authority)?;
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

fn terminal_cleanup_snapshot(
    authority: &ClaimAuthority,
) -> Result<&WorkerSnapshot, RealizationHttpError> {
    // claim_authority already enforces the canonical current protocol floor.
    // Terminal effects additionally forbid the no-directory local/test path.
    authority.snapshot.as_ref().ok_or_else(|| {
        RealizationHttpError::from(HostError::internal(
            "Worker directory is required for terminal-cleanup transport",
        ))
    })
}

pub(super) async fn session_cleanup_poll(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SessionCleanupPollReq>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        verify_terminal_cleanup_authority(&service, &worker, &request.identity, &request.lease)
            .await?;
        let work = terminal_cleanup_control(&service)
            .map_err(RealizationHttpError::from)?
            .terminal_cleanup_work(
                &awaken_session_types::SessionId::from(request.session_id),
                &request.lease,
            )
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
        let authorization = terminal_cleanup_control(&service)
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
        terminal_cleanup_control(&service)
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
        let workspace_id = terminal_cleanup_control(&service)
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
        terminal_cleanup_control(&service)
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
        let expected_resources = dispatch
            .session_resources
            .as_ref()
            .map(awaken_dispatch::SessionResourceEnvelope::decode_manifest)
            .transpose()
            .map_err(|error| {
                RealizationHttpError::from(HostError::bad_request(error.to_string()))
            })?;
        let receipt = request.session_work_authority.as_ref().ok_or_else(|| {
            RealizationHttpError::from(HostError::bad_request(
                "Session resume requires exact Work authority",
            ))
        })?;
        if receipt.session_id().as_str() != request.session_id
            || receipt.worker_owner() != request.identity.lease_owner()
        {
            return Err(RealizationHttpError::from(HostError::bad_request(
                "Session resume Work authority identity does not match the claim",
            )));
        }
        let observed = session_work_ownership(
            &service,
            &awaken_session_types::SessionId::from(&request.session_id),
            &request.identity.lease_owner(),
            authority.now_ms,
            awaken_session_work_contract::SessionWorkAcquisition::RealizationRenewal,
        )
        .await
        .map_err(RealizationHttpError::from)?;
        if !receipt.matches_observation(&observed) {
            return Err(RealizationHttpError::from(HostError::bad_request(
                "Session resume Work authority is stale",
            )));
        }
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
        if let Some(asserted) = request.continuation.as_ref() {
            if asserted.owner != request.identity.worker_id
                || asserted.runtime_incarnation != request.identity.lease_owner()
                || asserted.expires_at_unix_ms > registry_expiry
            {
                return Err(RealizationHttpError::from(HostError::bad_request(
                    "Session continuation exceeds authenticated Worker authority",
                )));
            }
            // Reuse the existing exact-fence renewal CAS. Stale continuation is
            // never converted into a reassignment; a cold/retired Worker must
            // explicitly take the ordinary assignment path without this proof.
            let renewed = control
                .renew_session_realization(awaken_session_realization_contract::RenewSessionRealization {
                    session_id: awaken_session_types::SessionId::from(
                        request.session_id.clone(),
                    ),
                    asserted_lease: asserted.clone(),
                    requested_expires_at_unix_ms: realization_expiry
                        .max(asserted.expires_at_unix_ms),
                })
                .await
                .map_err(RealizationHttpError::Control)?;
            if !awaken_session_realization_contract::realization_lease_generation_authorizes(
                &renewed.lease,
                asserted,
            ) {
                return Err(RealizationHttpError::Control(
                    awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership,
                ));
            }
        }
        let realization = match control
            .begin_session_realization(awaken_session_realization_contract::BeginSessionRealization {
                session_id: awaken_session_types::SessionId::from(&request.session_id),
                expected_resources,
                target: awaken_session_realization_contract::SessionRealizationTarget {
                    owner: request.identity.worker_id.clone(),
                    runtime_incarnation: request.identity.lease_owner(),
                    lease_expires_at_unix_ms: realization_expiry,
                    // The exact live dispatch guard above proves this Worker is
                    // the sole executor for the Session thread. It may therefore
                    // fence a predecessor Worker's longer-lived projection lease
                    // without waiting for an unrelated timeout.
                    // Without an exact live continuation, even the same process
                    // needs a successor: Work retirement can close its local
                    // predecessor while the durable root lease is still live.
                    reassign_existing_lease: request.continuation.is_none(),
                },
            })
            .await
        {
            Ok(realization)
                if receipt.binds_realization(
                    &request.session_id,
                    &realization.authority.lease.runtime_incarnation,
                    realization.projection.baseline.environment.self_hosted,
                    &realization.projection.baseline.environment.environment_id,
                ) =>
            {
                Some(realization)
            }
            Ok(_) => {
                return Err(RealizationHttpError::from(HostError::bad_request(
                    "Session realization directive does not match claimed Work authority",
                )));
            }
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::NotReady) => {
                return Err(RealizationHttpError::Control(
                    awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
                ));
            }
            Err(error) => return Err(RealizationHttpError::Control(error)),
        };
        // Renewal and projection building are separate existing operations.
        // Reassignment between them must not publish another generation as the
        // requested continuation, even if owner and incarnation are unchanged.
        if let (Some(asserted), Some(directive)) = (&request.continuation, &realization)
            && !awaken_session_realization_contract::realization_lease_generation_authorizes(
                &directive.authority.lease,
                asserted,
            )
        {
            return Err(RealizationHttpError::Control(
                awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership,
            ));
        }
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
    intent: awaken_session_state_contract::SessionEnvironmentEffectIntent,
}

#[derive(Deserialize)]
pub(super) struct SessionEnvironmentReceiptReq {
    identity: WorkerIdentity,
    receipt: awaken_session_state_contract::SessionEnvironmentReceipt,
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
            request.intent.session_id().as_str(),
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
            awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Unowned
        ) {
            return Err(RealizationHttpError::from(
                awaken_session_error_contract::RunError::classified(
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
            request.receipt.session_id.as_str(),
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
    lease: &awaken_session_resource_contract::SessionRealizationLease,
) -> Result<(), RealizationHttpError> {
    verify_worker_identity(worker, identity)
        .map_err(HostError::bad_request)
        .map_err(RealizationHttpError::from)?;
    let authority = claim_authority(service, worker, Some(identity), false).await?;
    if lease.owner != identity.worker_id
        || lease.runtime_incarnation != identity.lease_owner()
        || !awaken_session_realization_contract::realization_lease_is_live_at(
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
        awaken_session_work_contract::SessionWorkAcquisition::RealizationRenewal,
    )
    .await?;
    Ok(())
}

async fn acquire_session_work_owner(
    service: &WorkerDispatchService,
    session_id: &str,
    worker_owner: &str,
    now_ms: u64,
    acquisition: awaken_session_work_contract::SessionWorkAcquisition,
) -> Result<(), RealizationHttpError> {
    let ownership = session_work_ownership(
        service,
        &awaken_session_types::SessionId::from(session_id),
        worker_owner,
        now_ms,
        acquisition,
    )
    .await
    .map_err(RealizationHttpError::from)?;
    if ownership.permits_effect_for(worker_owner) {
        Ok(())
    } else {
        Err(RealizationHttpError::Control(
            awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
        ))
    }
}

async fn session_work_ownership(
    service: &WorkerDispatchService,
    session_id: &awaken_session_types::SessionId,
    worker_owner: &str,
    now_ms: u64,
    acquisition: awaken_session_work_contract::SessionWorkAcquisition,
) -> Result<awaken_session_work_contract::SessionWorkOwnership, HostError> {
    let authority = service
        .session_work
        .as_ref()
        .ok_or_else(|| HostError::internal("Session Work authority is not configured"))?;
    authority
        .acquire_session_work(session_id, worker_owner, now_ms, acquisition)
        .await
        .map_err(|error| HostError::internal(error.to_string()))
}

pub(super) fn session_control(
    service: &WorkerDispatchService,
) -> Result<&Arc<dyn awaken_session_realization_contract::SessionRealizationControl>, HostError> {
    service
        .session_control
        .as_ref()
        .ok_or_else(|| HostError::internal("Session control is not configured"))
}

pub(super) fn terminal_cleanup_control(
    service: &WorkerDispatchService,
) -> Result<&Arc<dyn awaken_session_realization_contract::SessionTerminalCleanupControl>, HostError>
{
    service
        .terminal_cleanup_control
        .as_ref()
        .ok_or_else(|| HostError::internal("Session terminal cleanup control is not configured"))
}

pub(super) async fn renew_session_realization(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<
        SessionRealizationReq<awaken_session_realization_contract::RenewSessionRealization>,
    >,
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
            || !awaken_session_realization_contract::realization_lease_is_live_at(
                asserted.expires_at_unix_ms,
                authority.now_ms,
            )
            || !awaken_session_realization_contract::realization_lease_is_live_at(
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
        let ownership = session_work_ownership(
            &service,
            &command.session_id,
            &request.identity.lease_owner(),
            authority.now_ms,
            awaken_session_work_contract::SessionWorkAcquisition::RealizationRenewal,
        )
        .await
        .map_err(RealizationHttpError::from)?;
        if !ownership.permits_effect_for(&request.identity.lease_owner()) {
            // A settled Run retires its self-hosted Session Work before the
            // longer-lived local realization lease next comes due. That is a
            // normal retirement proof, not evidence that another Worker stole
            // authority. Preserve the typed lifecycle result so the Worker
            // quietly revokes only its stale process-local projection.
            let failure = match ownership {
                awaken_session_work_contract::SessionWorkOwnership::Unowned => {
                    awaken_session_realization_contract::SessionRealizationControlFailure::Retired
                }
                awaken_session_work_contract::SessionWorkOwnership::Leased(_) => {
                    awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership
                }
                awaken_session_work_contract::SessionWorkOwnership::NotRequired => {
                    awaken_session_realization_contract::SessionRealizationControlFailure::NotReady
                }
            };
            return Err(RealizationHttpError::Control(failure));
        }
        let lease = session_control(&service)
            .map_err(RealizationHttpError::from)?
            .renew_session_realization(command)
            .await
            .map_err(RealizationHttpError::from)?;
        Ok(json!(lease))
    }
    .await;
    respond_realization(result)
}

pub(super) async fn activate_session_realization(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<
        SessionRealizationReq<awaken_session_realization_contract::ActivateSessionRealization>,
    >,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        verify_session_realization_authority(
            &service,
            &worker,
            &request.identity,
            request.command.session_id.as_str(),
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
        SessionRealizationReq<awaken_session_realization_contract::AcknowledgeSessionRealization>,
    >,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        verify_session_realization_authority(
            &service,
            &worker,
            &request.identity,
            request.command.session_id.as_str(),
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
    Json(request): Json<
        SessionRealizationReq<awaken_session_realization_contract::FailSessionRealization>,
    >,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        verify_session_realization_authority(
            &service,
            &worker,
            &request.identity,
            request.command.session_id.as_str(),
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
