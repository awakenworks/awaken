// @formal entrypoint delegation.independent_child_scheduling_lease
// @formal entrypoint ingress.epoch_reclaim
// @formal entrypoint ingress.single_owner
// @formal entrypoint remote_worker.single_live_claim_owner
// @formal entrypoint run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
// @formal entrypoint run_ingress.request_local_resolution_retry_honors_persisted_deadline
// @formal entrypoint worker.retry_preserves_pinned_binding
//! Exact SQLite dispatch claiming.
//!
//! This module owns the one `BEGIN IMMEDIATE` claim transition shared by
//! ordinary execution and committed-terminal repair. Selection, credential
//! admission, epoch advancement, operational events, and pending-input freeze
//! therefore cannot diverge between the two entry points.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

pub(super) async fn claim_retry_exhausted_transaction(
    conn: &mut SqliteConnection,
    prefix: &str,
    owner: &str,
    lease_ms: u64,
    now_ms: u64,
    max_attempts: u64,
) -> Result<Option<Claimed>, DispatchError> {
    let max_attempts_i64 = durable_i64("dispatch retry limit", max_attempts)?;
    let mut tx = immediate(conn).await?;
    // A retry-exhausted lease whose generation has no successor can never be
    // terminalized by a claim; it is dead-lettered here instead of aborting
    // this selection forever (it would otherwise head every later one).
    refuse_untrustworthy_expired_leases(&mut tx, prefix, now_ms).await?;
    let run_id = sqlx::query_scalar::<_, String>(
        "SELECT run_id FROM runtime_dispatch \
         WHERE status = 'running' AND lease_until IS NOT NULL \
         AND lease_until <= ?1 AND attempt_count >= ?2 \
         AND active_attempt_epoch IS NULL \
         ORDER BY created_at LIMIT 1",
    )
    .bind(crate::clock::db_millis(now_ms))
    .bind(max_attempts_i64)
    .fetch_optional(&mut *tx)
    .await
    .map_err(store_error)?;
    let Some(run_id) = run_id else {
        tx.commit().await.map_err(store_error)?;
        return Ok(None);
    };
    let claimed = claim_exact_transaction_with_mode(
        &mut tx,
        &run_id,
        owner,
        lease_ms,
        now_ms,
        None,
        &Default::default(),
        ExactClaimMode::RetryExhausted { max_attempts },
    )
    .await?;
    tx.commit().await.map_err(store_error)?;
    Ok(claimed)
}

