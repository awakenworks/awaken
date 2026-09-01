// PostgreSQL exact-claim transaction kernels. These remain private helpers of
// the parent adapter and do not introduce a second claim policy.
async fn claim_exact_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    requested_run: &RunId,
    owner: &str,
    lease_ms: u64,
    _now_ms: u64,
    worker: Option<&WorkerSnapshot>,
    capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
) -> Result<Option<Claimed>, DispatchError> {
    let now_ms = crate::postgres_helpers::postgres_now_ms(&mut **tx).await?;
    let phase = sqlx::query(&format!(
        "SELECT status, lease_until FROM {NS}_dispatch WHERE run_id = $1"
    ))
    .bind(&requested_run.0)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_error)?;
    let mode = if let Some(row) = phase {
        let status = row.try_get::<String, _>("status").map_err(store_error)?;
        let state = DispatchState::from_db(&status).ok_or_else(|| {
            DispatchError::Rejected(format!("unknown persisted dispatch state `{status}`"))
        })?;
        let deadline = row
            .try_get::<Option<i64>, _>("lease_until")
            .map_err(store_error)?
            .map(crate::clock::millis_from_db)
            .transpose()
            .map_err(|error| DispatchError::Rejected(error.to_string()))?;
        classify_exact_claim_mode(state, deadline, now_ms)
    } else {
        ExactClaimMode::Runnable
    };
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
async fn claim_exact_transaction_with_mode(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    requested_run: &RunId,
    owner: &str,
    lease_ms: u64,
    now_ms: u64,
    worker: Option<&WorkerSnapshot>,
    capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    mode: ExactClaimMode,
) -> Result<Option<Claimed>, DispatchError> {
    let p = NS;
    let thread_id = sqlx::query_scalar::<_, String>(&format!(
        "SELECT thread_id FROM {p}_dispatch WHERE run_id = $1"
    ))
    .bind(&requested_run.0)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_error)?;
    let Some(thread_id) = thread_id else {
        return Ok(None);
    };
    // The partial unique index remains the hard backstop, while this transaction
    // lock orders all claim transitions for one Thread before the eligibility
    // recheck. Hash collisions only over-serialize unrelated Threads.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(&thread_id)
        .execute(&mut **tx)
        .await
        .map_err(store_error)?;
    let thread_available = thread_available_for_claim(p);
    let no_running_peer = no_running_peer(p);
    let eligibility = match mode {
        ExactClaimMode::Runnable => format!(
            "((d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < $2) \
             OR (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS ( \
               SELECT 1 FROM {p}_pending pe WHERE pe.run_id = d.run_id \
               AND (pe.available_at IS NULL OR pe.available_at <= $2))) AND {thread_available}) \
             OR (d.status = 'pending' AND {thread_available})) AND $3::bigint IS NULL"
        ),
        ExactClaimMode::ReservationRecovery => format!(
            "((d.status = 'reserved' AND d.lease_until IS NOT NULL \
             AND d.lease_until < $2) \
             OR (d.status = 'reservation_running' AND d.lease_until IS NOT NULL \
             AND d.lease_until < $2)) AND {no_running_peer} AND $3::bigint IS NULL"
        ),
        ExactClaimMode::TerminalRecovery => {
            format!(
                "((d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < $2) \
                 OR (d.status = 'awaiting' AND d.lease_owner IS NULL AND d.lease_until IS NULL \
                 AND {no_running_peer})) AND $3::bigint IS NULL"
            )
        }
        ExactClaimMode::RetryExhausted { .. } => {
            "d.status = 'running' AND d.lease_until IS NOT NULL \
             AND d.lease_until < $2 AND d.attempt_count >= $3"
                .to_string()
        }
    };
    let sql = format!(
        "SELECT d.request, d.sandbox, d.status, d.worker_assignment, d.cancel_requested, \
                d.lease_owner, d.lease_epoch, d.lease_until, d.attempt_count \
         FROM {p}_dispatch d \
         WHERE d.run_id = $1 AND ({eligibility}) FOR UPDATE SKIP LOCKED LIMIT 1"
    );
    let retry_limit = mode
        .retry_limit()
        .map(|limit| durable_i64("dispatch retry limit", limit))
        .transpose()?;
    let Some(row) = sqlx::query(&sql)
        .bind(&requested_run.0)
        .bind(crate::clock::db_millis(now_ms))
        .bind(retry_limit)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_error)?
    else {
        return Ok(None);
    };
    let Json(request): Json<RunDispatch> = row.try_get("request").map_err(store_error)?;
    let previous: Option<Json<WorkerAssignment>> =
        row.try_get("worker_assignment").map_err(store_error)?;
    let sandbox: Option<String> = row.try_get("sandbox").map_err(store_error)?;
    let cancellation_requested: i64 = row.try_get("cancel_requested").map_err(store_error)?;
    let previous_owner: Option<String> = row.try_get("lease_owner").map_err(store_error)?;
    let previous_epoch: i64 = row.try_get("lease_epoch").map_err(store_error)?;
    if let ExactClaimMode::RetryExhausted { max_attempts } = mode {
        let status = row.try_get::<String, _>("status").map_err(store_error)?;
        let lease_until = row
            .try_get::<Option<i64>, _>("lease_until")
            .map_err(store_error)?;
        let attempt_count = row
            .try_get::<i64, _>("attempt_count")
            .map_err(store_error)?;
        if !retry_exhaustion_evidence_is_eligible(
            &status,
            lease_until,
            attempt_count,
            max_attempts,
            now_ms,
        )? {
            return Ok(None);
        }
    }
    let terminal_resolution = mode.bypasses_execution_admission();
    if !terminal_resolution
        && cancellation_requested == 0
        && match worker {
            Some(worker) => can_assign(
                worker,
                &request.placement,
                previous.as_ref().map(|value| &value.0),
                sandbox.is_some(),
                now_ms,
            )
            .is_err(),
            None => !can_claim_locally(&request.placement),
        }
    {
        return Ok(None);
    }
    let status: String = row.try_get("status").map_err(store_error)?;
    let current_transition =
        crate::persisted_dispatch_transition(&status, previous_epoch, cancellation_requested != 0)?;
    let claimed_transition = if mode == ExactClaimMode::ReservationRecovery {
        current_transition.recover_reservation()
    } else {
        current_transition.claim()
    }
    .map_err(crate::transition_error)?
    .ok_or_else(|| {
        DispatchError::Rejected(format!(
            "persisted dispatch state `{status}` is not claimable in {mode:?} mode"
        ))
    })?;
    let claim_epoch = claimed_transition.lease_epoch;
    let credential_bindings = if terminal_resolution || cancellation_requested != 0 {
        Vec::new()
    } else {
        compile_attempt_credential_bindings(&request, capabilities, claim_epoch, now_ms).map_err(
            |error| {
                DispatchError::Rejected(format!("credential attempt admission failed: {error}"))
            },
        )?
    };
    let expires = crate::clock::deadline_millis(now_ms, lease_ms);
    let claimed_status = crate::dispatch_state_db(claimed_transition.state);
    sqlx::query(&format!(
        "UPDATE {p}_dispatch SET status = $9, lease_owner = $1, lease_until = $2, \
         attempt_count = attempt_count + $3, lease_epoch = $5, worker_assignment = $6, \
         credential_bindings = $7, credential_receipts = $8 WHERE run_id = $4"
    ))
    .bind(owner)
    .bind(crate::clock::db_millis(expires))
    .bind(i64::from(status == "running"))
    .bind(&requested_run.0)
    .bind(i64::try_from(claim_epoch).map_err(|_| {
        DispatchError::Rejected(
            "dispatch claim epoch exceeds the Postgres authority range".to_string(),
        )
    })?)
    .bind(
        (!terminal_resolution)
            .then(|| worker.map(WorkerAssignment::from))
            .flatten()
            .map(Json),
    )
    .bind(Json(&credential_bindings))
    .bind(Json(Vec::<CredentialRealizationReceipt>::new()))
    .bind(claimed_status)
    .execute(&mut **tx)
    .await
    .map_err(store_error)?;
    let claim = RunClaim {
        run_id: requested_run.clone(),
        owner: owner.to_string(),
        epoch: claim_epoch,
    };
    if matches!(status.as_str(), "running" | "reservation_running") {
        let previous = RunClaim {
            run_id: requested_run.clone(),
            owner: previous_owner.ok_or_else(|| {
                DispatchError::Rejected(
                    "expired running dispatch has no persisted lease owner".to_string(),
                )
            })?,
            epoch: u64::try_from(previous_epoch).map_err(|_| {
                DispatchError::Rejected("persisted dispatch claim epoch is negative".to_string())
            })?,
        };
        let reason = if matches!(mode, ExactClaimMode::RetryExhausted { .. }) {
            LeaseLossReason::RetryExhausted
        } else {
            LeaseLossReason::Expired
        };
        insert_operation(
            tx,
            &DispatchOperation::LeaseLost {
                claim: previous.clone(),
                reason,
            },
        )
        .await?;
        insert_operation(
            tx,
            &DispatchOperation::Reclaimed {
                previous,
                claim: claim.clone(),
            },
        )
        .await?;
    } else {
        insert_operation(
            tx,
            &DispatchOperation::Claimed {
                claim: claim.clone(),
            },
        )
        .await?;
    }
    let rows = sqlx::query(&format!(
        "SELECT message_id, thread_id, correlation_id, result, context_messages, available_at \
         FROM {p}_pending WHERE run_id = $1 \
         AND (available_at IS NULL OR available_at <= $2) ORDER BY created_at"
    ))
    .bind(&requested_run.0)
    .bind(crate::clock::db_millis(now_ms))
    .fetch_all(&mut **tx)
    .await
    .map_err(store_error)?;
    let mut pending = Vec::with_capacity(rows.len());
    for row in rows {
        let Json(result): Json<ResumeResult> = row.try_get("result").map_err(store_error)?;
        let context_messages = row
            .try_get::<Option<Json<Vec<Message>>>, _>("context_messages")
            .map_err(store_error)?
            .map(|Json(messages)| messages)
            .unwrap_or_default();
        pending.push(PendingInput {
            message_id: row.try_get("message_id").map_err(store_error)?,
            run_id: requested_run.clone(),
            thread_id: ThreadId(row.try_get("thread_id").map_err(store_error)?),
            correlation_id: row.try_get("correlation_id").map_err(store_error)?,
            available_at_ms: row
                .try_get::<Option<i64>, _>("available_at")
                .map_err(store_error)?
                .map(crate::clock::millis_from_db)
                .transpose()
                .map_err(|err| DispatchError::Rejected(err.to_string()))?,
            result,
            context_messages,
        });
    }
    Ok(Some(Claimed {
        request,
        lease: Lease {
            run_id: claim.run_id,
            owner: claim.owner,
            expires_ms: expires,
            epoch: claim.epoch,
        },
        credential_bindings,
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
