// SQLite release transaction for the neutral DispatchQueue contract. Kept
// separate from the large trait implementation because it owns one physical
// attempt settlement boundary and is reused by multiple release entry points.
type ReleaseClaimRow = (
    String,
    Option<String>,
    i64,
    i64,
    i64,
    Option<String>,
    Option<i64>,
);

fn release_claim_transaction(
    conn: &mut rusqlite::Connection,
    p: &str,
    claim: RunClaim,
    retry_not_before: Option<i64>,
) -> Result<SettleOutcome, DispatchError> {
    let epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(store_error)?;
    let current: Option<ReleaseClaimRow> = tx
        .query_row(
            &format!(
                "SELECT status, lease_owner, lease_epoch, cancel_requested, attempt_count, \
                 active_attempt_owner, active_attempt_epoch FROM {p}_dispatch WHERE run_id = ?1"
            ),
            params![claim.run_id.0],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()
        .map_err(store_error)?;
    let Some((
        status,
        owner,
        persisted_epoch,
        cancellation_requested,
        attempt_count,
        active_attempt_owner,
        active_attempt_epoch,
    )) = current
    else {
        let _ = tx.rollback();
        return Ok(SettleOutcome::Fenced);
    };
    let attempt_count = durable_u64("dispatch attempt count", attempt_count)?;
    let active_attempt_epoch = active_attempt_epoch
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
        owner.as_deref() == Some(&claim.owner),
        slot,
        attempt_count,
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
            let _ = tx.rollback();
            return Ok(SettleOutcome::Fenced);
        }
        awaken_dispatch_contract::RelinquishTransition::Corrupt => {
            let _ = tx.rollback();
            return Err(DispatchError::Rejected(
                "persisted physical attempt slot is not an owner/epoch pair".to_string(),
            ));
        }
    };
    let attempt_count = durable_i64("dispatch attempt count", attempt_count)?;
    let changed = tx
        .execute(
            &format!(
                "UPDATE {p}_dispatch SET status = ?4, \
                 lease_owner = CASE WHEN ?5 IS NULL THEN NULL ELSE lease_owner END, \
                 lease_until = ?5, attempt_count = ?6, retry_not_before = ?7, \
                 created_at = strftime('%Y-%m-%d %H:%M:%f', 'now') \
                 WHERE run_id = ?1 AND status = 'running' \
                 AND lease_owner = ?2 AND lease_epoch = ?3"
            ),
            params![
                claim.run_id.0,
                claim.owner,
                epoch,
                crate::dispatch_state_db(next.state),
                lease_until,
                attempt_count,
                retry_not_before,
            ],
        )
        .map_err(store_error)?;
    if changed != 1 {
        let _ = tx.rollback();
        return Ok(SettleOutcome::Fenced);
    }
    insert_operation(
        &tx,
        p,
        &DispatchOperation::LeaseLost {
            claim,
            reason: LeaseLossReason::Relinquished,
        },
    )?;
    tx.commit().map_err(store_error)?;
    Ok(SettleOutcome::Applied)
}
