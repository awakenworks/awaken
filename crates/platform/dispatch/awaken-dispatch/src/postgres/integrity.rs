// @formal entrypoint run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
// @formal entrypoint run_ingress.failed_attempt_release_spends_one_attempt_and_exhausts_through_retry_exhaustion
// PostgreSQL dead-letter of one expired lease, shared by operator quarantine
// and the refusal of a lease generation that has no successor. Included by
// `postgres.rs`; these remain private helpers of the parent adapter.

/// Dead-letter one locked expired `running` row under its own expired lease
/// through the kernel's one retry-exhaustion transition, when the shared slot
/// kernel admits it. `row` carries `run_id, lease_owner, lease_epoch,
/// attempt_count, cancel_requested, active_attempt_owner,
/// active_attempt_epoch`, read `FOR UPDATE` in `tx`. `reason` names why the
/// lease was lost. Returns whether the row moved.
async fn dead_letter_expired_lease(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    row: &sqlx::postgres::PgRow,
    reason: LeaseLossReason,
    dead_lettered_at_ms: u64,
) -> Result<bool, DispatchError> {
    let p = NS;
    let owner = row
        .try_get::<Option<String>, _>("lease_owner")
        .map_err(store_error)?
        .ok_or_else(|| {
            DispatchError::Rejected(
                "expired running dispatch has no persisted lease owner".to_string(),
            )
        })?;
    let epoch = row.try_get::<i64, _>("lease_epoch").map_err(store_error)?;
    let attempt_count = row
        .try_get::<i64, _>("attempt_count")
        .map_err(store_error)?;
    let cancellation_requested: i64 = row.try_get("cancel_requested").map_err(store_error)?;
    let claim_epoch = durable_u64("dispatch lease epoch", epoch)?;
    // The SQL selection only prefilters. The slot disposition is decided here,
    // on the locked row, by the one kernel every backend shares.
    let active_attempt_owner: Option<String> =
        row.try_get("active_attempt_owner").map_err(store_error)?;
    let active_attempt_epoch: Option<i64> =
        row.try_get("active_attempt_epoch").map_err(store_error)?;
    let slot = PhysicalAttemptSlot::from_facts(
        active_attempt_owner.is_some(),
        active_attempt_epoch.is_some(),
        active_attempt_owner.as_deref() == Some(owner.as_str()),
        active_attempt_epoch == Some(epoch),
    );
    if !slot.permits_settlement() && !slot.quarantine_admits_occupation(cancellation_requested != 0)
    {
        return Ok(false);
    }
    let current =
        crate::persisted_dispatch_transition("running", epoch, cancellation_requested != 0)?;
    let awaken_dispatch_contract::GuardedTransition::Applied(next) =
        current.exhaust_retries(claim_epoch, true)
    else {
        return Ok(false);
    };
    let run_id: String = row.try_get("run_id").map_err(store_error)?;
    let changed = sqlx::query(&format!(
        "UPDATE {p}_dispatch SET status = $4, lease_owner = NULL, \
         lease_until = NULL, dead_lettered_at = $1 WHERE run_id = $2 \
         AND status = 'running' AND lease_epoch = $3"
    ))
    .bind(crate::clock::db_millis(dead_lettered_at_ms))
    .bind(&run_id)
    .bind(epoch)
    .bind(crate::dispatch_state_db(next.state))
    .execute(&mut **tx)
    .await
    .map_err(store_error)?;
    if changed.rows_affected() != 1 {
        return Ok(false);
    }
    let claim = RunClaim {
        run_id: RunId(run_id),
        owner,
        epoch: claim_epoch,
    };
    insert_operation(
        tx,
        &DispatchOperation::LeaseLost {
            claim: claim.clone(),
            reason,
        },
    )
    .await?;
    insert_operation(
        tx,
        &DispatchOperation::DeadLettered {
            claim,
            attempt_count: durable_u64("dispatch attempt count", attempt_count)?,
        },
    )
    .await?;
    Ok(true)
}

/// Settle every expired lease whose generation has no successor before a
/// claim scan selects, in its own committed transaction: each is
/// dead-lettered once, permanently, with `LeaseLost` carrying the integrity
/// code, so no scan ever selects it again and the candidates behind it stay
/// claimable in this scan. Rows another transaction holds are left to its next
/// scan. The signed column's last value is the one exhausted generation
/// ([`crate::sql_claim_generation_exhausted`]).
async fn refuse_untrustworthy_expired_leases(pool: &PgPool) -> Result<usize, DispatchError> {
    let p = NS;
    let mut tx = pool.begin().await.map_err(store_error)?;
    let now_ms = crate::postgres_helpers::postgres_now_ms(&mut *tx).await?;
    let rows = sqlx::query(&format!(
        "SELECT run_id, lease_owner, lease_epoch, attempt_count, cancel_requested, \
             active_attempt_owner, active_attempt_epoch \
             FROM {p}_dispatch WHERE status = 'running' \
             AND lease_until IS NOT NULL AND lease_until <= $1 AND lease_epoch = $2 \
             AND (active_attempt_epoch IS NULL OR cancel_requested <> 0) \
             ORDER BY created_at FOR UPDATE SKIP LOCKED"
    ))
    .bind(crate::clock::db_millis(now_ms))
    .bind(i64::MAX)
    .fetch_all(&mut *tx)
    .await
    .map_err(store_error)?;
    let mut refused = Vec::new();
    for row in &rows {
        if dead_letter_expired_lease(
            &mut tx,
            row,
            LeaseLossReason::LeaseGenerationIntegrityViolated,
            now_ms,
        )
        .await?
        {
            refused.push(row.try_get::<String, _>("run_id").map_err(store_error)?);
        }
    }
    tx.commit().await.map_err(store_error)?;
    // Observed only once the dead letter is durable.
    for run_id in &refused {
        crate::observe_untrustworthy_claim_generation(
            run_id,
            crate::UntrustworthyClaimGeneration::DeadLettered,
        );
    }
    Ok(refused.len())
}
