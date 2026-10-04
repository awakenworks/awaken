// @formal entrypoint credential.attempt_delivery_is_exact_epoch_bound_and_authority_preserving
// @formal evidence session_execution.one_physical_attempt_per_thread
//! Authenticated HTTP projection of Dispatch claim and attempt commands.
//!
//! The parent `WorkerDispatchService` remains the single application service;
//! these handlers add no queue, clock, identity, or lease authority.

use super::*;

/// Keep admission-owned resources alive after a caller drops its HTTP future.
fn complete_admission<F, T>(operation: F) -> tokio::task::JoinHandle<T>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    tokio::spawn(operation)
}

async fn attach_claim_credential_deliveries(
    service: &WorkerDispatchService,
    claimed: Option<awaken_dispatch::Claimed>,
) -> Result<Option<awaken_dispatch::Claimed>, HostError> {
    let Some(mut claimed) = claimed else {
        return Ok(None);
    };
    if claimed.credential_bindings.is_empty() {
        return Ok(Some(claimed));
    }
    let source = service.credential_deliveries.as_deref();
    let candidates = claimed
        .request
        .activation
        .snapshot
        .resolved_spec
        .attempt_candidates(claimed.request.activation.model_ref_override.as_deref());
    let mut deliveries = Vec::new();
    for candidate in candidates {
        let fingerprint = awaken_runtime_contract::candidate_fingerprint(candidate)
            .map_err(|error| HostError::internal(error.to_string()))?;
        let Some(binding) = claimed
            .credential_bindings
            .iter()
            .find(|binding| binding.candidate_fingerprint == fingerprint)
        else {
            continue;
        };
        let Some((published_access, material_binding)) =
            awaken_runtime_contract::candidate_credential_material_binding(candidate)
        else {
            continue;
        };
        if let Some(source) = source {
            let delivery = source
                .deliver(awaken_runtime_contract::AttemptCredentialDeliveryRequest {
                    candidate_fingerprint: fingerprint,
                    published_access: published_access.clone(),
                    selected_plaintext_holder: binding.selected_plaintext_holder.clone(),
                    binding: material_binding,
                    expected_provider: awaken_runtime_contract::candidate_provider_expectation(
                        candidate,
                    )
                    .map_err(HostError::internal)?,
                    claim_epoch: claimed.lease.epoch,
                })
                .await
                .map_err(HostError::unavailable)?;
            awaken_runtime_contract::verify_attempt_credential_delivery(
                binding,
                published_access,
                &delivery,
            )
            .map_err(|error| HostError::internal(error.to_string()))?;
            deliveries.push(delivery);
        } else if let Some(envelope) = &published_access.envelope {
            envelope
                .validate_for_holder(&binding.selected_plaintext_holder, service.clock.now_ms())
                .map_err(|_| {
                    HostError::unavailable(
                        "claim credential delivery expired and no delivery source is installed",
                    )
                })?;
        }
    }
    if source.is_some() && deliveries.len() != claimed.credential_bindings.len() {
        return Err(HostError::internal(
            "claim credential delivery did not cover every frozen binding",
        ));
    }
    claimed.credential_deliveries = deliveries;
    Ok(Some(claimed))
}

