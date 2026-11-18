// @formal entrypoint credential.attempt_delivery_is_exact_epoch_bound_and_authority_preserving
// @formal evidence session_execution.one_physical_attempt_per_thread
//! Authenticated HTTP projection of Dispatch claim and attempt commands.
//!
//! The parent `WorkerDispatchService` remains the single application service;
//! these handlers add no queue, clock, identity, or lease authority.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;
use awaken_credential_contract::CredentialEnvelopeValidation;

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
) -> Result<Option<awaken_dispatch::Claimed>, ApplicationError> {
    let Some(mut claimed) = claimed else {
        return Ok(None);
    };
    if claimed.credential_bindings.is_empty() {
        return Ok(Some(claimed));
    }
    let source = service.credential_grants.provider.as_deref();
    let candidates = claimed
        .request
        .activation
        .snapshot
        .resolved_spec
        .attempt_candidates(claimed.request.activation.model_ref_override.as_deref());
    let mut deliveries = Vec::new();
    for candidate in candidates {
        let fingerprint = awaken_credential_contract::candidate_fingerprint(candidate)
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
        let Some(binding) = claimed
            .credential_bindings
            .iter()
            .find(|binding| binding.candidate_fingerprint == fingerprint)
        else {
            continue;
        };
        let Some(request) = super::credential_grant::provider_delivery_request(
            candidate,
            binding,
            &awaken_dispatch::RunClaim::from(&claimed.lease),
        )?
        else {
            continue;
        };
        if let Some(source) = source {
            let delivery = source
                .deliver(request.clone())
                .await
                .map_err(ApplicationError::unavailable)?;
            awaken_credential_contract::verify_attempt_credential_delivery(
                binding,
                &request.published_access,
                &delivery,
            )
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
            deliveries.push(delivery);
        } else if let Some(envelope) = &request.published_access.envelope {
            envelope
                .validate_for_holder(&binding.selected_plaintext_holder, service.clock.now_ms())
                .map_err(|_| {
                    ApplicationError::unavailable(
                        "claim credential delivery expired and no delivery source is installed",
                    )
                })?;
        }
    }
    if source.is_some() && deliveries.len() != claimed.credential_bindings.len() {
        return Err(ApplicationError::internal(
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
) -> Result<Option<awaken_dispatch::Claimed>, ApplicationError> {
    let claim = claimed
        .as_ref()
        .map(|claimed| awaken_dispatch::RunClaim::from(&claimed.lease));
    match attach_claim_credential_deliveries(service, claimed).await {
        Ok(claimed) => {
            bind_claimed_session_work_authority(service, authority, claimed, false).await
        }
        Err(error) => {
            if let Some(claim) = claim {
                service
                    .dispatch
                    .relinquish_claim(&claim)
                    .await
                    .map_err(|release| ApplicationError::unavailable(release.to_string()))?;
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
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
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
        let claimed = service
            .dispatch
            .claim_new_run_compatible(request.request, &authority.snapshot, authority.lease_ms)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
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
        let claimed = service
            .dispatch
            .deliver_and_claim_compatible(request.input, &authority.snapshot, authority.lease_ms)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
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
        let workers = service
            .directory
            .list()
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?
            .into_iter()
            .map(|record| record.snapshot)
            .collect();
        let claimed = service
            .dispatch
            .claim_placed(
                &authority.snapshot,
                workers,
                service.placement_policy.clone(),
                authority.lease_ms,
            )
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
        let claimed = finalize_claim(&service, &authority, claimed).await?;
        Ok(json!({ "claimed": claimed }))
    }
    .await;
    respond(result)
}

pub(super) async fn claim_for_request(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<awaken_dispatch_contract::ClaimWorkRequest>,
) -> (StatusCode, Json<Value>) {
    use awaken_dispatch_contract::ClaimRequestOutcome;

    let result = async {
        let id = &request.claim.request_id;
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(ApplicationError::invalid("invalid claim request id"));
        }
        let (authority, outcome) = wait_for_claim_request(&service, &worker, &request).await?;
        match outcome {
            ClaimRequestOutcome::Unsupported => Ok(None),
            ClaimRequestOutcome::Empty => Ok(Some(json!({ "claimed": null }))),
            ClaimRequestOutcome::Claimed(claimed) => {
                // Leave the persisted receipt live across a transient response
                // preparation error so the same request can finish preparation.
                let claim = attach_claim_credential_deliveries(&service, Some(*claimed)).await?;
                let claim =
                    bind_claimed_session_work_authority(&service, &authority, claim, true).await?;
                Ok(Some(json!({ "claimed": claim })))
            }
        }
    }
    .await;
    match result {
        Ok(None) => (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "replayable claim unsupported" })),
        ),
        Ok(Some(response)) => respond(Ok(response)),
        Err(error) => respond(Err(error)),
    }
}

async fn search_claim_request(
    service: &WorkerDispatchService,
    worker: &VerifiedWorkerContext,
    request: &awaken_dispatch_contract::ClaimWorkRequest,
    search: awaken_dispatch_contract::ClaimSearch,
) -> Result<
    (
        super::ClaimAuthority,
        awaken_dispatch_contract::ClaimRequestOutcome,
    ),
    ApplicationError,
> {
    use awaken_dispatch_contract::{ClaimSearch, DispatchError};
    let authority = claim_authority(service, worker, request.identity.as_ref(), true).await?;
    let workers = if request.claim.run_id.is_none() && search != ClaimSearch::Replay {
        service
            .directory
            .list()
            .await
            .map_err(|error| ApplicationError::unavailable(error.to_string()))?
            .into_iter()
            .map(|record| record.snapshot)
            .collect()
    } else {
        Vec::new()
    };
    let result = if search == ClaimSearch::Any {
        service
            .dispatch
            .claim_for_request(
                &request.claim,
                &authority.snapshot,
                workers,
                service.placement_policy.clone(),
                authority.lease_ms,
            )
            .await
    } else {
        service
            .dispatch
            .search_claim_request(
                &request.claim,
                &authority.snapshot,
                workers,
                service.placement_policy.clone(),
                authority.lease_ms,
                search,
            )
            .await
    };
    let outcome = result.map_err(|error| match error {
        DispatchError::Rejected(message) => ApplicationError::invalid(message),
        DispatchError::Conflict(message) => ApplicationError::conflict(message),
        DispatchError::Unavailable(message) => ApplicationError::unavailable(message),
    })?;
    Ok((authority, outcome))
}

async fn wait_for_claim_request(
    service: &WorkerDispatchService,
    worker: &VerifiedWorkerContext,
    request: &awaken_dispatch_contract::ClaimWorkRequest,
) -> Result<
    (
        super::ClaimAuthority,
        awaken_dispatch_contract::ClaimRequestOutcome,
    ),
    ApplicationError,
> {
    use awaken_dispatch_contract::{ClaimRequestOutcome, ClaimSearch};
    let wait_ms = request.wait_ms.unwrap_or_default().min(2_000);
    if wait_ms == 0 || request.claim.run_id.is_some() {
        return search_claim_request(service, worker, request, ClaimSearch::Any).await;
    }

    let deadline = awaken_time::mono::now() + std::time::Duration::from_millis(wait_ms);
    let replay = search_claim_request(service, worker, request, ClaimSearch::Replay).await?;
    if matches!(replay.1, ClaimRequestOutcome::Unsupported) {
        return search_claim_request(service, worker, request, ClaimSearch::Any).await;
    }
    if !matches!(replay.1, ClaimRequestOutcome::Empty) {
        return Ok(replay);
    }
    loop {
        let changed = service.ready_shards.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let hint = service
            .ready_shards
            .observe(service.dispatch.as_ref())
            .await
            .map_err(|error| ApplicationError::unavailable(error.to_string()))?;
        let Some(shards) = hint else {
            return search_claim_request(service, worker, request, ClaimSearch::Any).await;
        };
        for shard in shards.into_iter().take(4) {
            if awaken_time::mono::now() >= deadline {
                break;
            }
            let Some(permit) = service.ready_shards.try_claim(shard) else {
                continue;
            };
            let outcome =
                search_claim_request(service, worker, request, ClaimSearch::Shard(shard)).await?;
            drop(permit);
            if matches!(outcome.1, ClaimRequestOutcome::Claimed(_)) {
                // Start the next idle Worker draining the same cached backlog
                // as soon as this claim frees capacity in the shard.
                service.ready_shards.changed.notify_one();
            }
            if !matches!(outcome.1, ClaimRequestOutcome::Empty) {
                return Ok(outcome);
            }
        }
        if awaken_time::mono::now() >= deadline {
            // A sibling Coordinator may have published the receipt during the
            // wait, including after the last candidate was removed.
            return search_claim_request(service, worker, request, ClaimSearch::Replay).await;
        }
        tokio::select! {
            () = changed => {}
            () = tokio::time::sleep_until(deadline.min(awaken_time::mono::now() + super::ready_shards::REFRESH)) => {}
        }
    }
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
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
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
        let claimed = service
            .dispatch
            .claim_run_compatible(&run_id, &authority.snapshot, authority.lease_ms)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
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
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
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
) -> Result<Value, ApplicationError> {
    if request.return_cause.is_some() {
        return Err(ApplicationError::invalid(
            "only physical finish accepts a return cause",
        ));
    }
    let authority = claim_authority(service, worker, request.identity.as_ref(), false).await?;
    if authority.owner != request.claim.owner {
        return Err(ApplicationError::invalid(
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
            .map_err(|error| ApplicationError::unavailable(error.to_string()))?
        {
            return Ok(json!({ "admission": awaken_dispatch::AttemptAdmission::Fenced }));
        }
        let snapshot = &authority.snapshot;
        // No database transaction/Thread lock is held across provider I/O. The
        // request budget limits waiting, never manufactures an unbound fallback.
        let binding = tokio::time::timeout(
            std::time::Duration::from_millis(authority.lease_ms.max(1)),
            source.binding_for(&snapshot.identity),
        )
        .await
        .map_err(|_| ApplicationError::unavailable("execution binding observation timed out"))??;
        if binding.worker() != &snapshot.identity {
            return Err(ApplicationError::internal(
                "execution binding source returned another Worker incarnation",
            ));
        }
        binding
            .validate_for(&request.claim)
            .map_err(|e| ApplicationError::internal(e.to_string()))?;
        // Registry replacement/expiry while the provider was awaited must not
        // reuse the first authentication snapshot. Dispatch separately rechecks
        // the exact claim and freezes the binding within its own transaction.
        claim_authority(service, worker, request.identity.as_ref(), false).await?;
        Some(binding)
    } else {
        None
    };
    // Every physical admission, bound or not, installs its slot under the
    // original Registry exclusion: a quiesce, deregistration or replacement
    // either precedes this lock (and is refused below) or waits for the
    // installation. `claim_authority` above read the Registry without it.
    let identity = request.identity.as_ref().ok_or_else(|| {
        ApplicationError::invalid("registered Worker identity is required for physical admission")
    })?;
    let guard = service
        .directory
        .lock_incarnation(identity)
        .await
        .map_err(|e| ApplicationError::unavailable(e.to_string()))?
        .ok_or_else(|| {
            ApplicationError::invalid("Worker incarnation changed before physical admission")
        })?;
    // Early refusal with this boundary's message only. Dispatch applies the
    // same standing law on its own clock while the guard is still held.
    if !guard
        .snapshot()
        .standing(service.clock.now_ms())
        .holds_authority()
    {
        guard
            .release()
            .await
            .map_err(ApplicationError::unavailable)?;
        return Err(ApplicationError::invalid(
            "Worker incarnation closed before physical admission",
        ));
    }
    // SQLite's blocking transaction survives HTTP cancellation. Move the
    // original Registry exclusion into the same completion task; dropping the
    // request must not release it ahead of a still-running slot installation.
    let dispatch = service.dispatch.clone();
    let claim = request.claim.clone();
    let admission = complete_admission(async move {
        let admission = dispatch
            .admit_attempt_guarded(&claim, binding.as_ref(), guard, handoff)
            .await;
        Ok::<_, ApplicationError>(admission)
    })
    .await
    .map_err(|_| ApplicationError::unavailable("physical admission completion unavailable"))??;
    let admission = admission.map_err(|error| match error {
        awaken_dispatch::DispatchError::Conflict(message) => ApplicationError::conflict(message),
        awaken_dispatch::DispatchError::Unavailable(message) => {
            ApplicationError::unavailable(message)
        }
        error => ApplicationError::internal(error.to_string()),
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
        let identity = exact_attempt_return_identity(&worker, &request)?;
        if request.return_cause.is_some() {
            require_attempt_observation_protocol(&service, identity).await?;
        }
        let finished = service
            .dispatch
            .finish_attempt(&request.claim, request.return_cause)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?
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
            return Err(ApplicationError::invalid(
                "authenticated worker does not own the relinquished claim",
            ));
        }
        if request.attempt_failed && request.retry_after_ms.is_none() {
            return Err(ApplicationError::invalid(
                "a failed attempt is released behind a retry deadline",
            ));
        }
        // Session resolution can fail before physical begin/handoff is reached.
        // Reuse its exact-claim stop application while this mutation lease is
        // still current; otherwise an occupied predecessor can prevent reaching
        // the only recovery opportunity on every subsequent resolution retry.
        // Keep the result until after release: provider failure/timeout must not
        // strand this claim, and Unknown must not clear physical occupancy.
        let observation = if request.retry_after_ms.is_some() {
            observe_expired_predecessor(&service, &request.claim, authority.lease_ms).await
        } else {
            Ok(None)
        };
        // Read the slot after the stop observation, which may have emptied it,
        // and before the release that ends this claim's locked view of it.
        let parked = match (request.retry_after_ms, observation.is_ok()) {
            (Some(_), true) => park_root_session_work(&service, &request.claim).await,
            _ => Ok(()),
        };
        let relinquished = match (request.retry_after_ms, request.attempt_failed) {
            (Some(retry_after_ms), true) => {
                service
                    .dispatch
                    .retry_claim(&request.claim, retry_after_ms, service.max_attempts)
                    .await
            }
            (Some(retry_after_ms), false) => {
                service
                    .dispatch
                    .defer_claim(&request.claim, retry_after_ms)
                    .await
            }
            (None, _) => service.dispatch.relinquish_claim(&request.claim).await,
        }
        .map_err(|error| ApplicationError::internal(error.to_string()))?
        .applied();
        let observation = observation?;
        tracing::debug!(?observation, "backoff predecessor process stop observation");
        parked?;
        Ok(json!({ "relinquished": relinquished }))
    }
    .await;
    respond(result)
}

/// Whether a deferred claim parks its Session's Environment Work.
///
/// Two fences exist and stay separate. The Dispatch attempt slot is the
/// physical fence of the Run's Thread (ADR-0024, ADR-0065): it stays occupied
/// until real stop evidence empties it, and nothing here touches it. The
/// Session's Work item is the Environment's single active admission (ADR-0075
/// D2); a writable or exclusive Environment depends on it so that no two
/// Sessions act inside it at once. Releasing the Work while the slot is
/// occupied is therefore permitted only when nothing of the retained attempt
/// can still act inside the Environment:
///
/// * the Worker durably recorded that the attempt's local entry closed
///   (`close_attempt`): every participant it ran in-process has returned and
///   none can start; and
/// * the attempt's committed footprint proves it entered only model
///   invocations (a coverage marker before every entry, no ToolBatch ever), so
///   no tool, process or Sandbox effect exists, and the only remaining
///   uncertainty is a provider's, outside the Environment.
///
/// This is ADR-0065's rule that a Blocked physical admission must not hold a
/// resource it cannot use while it waits for evidence, applied to Environment
/// admission instead of Pool concurrency. It settles nothing: no outcome,
/// observer, Memory extraction or successor execution is decided, and a later
/// claim of the same Run revives the Work through the ordinary claimed-Run
/// acquisition and is blocked again by the same slot.
///
/// | Rule | root Run | slot     | local entry | footprint             | Work     |
/// |------|----------|----------|-------------|-----------------------|----------|
/// | P1   | yes      | occupied | closed      | model invocations     | released |
/// | P2   | yes      | occupied | open        | any                   | kept     |
/// | P3   | yes      | occupied | closed      | tools or unqualified  | kept     |
/// | P4   | yes      | empty    | -           | -                     | kept     |
/// | P5   | no       | any      | any         | any                   | kept     |
///
/// P5: a child Run borrows its parent's Work and never releases it.
///
/// The root Session whose Work this deferral parks. Root affinity and slot
/// occupancy are read under the exact claim's commit guard; local entry
/// closure from the persisted attempt; containment from the committed
/// footprint. A stale claim, a changed slot or any unreadable fact parks
/// nothing: the Work is kept and the deferral itself still proceeds.
async fn park_root_session_work(
    service: &WorkerDispatchService,
    claim: &awaken_dispatch::RunClaim,
) -> Result<(), ApplicationError> {
    // Read committed containment without holding a SQL connection. The selected
    // atomic port rechecks the exact claim and physical record after this read.
    let attempt = match service.dispatch.load_physical_attempt(&claim.run_id).await {
        Ok(Some(attempt)) => attempt,
        Ok(None) => return Ok(()),
        Err(error) => {
            tracing::warn!(run_id=%claim.run_id.0, %error, "deferral keeps Session Work: physical attempt is unreadable");
            return Ok(());
        }
    };
    let model_only = if attempt.closing {
        match awaken_dispatch::PhysicalAttemptRecoveryRead::new(service.recovery.as_ref())
            .snapshot_for(&attempt)
            .await
            .map(|snapshot| awaken_dispatch::NativeAttemptFootprint::decode(&snapshot))
        {
            Ok(Ok(footprint)) => footprint.entered_only_model_invocations(),
            _ => false,
        }
    } else {
        false
    };
    service
        .worker_work_release
        .release(claim, &attempt, model_only)
        .await
        .map(|_| ())
        .map_err(|error| ApplicationError::unavailable(error.to_string()))
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
            return Err(ApplicationError::invalid(
                "authenticated Worker does not own the Session reservation claim",
            ));
        }
        let applied = service
            .dispatch
            .resolve_claimed_session_run_reservation(&request.claim, request.resolution)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?
            .applied();
        Ok(json!({ "applied": applied }))
    }
    .await;
    respond(result)
}

fn exact_attempt_return_identity<'a>(
    worker: &VerifiedWorkerContext,
    request: &'a AttemptExecutionReq,
) -> Result<&'a WorkerIdentity, ApplicationError> {
    let identity = request.identity.as_ref().ok_or_else(|| {
        ApplicationError::invalid("registered Worker identity is required for quiescence")
    })?;
    // Unlike begin/commit, a quiescence ACK may arrive after the registry or
    // Run lease advanced. Authenticate the exact signed incarnation, then let
    // the Dispatch aggregate clear only its matching predecessor slot.
    verify_worker_identity(worker, identity).map_err(ApplicationError::invalid)?;
    if identity.lease_owner() != request.claim.owner {
        return Err(ApplicationError::invalid(
            "authenticated Worker does not own the quiesced physical attempt",
        ));
    }
    Ok(identity)
}

