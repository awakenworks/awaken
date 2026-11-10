// @formal entrypoint run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
// @formal entrypoint run_ingress.failed_attempt_release_spends_one_attempt_and_exhausts_through_retry_exhaustion
//! SQLite dead-letter of one expired lease, shared by operator quarantine and
//! the refusal of a lease generation that has no successor.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

/// One expired `running` row as the dead-letter decision reads it:
/// run id, lease owner, lease epoch, attempt count, cancellation, and the
/// physical slot's owner and epoch.
pub(super) type ExpiredLeaseRow = (
    String,
    Option<String>,
    i64,
    i64,
    i64,
    Option<String>,
    Option<i64>,
);

/// The columns of [`ExpiredLeaseRow`], in order.
pub(super) const EXPIRED_LEASE_COLUMNS: &str = "run_id, lease_owner, lease_epoch, attempt_count, \
     cancel_requested, active_attempt_owner, active_attempt_epoch";

/// Dead-letter one expired `running` row under its own expired lease through
/// the kernel's one retry-exhaustion transition, when the shared slot kernel
/// admits it. `reason` names why the lease was lost. Returns whether the row
/// moved; the caller's transaction commits it with its two operational facts.
pub(super) async fn dead_letter_expired_lease(
    tx: &mut SqliteConnection,
    p: &str,
    row: ExpiredLeaseRow,
    reason: LeaseLossReason,
    dead_lettered_at_ms: u64,
    now_ms: u64,
) -> Result<bool, DispatchError> {
    let (
        run_id,
        owner,
        epoch,
        attempt_count,
        cancellation_requested,
        active_attempt_owner,
        active_attempt_epoch,
    ) = row;
    let Some(owner) = owner else {
        return Err(DispatchError::Rejected(
            "expired running dispatch has no persisted lease owner".to_string(),
        ));
    };
    let claim_epoch = crate::stored_claim_epoch(epoch)?;
    // The SQL selection only prefilters. The slot disposition is decided
    // here, on the locked row, by the one kernel every backend shares.
    let slot = awaken_dispatch_contract::PhysicalAttemptSlot::from_facts(
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
    let changed = sqlx::query(
        "UPDATE runtime_dispatch SET status = ?4, \
         lease_owner = NULL, lease_until = NULL, dead_lettered_at = ?1 \
         WHERE run_id = ?2 AND status = 'running' AND lease_epoch = ?3",
    )
    .bind(crate::clock::db_millis(dead_lettered_at_ms))
    .bind(&run_id)
    .bind(epoch)
    .bind(crate::dispatch_state_db(next.state))
    .execute(&mut *tx)
    .await
    .map_err(store_error)?
    .rows_affected();
    if changed == 0 {
        return Ok(false);
    }
    let claim = RunClaim {
        run_id: RunId(run_id),
        owner,
        epoch: claim_epoch,
    };
    insert_operation(
        tx,
        p,
        &DispatchOperation::LeaseLost {
            claim: claim.clone(),
            reason,
        },
        now_ms,
    )
    .await?;
    insert_operation(
        tx,
        p,
        &DispatchOperation::DeadLettered {
            claim,
            attempt_count: durable_u64("dispatch attempt count", attempt_count)?,
        },
        now_ms,
    )
    .await?;
    Ok(true)
}

/// Settle every expired lease whose generation has no successor before a claim
/// scan selects, inside the scan's own transaction: each is dead-lettered once,
/// permanently, with `LeaseLost` carrying the integrity code, so no scan ever
/// selects it again and the candidates behind it stay claimable in this scan.
/// The signed column's last value is the one exhausted generation
/// ([`crate::sql_claim_generation_violation`]).
pub(super) async fn refuse_untrustworthy_expired_leases(
    tx: &mut SqliteConnection,
    p: &str,
    now_ms: u64,
) -> Result<usize, DispatchError> {
    // Only fixed expired-lease columns and the built-in namespace are composed.
    let rows = sqlx::query_as::<_, ExpiredLeaseRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {EXPIRED_LEASE_COLUMNS} FROM {p}_dispatch WHERE status = 'running' \
         AND lease_until IS NOT NULL AND lease_until <= ?1 AND lease_epoch = ?2 \
         AND (active_attempt_epoch IS NULL OR cancel_requested <> 0) \
         ORDER BY created_at"
    )))
    .bind(crate::clock::db_millis(now_ms))
    .bind(i64::MAX)
    .fetch_all(&mut *tx)
    .await
    .map_err(store_error)?;
    let mut refused = 0;
    for row in rows {
        debug_assert!(
            crate::sql_claim_generation_violation(row.2)
                == Some(crate::ClaimGenerationViolation::Exhausted)
        );
        let run_id = row.0.clone();
        if dead_letter_expired_lease(
            tx,
            p,
            row,
            LeaseLossReason::LeaseGenerationIntegrityViolated,
            now_ms,
            now_ms,
        )
        .await?
        {
            crate::observe_untrustworthy_claim_generation(
                &run_id,
                crate::UntrustworthyClaimGeneration::DeadLettered,
            );
            refused += 1;
        }
    }
    Ok(refused)
}