pub(super) async fn claim_exact_transaction(
    tx: &mut SqliteConnection,
    requested_run: &str,
    owner: &str,
    lease_ms: u64,
    now_ms: u64,
    worker: Option<&WorkerSnapshot>,
    capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
) -> Result<Option<Claimed>, DispatchError> {
    let phase: Option<(String, Option<i64>)> =
        sqlx::query_as("SELECT status, lease_until FROM runtime_dispatch WHERE run_id = ?1")
            .bind(requested_run)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_error)?;
    let mode = phase
        .map(|(status, deadline)| {
            let state = DispatchState::from_db(&status).ok_or_else(|| {
                DispatchError::Rejected(format!("unknown persisted dispatch state `{status}`"))
            })?;
            let deadline = deadline
                .map(crate::clock::millis_from_db)
                .transpose()
                .map_err(|error| DispatchError::Rejected(error.to_string()))?;
            Ok(classify_exact_claim_mode(state, deadline, now_ms))
        })
        .transpose()?
        .unwrap_or(ExactClaimMode::Runnable);
    claim_exact_transaction_with_mode(
        tx,
        requested_run,
        owner,
        lease_ms,
        now_ms,
        worker,
        capabilities,
        mode,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn claim_exact_transaction_with_mode(
    tx: &mut SqliteConnection,
    requested_run: &str,
    owner: &str,
    lease_ms: u64,
    now_ms: u64,
    worker: Option<&WorkerSnapshot>,
    capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    mode: ExactClaimMode,
) -> Result<Option<Claimed>, DispatchError> {
    let prefix = NS;
    type PickedDispatch = (
        String,
        Option<String>,
        String,
        Option<String>,
        i64,
        Option<String>,
        i64,
        Option<i64>,
        i64,
    );
    let thread_available = thread_available_for_claim(prefix);
    let no_running_peer = no_running_peer(prefix);
    let eligibility = match mode {
        ExactClaimMode::Runnable => format!(
            "((d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until <= ?2) \
             OR (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS ( \
               SELECT 1 FROM {prefix}_pending pe WHERE pe.run_id = d.run_id \
               AND (pe.available_at IS NULL OR pe.available_at <= ?2))) AND {thread_available}) \
             OR (d.status = 'pending' AND {thread_available})) \
             AND (d.retry_not_before IS NULL \
                  OR d.retry_not_before <= ?2) AND ?3 IS NULL"
        ),
        ExactClaimMode::ReservationRecovery => format!(
            "((d.status = 'reserved' AND d.lease_until IS NOT NULL \
             AND d.lease_until <= ?2) \
             OR (d.status = 'reservation_running' AND d.lease_until IS NOT NULL \
             AND d.lease_until <= ?2)) AND {no_running_peer} \
             AND (d.retry_not_before IS NULL OR d.retry_not_before <= ?2) \
             AND ?3 IS NULL"
        ),
        ExactClaimMode::TerminalRecovery => {
            format!(
                "d.active_attempt_owner IS NULL AND d.active_attempt_epoch IS NULL AND \
                 ((d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until <= ?2) \
                 OR (d.status IN ('pending', 'awaiting') \
                 AND d.lease_owner IS NULL AND d.lease_until IS NULL \
                 AND {no_running_peer})) \
                 AND (d.retry_not_before IS NULL OR d.retry_not_before <= ?2) \
                 AND ?3 IS NULL"
            )
        }
        ExactClaimMode::RetryExhausted { .. } => {
            "d.status = 'running' AND d.lease_until IS NOT NULL \
             AND d.lease_until <= ?2 AND d.attempt_count >= ?3"
                .to_string()
        }
    };
    let sql = format!(
        "SELECT d.request, d.sandbox, d.status, d.worker_assignment, d.cancel_requested, \
                d.lease_owner, d.lease_epoch, d.lease_until, d.attempt_count \
         FROM {prefix}_dispatch d \
         WHERE d.run_id = ?1 AND ({eligibility}) LIMIT 1"
    );
    let retry_limit = mode
        .retry_limit()
        .map(|limit| durable_i64("dispatch retry limit", limit))
        .transpose()?;
    // eligibility is the closed ExactClaimMode match; prefix is the built-in
    // namespace. Every run/time/limit value remains a bind parameter.
    let picked: Option<PickedDispatch> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(requested_run)
        .bind(crate::clock::db_millis(now_ms))
        .bind(retry_limit)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
    let Some((
        request_json,
        sandbox,
        status,
        previous_json,
        cancellation_requested,
        previous_owner,
        previous_epoch,
        lease_until,
        attempt_count,
    )) = picked
    else {
        return Ok(None);
    };
    let request: RunDispatch = serde_json::from_str(&request_json).map_err(json_err)?;
    let previous: Option<WorkerAssignment> = previous_json
        .map(|value| serde_json::from_str(&value).map_err(json_err))
        .transpose()?;
    if let ExactClaimMode::RetryExhausted { max_attempts } = mode
        && !retry_exhaustion_evidence_is_eligible(
            &status,
            lease_until,
            attempt_count,
            max_attempts,
            now_ms,
        )?
    {
        return Ok(None);
    }
    let terminal_resolution = mode.bypasses_execution_admission();
    if request
        .worker_codec_admission(worker.map(|worker| &worker.manifest))
        .is_err()
    {
        return Ok(None);
    }
    if !terminal_resolution
        && cancellation_requested == 0
        && match worker {
            Some(worker) => can_assign(
                worker,
                &request.placement,
                previous.as_ref(),
                sandbox.is_some(),
                now_ms,
            )
            .is_err(),
            None => !can_claim_locally(&request.placement),
        }
    {
        return Ok(None);
    }
    let current_transition =
        crate::persisted_dispatch_transition(&status, previous_epoch, cancellation_requested != 0)?;
    let (claimed_transition, request) = claim_with_frozen(current_transition, request, mode)
        .map_err(crate::transition_error)?
        .ok_or_else(|| {
            DispatchError::Rejected(format!(
                "persisted dispatch state `{status}` is not claimable in {mode:?} mode"
            ))
        })?;
    let claim_epoch = claimed_transition.lease_epoch;
    let credential_operation =
        claim_credential_operation(terminal_resolution, cancellation_requested != 0);
    let credential_bindings = compile_claim_credential_bindings(
        &request,
        capabilities,
        claim_epoch,
        now_ms,
        credential_operation,
    )
    .map_err(|error| {
        DispatchError::Rejected(format!("credential claim admission failed: {error}"))
    })?;
    let expires = crate::clock::deadline_millis(now_ms, lease_ms);
    let claimed_status = crate::dispatch_state_db(claimed_transition.state);
    let claim_epoch_db = crate::minted_claim_epoch_column(claim_epoch)?;
    let assignment_json = (!terminal_resolution)
        .then(|| worker.map(WorkerAssignment::from))
        .flatten()
        .map(|value| json(&value))
        .transpose()?;
    sqlx::query(
        "UPDATE runtime_dispatch SET status = ?9, lease_owner = ?1, \
         lease_until = ?2, attempt_count = attempt_count + ?3, \
         lease_epoch = ?5, worker_assignment = ?6, credential_bindings = ?7, \
         credential_receipts = ?8, retry_not_before = NULL WHERE run_id = ?4",
    )
    .bind(owner)
    .bind(crate::clock::db_millis(expires))
    .bind(i64::from(status == "running"))
    .bind(requested_run)
    .bind(claim_epoch_db)
    .bind(assignment_json)
    .bind(json(&credential_bindings)?)
    .bind(json(&Vec::<CredentialRealizationReceipt>::new())?)
    .bind(claimed_status)
    .execute(&mut *tx)
    .await
    .map_err(store_error)?;
    let claim = RunClaim {
        run_id: RunId(requested_run.to_string()),
        owner: owner.to_string(),
        epoch: claim_epoch,
    };
    if matches!(status.as_str(), "running" | "reservation_running") {
        let previous = RunClaim {
            run_id: RunId(requested_run.to_string()),
            owner: previous_owner.ok_or_else(|| {
                DispatchError::Rejected(
                    "expired running dispatch has no persisted lease owner".to_string(),
                )
            })?,
            epoch: crate::stored_claim_epoch(previous_epoch)?,
        };
        let reason = if matches!(mode, ExactClaimMode::RetryExhausted { .. }) {
            LeaseLossReason::RetryExhausted
        } else {
            LeaseLossReason::Expired
        };
        insert_operation(
            tx,
            prefix,
            &DispatchOperation::LeaseLost {
                claim: previous.clone(),
                reason,
            },
            now_ms,
        )
        .await?;
        insert_operation(
            tx,
            prefix,
            &DispatchOperation::Reclaimed {
                previous,
                claim: claim.clone(),
            },
            now_ms,
        )
        .await?;
    } else {
        insert_operation(
            tx,
            prefix,
            &DispatchOperation::Claimed {
                claim: claim.clone(),
            },
            now_ms,
        )
        .await?;
    }
    let pending = pending_for_run(tx, prefix, requested_run, now_ms).await?;
    Ok(Some(Claimed {
        session_work_authority: None,
        request,
        lease: DispatchLease::from_persisted(claim.run_id, claim.owner, expires, claim.epoch)?,
        credential_bindings,
        credential_deliveries: Vec::new(),
        cancellation_requested: cancellation_requested != 0,
        pending,
        recovered: status == "running",
        session_activity_admission_required: mode == ExactClaimMode::ReservationRecovery,
        sandbox,
        assignment: (!terminal_resolution)
            .then(|| worker.map(WorkerAssignment::from))
            .flatten(),
    }))
}
