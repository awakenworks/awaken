// @formal entrypoint run_ingress.failed_attempt_release_spends_one_attempt_and_exhausts_through_retry_exhaustion
// @formal entrypoint run_ingress.relinquish_requires_exact_current_claim_and_preserves_retry_budget
// PostgreSQL release transaction for the neutral DispatchQueue contract, shared
// by relinquish, deferral and failed-attempt release. Kept apart from the large
// trait implementation because it owns one physical attempt settlement
// boundary.
async fn release_claim_transaction(
    pool: &PgPool,
    claim: &RunClaim,
    retry_after_ms: Option<u64>,
    attempt: awaken_dispatch_contract::ReleasedAttempt,
) -> Result<SettleOutcome, DispatchError> {
    let p = NS;
    let epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
    let mut tx = pool.begin().await.map_err(store_error)?;
    let retry_not_before = if let Some(retry_after_ms) = retry_after_ms {
        let now_ms = crate::postgres_helpers::postgres_now_ms(&mut *tx).await?;
        Some(durable_i64(
            "dispatch resolution retry deadline",
            crate::clock::deadline_millis(now_ms, retry_after_ms),
        )?)
    } else {
        None
    };
    let current = sqlx::query(&format!(
        "SELECT status, lease_owner, lease_epoch, cancel_requested, attempt_count, \
         active_attempt_owner, active_attempt_epoch \
         FROM {p}_dispatch WHERE run_id = $1 FOR UPDATE"
    ))
    .bind(&claim.run_id.0)
    .fetch_optional(&mut *tx)
    .await
    .map_err(store_error)?;
    let Some(current) = current else {
        let _ = tx.rollback().await;
        return Ok(SettleOutcome::Fenced);
    };
    let status: String = current.try_get("status").map_err(store_error)?;
    let persisted_epoch: i64 = current.try_get("lease_epoch").map_err(store_error)?;
    let persisted_owner: Option<String> = current.try_get("lease_owner").map_err(store_error)?;
    let cancellation_requested: i64 = current.try_get("cancel_requested").map_err(store_error)?;
    let attempt_count = durable_u64(
        "dispatch attempt count",
        current.try_get("attempt_count").map_err(store_error)?,
    )?;
    let active_attempt_owner: Option<String> = current
        .try_get("active_attempt_owner")
        .map_err(store_error)?;
    let active_attempt_epoch = current
        .try_get::<Option<i64>, _>("active_attempt_epoch")
        .map_err(store_error)?
        .map(|value| durable_u64("dispatch attempt epoch", value))
        .transpose()?;
    let slot = PhysicalAttemptSlot::from_facts(
        active_attempt_owner.is_some(),
        active_attempt_epoch.is_some(),
        active_attempt_owner.as_deref() == Some(&claim.owner),
        active_attempt_epoch == Some(claim.epoch),
    );
    let transition = crate::persisted_dispatch_transition(
        &status,
        persisted_epoch,
        cancellation_requested != 0,
    )?;
    let (next, lease_until, attempt_count) = match transition.relinquish(
        claim.epoch,
        persisted_owner.as_deref() == Some(&claim.owner),
        slot,
        active_attempt_epoch,
        attempt_count,
        attempt,
    ) {
        awaken_dispatch_contract::RelinquishTransition::Pending {
            state,
            attempt_count,
        } => (state, None, attempt_count),
        awaken_dispatch_contract::RelinquishTransition::Recovering {
            state,
            attempt_count,
        } => (state, Some(0_i64), attempt_count),
        awaken_dispatch_contract::RelinquishTransition::Fenced => {
            let _ = tx.rollback().await;
            return Ok(SettleOutcome::Fenced);
        }
        awaken_dispatch_contract::RelinquishTransition::Corrupt => {
            let _ = tx.rollback().await;
            return Err(DispatchError::Rejected(
                "persisted physical attempt slot is not an owner/epoch pair".to_string(),
            ));
        }
    };
    let attempt_count = durable_i64("dispatch attempt count", attempt_count)?;
    let changed = sqlx::query(&format!(
        "UPDATE {p}_dispatch SET status = $4, \
         lease_owner = CASE WHEN $5 IS NULL THEN NULL ELSE lease_owner END, \
         lease_until = $5, attempt_count = $6, retry_not_before = $7, \
         created_at = clock_timestamp() WHERE run_id = $1 AND status = 'running' \
         AND lease_owner = $2 AND lease_epoch = $3"
    ))
    .bind(&claim.run_id.0)
    .bind(&claim.owner)
    .bind(epoch)
    .bind(crate::dispatch_state_db(next.state))
    .bind(lease_until)
    .bind(attempt_count)
    .bind(retry_not_before)
    .execute(&mut *tx)
    .await
    .map_err(store_error)?;
    if changed.rows_affected() != 1 {
        let _ = tx.rollback().await;
        return Ok(SettleOutcome::Fenced);
    }
    insert_operation(
        &mut tx,
        &DispatchOperation::LeaseLost {
            claim: claim.clone(),
            reason: LeaseLossReason::Relinquished,
        },
    )
    .await?;
    tx.commit().await.map_err(store_error)?;
    Ok(SettleOutcome::Applied)
}
