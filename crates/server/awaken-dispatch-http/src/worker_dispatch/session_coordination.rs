// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Static owner: HTTP adapter for claim-scoped Session coordination. Dynamic path:
// @formal reason: authenticated Worker requests carry the exact dispatch claim into the neutral
// @formal reason: coordination port and return its typed result. Evidence:
// @formal reason: worker.remote_execution.remote_protocol and signed transport tests; this is a
// @formal reason: product-requirement classification, not a direct proof claim.
//! Claim-fenced HTTP anti-corruption handlers for Session Agent coordination.
//!
//! This module owns no Session or dispatch state. It authenticates one existing
//! Worker claim and forwards the command to the authoritative Session application
//! port configured on `WorkerDispatchService`.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_dispatch::{RunClaim, WorkerIdentity};
use awaken_worker_transport_security::VerifiedWorkerContext;

use super::{
    ApplicationError, ClaimAuthority, RealizationHttpError, WorkerDispatchService, claim_authority,
    observe_expired_predecessor, respond_realization,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ClaimedSessionReq {
    claim: RunClaim,
    identity: WorkerIdentity,
    session_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ClaimedModelRequestReq {
    claim: RunClaim,
    identity: WorkerIdentity,
    session_id: String,
    thread_id: awaken_agent_contract::agent::thread::Id,
    run_id: RunId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ClaimedRunActivityReq {
    claim: RunClaim,
    identity: WorkerIdentity,
    session_id: String,
    agent_id: String,
    run_id: RunId,
    mode: awaken_session_execution_contract::SessionRunActivityAdmissionMode,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ClaimedSessionMessageReq {
    claim: RunClaim,
    identity: WorkerIdentity,
    command: awaken_session_execution_contract::SessionAgentMessageCommand,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ClaimedSessionBoundaryReq {
    claim: RunClaim,
    identity: WorkerIdentity,
    command: awaken_session_execution_contract::SessionAgentBoundaryCommand,
}

async fn claimed_session_authority(
    service: &WorkerDispatchService,
    worker: &VerifiedWorkerContext,
    identity: &WorkerIdentity,
    claim: &RunClaim,
    session_id: &str,
) -> Result<(ClaimAuthority, awaken_dispatch::CommitEpochGuard), RealizationHttpError> {
    // This guard proves the authenticated claim and trusted Session affinity at
    // command admission. Callers deliberately drop it before invoking the
    // Session application: send/report re-enter this same
    // dispatch store and holding its physical transaction would deadlock. Every
    // mutating command carries deterministic Run/operation/message coordinates,
    // so a lease handoff after admission can only cause the successor to replay
    // the same idempotent application effect; queue settlement remains fenced by
    // the original claim epoch.
    let authority = claim_authority(service, worker, Some(identity), false)
        .await
        .map_err(RealizationHttpError::from)?;
    if authority.owner != claim.owner {
        return Err(RealizationHttpError::from(ApplicationError::invalid(
            "authenticated worker does not own the Session claim",
        )));
    }
    let guard = service
        .dispatch
        .lock_commit_epoch(claim)
        .await
        .map_err(|error| ApplicationError::internal(error.to_string()))
        .map_err(RealizationHttpError::from)?
        .ok_or_else(|| {
            RealizationHttpError::from(ApplicationError::invalid("Session claim is stale"))
        })?;
    if !guard.is_live_at(authority.now_ms) {
        return Err(RealizationHttpError::from(ApplicationError::invalid(
            "Session claim lease has expired",
        )));
    }
    if guard.request().run_id() != &claim.run_id {
        return Err(RealizationHttpError::from(ApplicationError::invalid(
            "guarded dispatch does not match the Session claim",
        )));
    }
    if guard.request().session_thread_id().0 != session_id {
        return Err(RealizationHttpError::from(ApplicationError::invalid(
            "Session target does not match the claimed Run",
        )));
    }
    Ok((authority, guard))
}

fn require_primary_session_claim(
    guard: &awaken_dispatch::CommitEpochGuard,
) -> Result<(), RealizationHttpError> {
    let dispatch = guard.request();
    if dispatch.thread_id() != dispatch.session_thread_id() {
        return Err(RealizationHttpError::from(ApplicationError::invalid(
            "Agent coordination tools require the claimed primary Session Run",
        )));
    }
    Ok(())
}

fn session_coordination(
    service: &WorkerDispatchService,
) -> Result<
    &Arc<dyn awaken_session_execution_contract::SessionAgentCoordination>,
    RealizationHttpError,
> {
    service.session_coordination.as_ref().ok_or_else(|| {
        RealizationHttpError::from(ApplicationError::internal(
            "Session coordination control is not configured",
        ))
    })
}

fn coordination_failure(
    error: awaken_session_types::RunError,
) -> awaken_session_realization_contract::SessionRealizationControlFailure {
    match error.kind {
        awaken_session_types::RunErrorKind::BadRequest => {
            awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(
                error.message,
            )
        }
        awaken_session_types::RunErrorKind::Conflict => {
            awaken_session_realization_contract::SessionRealizationControlFailure::Conflict
        }
        awaken_session_types::RunErrorKind::Internal
        | awaken_session_types::RunErrorKind::Unavailable => {
            awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(
                error.message,
            )
        }
    }
}

pub(super) async fn session_agents_list(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<ClaimedSessionReq>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        let (_, guard) = claimed_session_authority(
            &service,
            &worker,
            &request.identity,
            &request.claim,
            &request.session_id,
        )
        .await?;
        require_primary_session_claim(&guard)?;
        drop(guard);
        let agents = session_coordination(&service)?
            .list_session_agents(&awaken_session_types::SessionId::from(request.session_id))
            .await
            .map_err(coordination_failure)?;
        Ok(json!({ "agents": agents }))
    }
    .await;
    respond_realization(result)
}

pub(super) async fn session_model_request_admit(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<ClaimedModelRequestReq>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        let (_, guard) = claimed_session_authority(
            &service,
            &worker,
            &request.identity,
            &request.claim,
            &request.session_id,
        )
        .await?;
        if request.run_id != request.claim.run_id
            || request.run_id != *guard.request().run_id()
            || request.thread_id != *guard.request().thread_id()
        {
            return Err(RealizationHttpError::from(ApplicationError::invalid(
                "model-request admission does not match the claimed Session dispatch",
            )));
        }
        // Unlike message/report commands this application call does not enqueue
        // work. Keep the exact claim guard alive across root usage
        // reconciliation so a superseded Worker cannot spend another request.
        let admitted = session_coordination(&service)?
            .admit_session_model_request(
                &awaken_session_types::SessionId::from(request.session_id),
                &request.thread_id,
                &request.run_id,
            )
            .await
            .map_err(coordination_failure)?;
        drop(guard);
        Ok(json!({ "admitted": admitted }))
    }
    .await;
    respond_realization(result)
}

pub(super) async fn session_run_activity_admit(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<ClaimedRunActivityReq>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        let authority = claim_authority(&service, &worker, Some(&request.identity), false)
            .await
            .map_err(RealizationHttpError::from)?;
        if authority.owner != request.claim.owner {
            return Err(RealizationHttpError::from(ApplicationError::invalid(
                "authenticated Worker does not own the Session reservation claim",
            )));
        }
        let guard = service
            .dispatch
            .lock_session_run_reservation_epoch(&request.claim)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))
            .map_err(RealizationHttpError::from)?
            .ok_or_else(|| {
                RealizationHttpError::from(ApplicationError::invalid(
                    "Session Run reservation claim is stale",
                ))
            })?;
        let dispatch = guard.request();
        let expected_mode = if guard.cancellation_requested() {
            awaken_session_execution_contract::SessionRunActivityAdmissionMode::RecoverOnly
        } else {
            awaken_session_execution_contract::SessionRunActivityAdmissionMode::RecoverOrAdmit
        };
        if !guard.is_live_at(authority.now_ms)
            || request.run_id != request.claim.run_id
            || request.run_id != *dispatch.run_id()
            || dispatch.thread_id() != dispatch.session_thread_id()
            || dispatch.thread_id().0 != request.session_id
            || dispatch.session_activity_epoch.is_some()
            || dispatch.activation.snapshot.root_agent_id.0 != request.agent_id
            || request.mode != expected_mode
        {
            return Err(RealizationHttpError::from(ApplicationError::invalid(
                "Session Run activity admission does not match the claimed reservation",
            )));
        }
        // The exact Session operation is derived from the guarded Run identity.
        // Keep this narrow reservation guard across the root CAS: cancellation
        // cannot delete the row between a RecoverOrAdmit read and its receipt
        // commit. The guard is released before the Worker's separate queue
        // resolution transaction.
        let admission = session_coordination(&service)?
            .admit_session_run_activity(
                &awaken_session_types::SessionId::from(request.session_id),
                &request.agent_id,
                &request.run_id,
                request.mode,
            )
            .await
            .map_err(coordination_failure)?;
        drop(guard);
        Ok(json!({ "admission": admission }))
    }
    .await;
    respond_realization(result)
}

