// SQLite physical-slot admission uses the canonical Dispatch transition kernel.
// Included by sqlite.rs; no separate state or transition authority.
struct PersistedAttemptFacts {
    status: String,
    lease_owner: Option<String>,
    lease_epoch: i64,
    lease_until: Option<i64>,
    active_owner: Option<String>,
    active_epoch: Option<i64>,
}

impl SqliteDispatchStore {
    async fn admit_physical_attempt(
        &self,
        claim: &RunClaim,
        now_ms: u64,
        handoff: bool,
    ) -> Result<AttemptAdmission, DispatchError> {
        let claim = claim.clone();
        self.with_conn(move |conn, p| {
            let epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            let current: Option<PersistedAttemptFacts> = tx
                .query_row(
                    &format!(
                        "SELECT status, lease_owner, lease_epoch, lease_until, \
                         active_attempt_owner, active_attempt_epoch \
                         FROM {p}_dispatch WHERE run_id = ?1"
                    ),
                    params![claim.run_id.0],
                    |row| {
                        Ok(PersistedAttemptFacts {
                            status: row.get(0)?,
                            lease_owner: row.get(1)?,
                            lease_epoch: row.get(2)?,
                            lease_until: row.get(3)?,
                            active_owner: row.get(4)?,
                            active_epoch: row.get(5)?,
                        })
                    },
                )
                .optional()
                .map_err(store_error)?;
            let Some(current) = current else {
                let _ = tx.rollback();
                return Ok(AttemptAdmission::Fenced);
            };
            let persisted_epoch = durable_u64("dispatch lease epoch", current.lease_epoch)?;
            let active_epoch = current
                .active_epoch
                .map(|value| durable_u64("dispatch attempt epoch", value))
                .transpose()?;
            let live = current.status == "running"
                && current.lease_owner.as_deref() == Some(&claim.owner)
                && persisted_epoch == claim.epoch
                && current
                    .lease_until
                    .is_some_and(|until| until >= crate::clock::db_millis(now_ms));
            let slot = PhysicalAttemptSlot::from_facts(
                current.active_owner.is_some(),
                active_epoch.is_some(),
                current.active_owner.as_deref() == Some(&claim.owner),
                active_epoch == Some(claim.epoch),
            );
            let transition = if handoff {
                handoff_physical_attempt(live, slot)
            } else {
                begin_physical_attempt(live, slot)
            };
            let admission = match transition {
                PhysicalAttemptTransition::Stutter => AttemptAdmission::AlreadyApplied,
                PhysicalAttemptTransition::Blocked => AttemptAdmission::Blocked,
                PhysicalAttemptTransition::Fenced => AttemptAdmission::Fenced,
                PhysicalAttemptTransition::Install | PhysicalAttemptTransition::Replace => {
                    tx.execute(
                        &format!(
                            "UPDATE {p}_dispatch SET active_attempt_owner = ?2, \
                             active_attempt_epoch = ?3 WHERE run_id = ?1"
                        ),
                        params![claim.run_id.0, claim.owner, epoch],
                    )
                    .map_err(store_error)?;
                    AttemptAdmission::Applied
                }
                PhysicalAttemptTransition::Corrupt => {
                    return Err(DispatchError::Rejected(
                        "persisted physical attempt slot is not an owner/epoch pair".to_string(),
                    ));
                }
                PhysicalAttemptTransition::Clear => {
                    unreachable!("attempt admission never clears a slot")
                }
            };
            tx.commit().map_err(store_error)?;
            Ok(admission)
        })
        .await
    }
}
