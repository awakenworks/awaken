//! Authenticated HTTP projection of Dispatch claim and attempt commands.
//!
//! The parent `WorkerDispatchService` remains the single application service;
//! these handlers add no queue, clock, identity, or lease authority.

use super::*;

pub(super) async fn enqueue(
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

pub(super) async fn claim_new_run(
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

pub(super) async fn deliver_and_claim(
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

pub(super) async fn claim(
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

pub(super) async fn claim_retry_exhausted(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<ClaimWorkerReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let authority = claim_authority(&service, &worker, request.identity.as_ref(), true).await?;
        let claimed = service
            .dispatch
            .claim_retry_exhausted(
                &authority.owner,
                authority.lease_ms,
                authority.now_ms,
                service.max_attempts,
            )
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "claimed": claimed }))
    }
    .await;
    respond(result)
}

pub(super) async fn claim_run(
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

pub(super) async fn renew(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<RenewReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let authority =
            claim_authority(&service, &worker, request.identity.as_ref(), false).await?;
        let outcome = service
            .dispatch
            .renew_lease(
                &RunClaim {
                    run_id: RunId(request.run_id),
                    owner: authority.owner,
                    epoch: request.lease_epoch,
                },
                authority.lease_ms,
                authority.now_ms,
            )
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(json!({ "outcome": outcome }))
    }
    .await;
    respond(result)
}

pub(super) async fn begin_attempt(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<AttemptExecutionReq>,
) -> (StatusCode, Json<Value>) {
    respond(admit_attempt(&service, &worker, &request, false).await)
}

pub(super) async fn handoff_attempt(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<AttemptExecutionReq>,
) -> (StatusCode, Json<Value>) {
    respond(admit_attempt(&service, &worker, &request, true).await)
}

// One authority/lookup sequence for ordinary admission and remote continuation.
// The Dispatch aggregate remains the only owner of their different transitions.
async fn admit_attempt(
    service: &WorkerDispatchService,
    worker: &VerifiedWorkerContext,
    request: &AttemptExecutionReq,
    handoff: bool,
) -> Result<Value, HostError> {
    let authority = claim_authority(service, worker, request.identity.as_ref(), false).await?;
    if authority.owner != request.claim.owner {
        return Err(HostError::bad_request(
            "authenticated Worker does not own the physical attempt claim",
        ));
    }
    let binding = if let Some(source) = &service.execution_bindings {
        // Reject invented/stale claims before expensive provider I/O. This is
        // only a preflight; the final transaction still owns execution admission.
        if !service
            .dispatch
            .claim_is_current(&request.claim, service.clock.now_ms())
            .await
            .map_err(|error| HostError::unavailable(error.to_string()))?
        {
            return Ok(json!({ "admission": awaken_dispatch::AttemptAdmission::Fenced }));
        }
        let snapshot = authority.snapshot.as_ref().ok_or_else(|| {
            HostError::bad_request("execution binding requires a registered Worker")
        })?;
        // No database transaction/Thread lock is held across provider I/O. The
        // request budget limits waiting, never manufactures an unbound fallback.
        let binding = tokio::time::timeout(
            std::time::Duration::from_millis(authority.lease_ms.max(1)),
            source.binding_for(&snapshot.identity),
        )
        .await
        .map_err(|_| HostError::unavailable("execution binding observation timed out"))??;
        if binding.worker() != &snapshot.identity {
            return Err(HostError::internal(
                "execution binding source returned another Worker incarnation",
            ));
        }
        binding
            .validate_for(&request.claim)
            .map_err(|e| HostError::internal(e.to_string()))?;
        // Registry replacement/expiry while the provider was awaited must not
        // reuse the first authentication snapshot. Dispatch separately rechecks
        // the exact claim and freezes the binding within its own transaction.
        claim_authority(service, worker, request.identity.as_ref(), false).await?;
        Some(binding)
    } else {
        None
    };
    let now_ms = service.clock.now_ms();
    let admission = if handoff {
        service
            .dispatch
            .handoff_attempt(&request.claim, now_ms, binding.as_ref())
            .await
    } else {
        service
            .dispatch
            .begin_attempt(&request.claim, now_ms, binding.as_ref())
            .await
    }
    .map_err(|error| match error {
        awaken_dispatch::DispatchError::Conflict(message) => HostError::conflict(message),
        error => HostError::internal(error.to_string()),
    })?;
    if admission == awaken_dispatch::AttemptAdmission::Blocked {
        let observation =
            observe_expired_predecessor(service, &request.claim, authority.lease_ms).await?;
        // This request was not admitted. Even after independently confirmed
        // retirement, return its original Blocked result: the ordinary retry
        // must reacquire current authority and atomically install its own slot.
        // Local-only stop sources retain occupancy through the Unknown default.
        tracing::debug!(?observation, "blocked predecessor process stop observation");
    }
    Ok(json!({ "admission": admission }))
}

pub(super) async fn finish_attempt(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<AttemptExecutionReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let identity = request.identity.as_ref().ok_or_else(|| {
            HostError::bad_request("registered Worker identity is required for quiescence")
        })?;
        // Unlike begin/commit, a quiescence ACK may arrive after the registry or
        // Run lease advanced. Authenticate the exact signed incarnation, then let
        // the Dispatch aggregate clear only its matching predecessor slot.
        verify_worker_identity(&worker, identity).map_err(HostError::bad_request)?;
        if identity.lease_owner() != request.claim.owner {
            return Err(HostError::bad_request(
                "authenticated Worker does not own the quiesced physical attempt",
            ));
        }
        let finished = service
            .dispatch
            .finish_attempt(&request.claim)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?
            .applied();
        Ok(json!({ "finished": finished }))
    }
    .await;
    respond(result)
}

pub(super) async fn relinquish(
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

pub(super) async fn resolve_session_run_reservation(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<ReservationResolutionReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let authority =
            claim_authority(&service, &worker, request.identity.as_ref(), false).await?;
        if authority.owner != request.claim.owner {
            return Err(HostError::bad_request(
                "authenticated Worker does not own the Session reservation claim",
            ));
        }
        let applied = service
            .dispatch
            .resolve_claimed_session_run_reservation(&request.claim, request.resolution)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?
            .applied();
        Ok(json!({ "applied": applied }))
    }
    .await;
    respond(result)
}