pub(super) async fn session_agent_send(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<ClaimedSessionMessageReq>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        let (_, guard) = claimed_session_authority(
            &service,
            &worker,
            &request.identity,
            &request.claim,
            request.command.session_id.as_str(),
        )
        .await?;
        require_primary_session_claim(&guard)?;
        if request.command.source_run_id != request.claim.run_id
            || request.command.source_thread_id != *guard.request().thread_id()
        {
            return Err(RealizationHttpError::from(ApplicationError::invalid(
                "Agent message source does not match the claimed primary Run",
            )));
        }
        drop(guard);
        let receipt = session_coordination(&service)?
            .send_session_agent_message(request.command)
            .await
            .map_err(coordination_failure)?;
        Ok(json!({ "receipt": receipt }))
    }
    .await;
    respond_realization(result)
}

pub(super) async fn session_agent_settle(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<ClaimedSessionBoundaryReq>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        let (authority, guard) = claimed_session_authority(
            &service,
            &worker,
            &request.identity,
            &request.claim,
            request.command.session_id.as_str(),
        )
        .await?;
        if !guard.permits_settlement() {
            guard.release().await.map_err(|error| {
                RealizationHttpError::from(ApplicationError::unavailable(error))
            })?;
            // A remote Dispatch observer reaches this route before the Worker's
            // final `/dispatch/settle`. Progress an independently terminated
            // predecessor through the same application service used by attempt
            // admission and final settlement; the observer retry remains the
            // only path that can advance Session state.
            let observation =
                observe_expired_predecessor(&service, &request.claim, authority.lease_ms)
                    .await
                    .map_err(RealizationHttpError::from)?;
            tracing::debug!(
                ?observation,
                "blocked Session observer predecessor stop observation"
            );
            return Err(RealizationHttpError::from(ApplicationError::unavailable(
                "Session boundary is waiting for predecessor physical quiescence",
            )));
        }
        let dispatch = guard.request();
        if request.command.source_run_id != request.claim.run_id
            || request.command.source_thread_id != *dispatch.thread_id()
            || request.command.source_agent_id != dispatch.activation.snapshot.root_agent_id.0
            || Some(request.command.session_activity_epoch) != dispatch.session_activity_epoch
            || request.command.cancellation_requested != guard.cancellation_requested()
        {
            return Err(RealizationHttpError::from(ApplicationError::invalid(
                "Agent settlement does not match the claimed Session dispatch",
            )));
        }
        drop(guard);
        session_coordination(&service)?
            .settle_session_agent_boundary(request.command)
            .await
            .map_err(coordination_failure)?;
        Ok(json!({ "settled": true }))
    }
    .await;
    respond_realization(result)
}