async fn finalize_claim(
    service: &WorkerDispatchService,
    authority: &ClaimAuthority,
    claimed: Option<awaken_dispatch::Claimed>,
) -> Result<Option<awaken_dispatch::Claimed>, HostError> {
    let claim = claimed
        .as_ref()
        .map(|claimed| awaken_dispatch::RunClaim::from(&claimed.lease));
    match attach_claim_credential_deliveries(service, claimed).await {
        Ok(claimed) => bind_claimed_session_work_authority(service, authority, claimed).await,
        Err(error) => {
            if let Some(claim) = claim {
                service
                    .dispatch
                    .relinquish_claim(&claim)
                    .await
                    .map_err(|release| HostError::unavailable(release.to_string()))?;
            }
            Err(error)
        }
    }
}

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
                .claim_new_run_compatible(request.request, snapshot, authority.lease_ms)
                .await
        } else {
            service
                .dispatch
                .claim_new_run(
                    request.request,
                    &authority.owner,
                    authority.lease_ms,
                    &service.local_credential_capabilities,
                )
                .await
        }
        .map_err(|error| HostError::internal(error.to_string()))?;
        let claimed = finalize_claim(&service, &authority, claimed).await?;
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
                .deliver_and_claim_compatible(request.input, snapshot, authority.lease_ms)
                .await
        } else {
            service
                .dispatch
                .deliver_and_claim(
                    request.input,
                    &authority.owner,
                    authority.lease_ms,
                    &service.local_credential_capabilities,
                )
                .await
        }
        .map_err(|error| HostError::internal(error.to_string()))?;
        let claimed = finalize_claim(&service, &authority, claimed).await?;
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
                    .claim_placed(snapshot, workers, policy.clone(), authority.lease_ms)
                    .await
            } else {
                service
                    .dispatch
                    .claim_compatible(snapshot, authority.lease_ms)
                    .await
            }
        } else {
            service
                .dispatch
                .claim(
                    &authority.owner,
                    authority.lease_ms,
                    &service.local_credential_capabilities,
                )
                .await
        }
        .map_err(|error| HostError::internal(error.to_string()))?;
        let claimed = finalize_claim(&service, &authority, claimed).await?;
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
            .claim_retry_exhausted(&authority.owner, authority.lease_ms, service.max_attempts)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        let claimed = finalize_claim(&service, &authority, claimed).await?;
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
                .claim_run_compatible(&run_id, snapshot, authority.lease_ms)
                .await
        } else {
            service
                .dispatch
                .claim_run(
                    &run_id,
                    &authority.owner,
                    authority.lease_ms,
                    &service.local_credential_capabilities,
                )
                .await
        }
        .map_err(|error| HostError::internal(error.to_string()))?;
        let claimed = finalize_claim(&service, &authority, claimed).await?;
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
            .claim_is_current(&request.claim)
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
    let incarnation_guard = if service.execution_bindings.is_some() {
        let identity = request.identity.as_ref().ok_or_else(|| {
            HostError::bad_request("registered Worker identity is required for physical admission")
        })?;
        let guard = super::directory(service)?
            .lock_incarnation(identity)
            .await
            .map_err(|e| HostError::unavailable(e.to_string()))?
            .ok_or_else(|| {
                HostError::bad_request("Worker incarnation changed before physical admission")
            })?;
        let snapshot = guard.snapshot();
        if snapshot.expires_at_ms <= service.clock.now_ms()
            || matches!(
                snapshot.state,
                awaken_dispatch::WorkerState::Quiesced | awaken_dispatch::WorkerState::Dead
            )
        {
            guard.release().await.map_err(HostError::unavailable)?;
            return Err(HostError::bad_request(
                "Worker incarnation closed before physical admission",
            ));
        }
        Some(guard)
    } else {
        None
    };
    // SQLite's blocking transaction survives HTTP cancellation. Move the
    // original Registry exclusion into the same completion task; dropping the
    // request must not release it ahead of a still-running slot installation.
    let dispatch = service.dispatch.clone();
    let claim = request.claim.clone();
    let admission = complete_admission(async move {
        let admission = if let Some(guard) = incarnation_guard {
            dispatch
                .admit_attempt_guarded(&claim, binding.as_ref(), guard, handoff)
                .await
        } else if handoff {
            dispatch.handoff_attempt(&claim, binding.as_ref()).await
        } else {
            dispatch.begin_attempt(&claim, binding.as_ref()).await
        };
        Ok::<_, HostError>(admission)
    })
    .await
    .map_err(|_| HostError::unavailable("physical admission completion unavailable"))??;
    let admission = admission.map_err(|error| match error {
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
        exact_attempt_return_identity(&worker, &request)?;
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
        let relinquished = match request.retry_after_ms {
            Some(retry_after_ms) => {
                service
                    .dispatch
                    .defer_claim(&request.claim, retry_after_ms)
                    .await
            }
            None => service.dispatch.relinquish_claim(&request.claim).await,
        }
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

fn exact_attempt_return_identity<'a>(
    worker: &VerifiedWorkerContext,
    request: &'a AttemptExecutionReq,
) -> Result<&'a WorkerIdentity, HostError> {
    let identity = request.identity.as_ref().ok_or_else(|| {
        HostError::bad_request("registered Worker identity is required for quiescence")
    })?;
    // Unlike begin/commit, a quiescence ACK may arrive after the registry or
    // Run lease advanced. Authenticate the exact signed incarnation, then let
    // the Dispatch aggregate clear only its matching predecessor slot.
    verify_worker_identity(worker, identity).map_err(HostError::bad_request)?;
    if identity.lease_owner() != request.claim.owner {
        return Err(HostError::bad_request(
            "authenticated Worker does not own the quiesced physical attempt",
        ));
    }
    Ok(identity)
}