/// The original close guard admits observation semantics for one immutable
/// incarnation. It never requires current/Ready/TTL and grants no stop role.
async fn require_attempt_observation_protocol(
    service: &WorkerDispatchService,
    identity: &WorkerIdentity,
) -> Result<(), ApplicationError> {
    let record = service
        .directory
        .incarnation(identity)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
        .ok_or_else(|| ApplicationError::invalid("Worker incarnation is unknown"))?;
    if !record.snapshot.manifest.supports_current_runtime_protocol() {
        return Err(ApplicationError::invalid(
            "Worker cannot attest the current attempt-closure protocol",
        ));
    }
    Ok(())
}

pub(super) async fn close_attempt(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<AttemptExecutionReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        if request.return_cause.is_some() {
            return Err(ApplicationError::invalid(
                "only physical finish accepts a return cause",
            ));
        }
        let identity = exact_attempt_return_identity(&worker, &request)?;
        require_attempt_observation_protocol(&service, identity).await?;
        let closed = service
            .dispatch
            .close_attempt(&request.claim)
            .await
            .map_err(|error| ApplicationError::unavailable(error.to_string()))?
            .applied();
        Ok(json!({"closed": closed}))
    }
    .await;
    respond(result)
}

pub(super) async fn record_credential_realization(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<CredentialRealizationReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let authority =
            claim_authority(&service, &worker, request.identity.as_ref(), false).await?;
        if authority.owner != request.claim.owner {
            return Err(ApplicationError::invalid(
                "authenticated worker does not own the credential realization claim",
            ));
        }
        let outcome = service
            .dispatch
            .record_credential_realization(&request.claim, request.receipt)
            .await
            .map_err(|error| ApplicationError::invalid(error.to_string()))?;
        Ok(json!({ "applied": outcome.applied() }))
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
                    manifest: awaken_worker_contract::WorkerManifest {
                        runtime_protocol: awaken_worker_contract::VersionRange::exact(
                            awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION,
                        ),
                        ..Default::default()
                    },
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
