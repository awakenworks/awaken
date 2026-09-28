// @formal evidence session_execution.one_physical_attempt_per_thread
// SQLite physical-slot admission uses the canonical Dispatch transition kernel.
// Included by sqlite.rs; no separate state or transition authority.
struct PersistedAttemptFacts {
    status: String,
    lease_owner: Option<String>,
    lease_epoch: i64,
    lease_until: Option<i64>,
    active_owner: Option<String>,
    active_epoch: Option<i64>,
    retired_epoch: i64,
    binding: Option<String>,
    assignment: Option<String>,
}

impl SqliteDispatchStore {
    async fn admit_physical_attempt(
        &self,
        claim: &RunClaim,
        now_ms: u64,
        handoff: bool,
        binding: Option<&AttemptExecutionBinding>,
    ) -> Result<AttemptAdmission, DispatchError> {
        let claim = claim.clone();
        let binding = binding.cloned();
        self.with_conn(move |conn, p| {
            let epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            let current: Option<PersistedAttemptFacts> = tx
                .query_row(
                    &format!(
                        "SELECT status, lease_owner, lease_epoch, lease_until, \
                         active_attempt_owner, active_attempt_epoch, retired_attempt_epoch, active_attempt_binding, worker_assignment \
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
                            retired_epoch: row.get(6)?,
                            binding: row.get(7)?,
                            assignment: row.get(8)?,
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
            let retired_epoch = durable_u64("retired attempt epoch", current.retired_epoch)?;
            let live = awaken_dispatch_contract::attempt_epoch_is_open(claim.epoch, retired_epoch)
                && current.status == "running"
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
            let (transition, admission) =
                crate::dispatch::decide_physical_attempt_admission(live, slot, handoff,
                    tx.query_row(&format!(
                        "SELECT EXISTS (SELECT 1 FROM {p}_dispatch peer JOIN {p}_dispatch target
                         ON peer.thread_id = target.thread_id WHERE target.run_id = ?1
                         AND peer.run_id <> target.run_id
                         AND (peer.active_attempt_owner IS NOT NULL OR peer.active_attempt_epoch IS NOT NULL))"
                    ), params![claim.run_id.0], |row| row.get::<_, bool>(0)).map_err(store_error)?
                )?;
            let stored = crate::dispatch::decode_physical_attempt(
                &claim.run_id, current.active_owner, active_epoch, current.binding,
            )?;
            let assignment: Option<WorkerAssignment> = current.assignment
                .map(|json| serde_json::from_str(&json).map_err(DispatchError::unavailable)).transpose()?;
            crate::dispatch::verify_physical_attempt_binding(
                transition, &claim, assignment.as_ref(), binding.as_ref(),
                stored.as_ref().and_then(|a| a.binding.as_ref()),
            )?;
            if matches!(
                transition,
                PhysicalAttemptTransition::Install | PhysicalAttemptTransition::Replace
            ) {
                tx.execute(
                    &format!(
                        "UPDATE {p}_dispatch SET active_attempt_owner = ?2, \
                             active_attempt_epoch = ?3, active_attempt_binding = ?4 WHERE run_id = ?1"
                    ),
                    params![claim.run_id.0, claim.owner, epoch, binding.as_ref().map(serde_json::to_string).transpose().map_err(DispatchError::unavailable)?],
                )
                .map_err(store_error)?;
            }
            tx.commit().map_err(store_error)?;
            Ok(admission)
        })
        .await
    }
}