pub(super) async fn close_attempt(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<AttemptExecutionReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let identity = exact_attempt_return_identity(&worker, &request)?;
        let directory = service
            .directory
            .as_ref()
            .ok_or_else(|| HostError::bad_request("entry closure requires a registered Worker"))?;
        let record = directory
            .incarnation(identity)
            .await
            .map_err(|error| HostError::unavailable(error.to_string()))?
            .ok_or_else(|| HostError::bad_request("Worker incarnation is unknown"))?;
        if !record.snapshot.manifest.supports_current_runtime_protocol() {
            return Err(HostError::bad_request(
                "Worker cannot attest the current attempt-closure protocol",
            ));
        }
        let closed = service
            .dispatch
            .close_attempt(&request.claim)
            .await
            .map_err(|error| HostError::unavailable(error.to_string()))?
            .applied();
        Ok(json!({"closed": closed}))
    }
    .await;
    respond(result)
}

#[cfg(test)]
mod admission_cancellation_tests {
    use super::complete_admission;
    use awaken_dispatch::{RegistryMutation, WorkerDirectory, WorkerRegistration};

    #[tokio::test]
    async fn cancelled_receipt_keeps_original_exclusion_until_admission_finishes() {
        // C1 production completion owner holds a real Registry guard; C2 HTTP
        // receipt is dropped; C3 underlying operation has not completed. R1
        // C1+C2+C3 -> closure stays queued. R2 actual completion -> release and
        // closure commits. This qualifies cancellation ownership, independently
        // of store conformance; it does not claim an actual SQLite write test.
        let directory = std::sync::Arc::new(awaken_worker_registry::MemoryWorkerDirectory::new());
        let identity = directory
            .register(
                WorkerRegistration {
                    worker_id: "cancelled-admission".into(),
                    incarnation_id: "boot".into(),
                    manifest: Default::default(),
                    create_ledger: None,
                },
                100,
                50,
            )
            .await
            .unwrap()
            .snapshot
            .identity;
        let guard = directory
            .lock_incarnation(&identity)
            .await
            .unwrap()
            .unwrap();
        let (finish, finished) = tokio::sync::oneshot::channel();
        let receipt = complete_admission(async move {
            finished.await.unwrap();
            guard.release().await.unwrap();
        });
        drop(receipt);
        let mut close = Box::pin(directory.deregister(&identity));
        std::future::poll_fn(|cx| {
            assert!(
                std::future::Future::poll(close.as_mut(), cx).is_pending(),
                "R1 original exclusion survives receipt loss"
            );
            std::task::Poll::Ready(())
        })
        .await;
        finish.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), close)
                .await
                .unwrap()
                .unwrap(),
            RegistryMutation::Applied,
            "R2"
        );
    }
}
