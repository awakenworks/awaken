// @formal evidence session_execution.one_physical_attempt_per_thread
// SQLite physical-slot admission uses the canonical Dispatch transition kernel.
// Included by sqlite.rs; no separate state or transition authority.
struct PersistedAttemptFacts {
    request: String,
    status: String,
    lease_owner: Option<String>,
    lease_epoch: i64,
    lease_until: Option<i64>,
    active_owner: Option<String>,
    active_epoch: Option<i64>,
    retired_epoch: i64,
    binding: Option<String>,
    assignment: Option<String>,
    closing: bool,
}

type PersistedAttemptRow = (
    String,
    Option<String>,
    i64,
    Option<i64>,
    Option<String>,
    Option<i64>,
    i64,
    Option<String>,
    Option<String>,
    bool,
    String,
);

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
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
        let mut tx = immediate(&mut conn).await?;
        let current: Option<PersistedAttemptFacts> = sqlx::query_as::<_, PersistedAttemptRow>(
            &format!(
                "SELECT status, lease_owner, lease_epoch, lease_until, \
                 active_attempt_owner, active_attempt_epoch, retired_attempt_epoch, active_attempt_binding, worker_assignment, active_attempt_closing, request \
                 FROM {p}_dispatch WHERE run_id = ?1"
            ),
        )
        .bind(&claim.run_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?
        .map(
            |(
                status,
                lease_owner,
                lease_epoch,
                lease_until,
                active_owner,
                active_epoch,
                retired_epoch,
                binding,
                assignment,
                closing,
                request,
            )| PersistedAttemptFacts {
                status,
                lease_owner,
                lease_epoch,
                lease_until,
                active_owner,
                active_epoch,
                retired_epoch,
                binding,
                assignment,
                closing,
                request,
            },
        );
        let Some(current) = current else {
            let _ = tx.rollback().await;
            return Ok(AttemptAdmission::Fenced);
        };
        let persisted_epoch = crate::stored_claim_epoch(current.lease_epoch)?;
        let active_epoch = current
            .active_epoch
            .map(crate::stored_claim_epoch)
            .transpose()?;
        let retired_epoch = crate::stored_claim_epoch(current.retired_epoch)?;
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
        let peer_occupied = sqlx::query_scalar::<_, bool>(&format!(
            "SELECT EXISTS (SELECT 1 FROM {p}_dispatch peer JOIN {p}_dispatch target
             ON peer.thread_id = target.thread_id WHERE target.run_id = ?1
             AND peer.run_id <> target.run_id
             AND (peer.active_attempt_owner IS NOT NULL OR peer.active_attempt_epoch IS NOT NULL))"
        ))
        .bind(&claim.run_id.0)
        .fetch_one(&mut *tx)
        .await
        .map_err(store_error)?;
        let (transition, admission) = crate::dispatch::decide_physical_attempt_admission(
            live,
            slot,
            handoff,
            peer_occupied,
            current.closing,
        )?;
        let stored = crate::dispatch::decode_physical_attempt(
            &claim.run_id,
            current.active_owner,
            active_epoch,
            current.binding,
            current.closing,
        )?;
        let assignment: Option<WorkerAssignment> = current
            .assignment
            .map(|json| serde_json::from_str(&json).map_err(DispatchError::unavailable))
            .transpose()?;
        crate::dispatch::verify_physical_attempt_binding(
            transition,
            &claim,
            assignment.as_ref(),
            binding.as_ref(),
            stored.as_ref().and_then(|a| a.binding.as_ref()),
        )?;
        if matches!(
            transition,
            PhysicalAttemptTransition::Install | PhysicalAttemptTransition::Replace
        ) {
            let request: RunDispatch = serde_json::from_str(&current.request).map_err(json_err)?;
            let operation =
                crate::dispatch::physical_admission_operation(&claim, binding.as_ref(), &request);
            insert_operation(&mut tx, p, operation, now_ms).await?;
            let binding_json = binding
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(DispatchError::unavailable)?;
            sqlx::query(&format!(
                "UPDATE {p}_dispatch SET active_attempt_owner = ?2, \
                     active_attempt_epoch = ?3, active_attempt_binding = ?4, active_attempt_closing = FALSE WHERE run_id = ?1"
            ))
            .bind(&claim.run_id.0)
            .bind(&claim.owner)
            .bind(epoch)
            .bind(binding_json)
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        }
        tx.commit().await.map_err(store_error)?;
        Ok(admission)
    }
}

impl SqliteDispatchStore {
    pub(super) async fn finish_physical_attempt_with(
        &self,
        claim: &RunClaim,
        purpose: crate::dispatch::AttemptFinish<'_>,
    ) -> Result<SettleOutcome, DispatchError> {
        let (_authority, mut connection) = self.with_conn().await?;
        let mut tx = immediate(&mut connection).await?;
        let p = NS;
        let epoch = durable_i64("dispatch attempt epoch", claim.epoch)?;
        let current: Option<crate::dispatch::PhysicalAttemptStorageRow<String>> = sqlx::query_as(&format!("SELECT active_attempt_owner, active_attempt_epoch, active_attempt_binding, request, active_attempt_closing FROM {p}_dispatch WHERE run_id = ?1"))
            .bind(&claim.run_id.0).fetch_optional(&mut *tx).await.map_err(store_error)?;
        let row_exists = current.is_some();
        let (slot, closing, original) = if let Some((owner, epoch, binding, request, closing)) =
            current
        {
            let epoch = epoch.map(crate::stored_claim_epoch).transpose()?;
            let slot = PhysicalAttemptSlot::from_facts(
                owner.is_some(),
                epoch.is_some(),
                owner.as_deref() == Some(&claim.owner),
                epoch == Some(claim.epoch),
            );
            let original = if purpose.requires_fact() {
                let request = serde_json::from_str::<RunDispatch>(&request).map_err(json_err)?;
                let mut original = crate::dispatch::decode_physical_attempt(
                    &claim.run_id,
                    owner,
                    epoch,
                    binding,
                    closing,
                )?;
                if let Some(original) = &mut original {
                    original.thread_id = Some(request.thread_id().clone());
                    original.session_thread_id = request.session_thread_id;
                }
                original
            } else {
                None
            };
            (slot, closing, original)
        } else {
            (PhysicalAttemptSlot::Empty, false, None)
        };
        let prior = if purpose.requires_fact() {
            read_stored_operation_page(
                &mut tx,
                DispatchCursor::default(),
                usize::MAX,
                Some(&claim.run_id),
                crate::dispatch::OperationReadPurpose::AttemptClosure,
            )
            .await?
        } else {
            Vec::new()
        };
        let (transition, outcome, fact) = crate::dispatch::plan_physical_attempt_finish(
            claim,
            purpose,
            slot,
            closing,
            original.as_ref(),
            prior
                .iter()
                .filter_map(|event| event.operation.physical_fact()),
            row_exists,
        )?;
        if transition == PhysicalAttemptTransition::Clear {
            // Both append and exact slot retirement share this original
            // transaction. Append failure cannot discard the original binding.
            if let Some(fact) = &fact {
                insert_operation(&mut tx, p, fact, self.clock.now_ms()).await?;
            }
            sqlx::query(&format!("UPDATE {p}_dispatch SET active_attempt_owner = NULL, active_attempt_epoch = NULL, active_attempt_binding = NULL, active_attempt_closing = FALSE, retired_attempt_epoch = ?3 WHERE run_id = ?1 AND active_attempt_owner = ?2 AND active_attempt_epoch = ?3"))
                .bind(&claim.run_id.0).bind(&claim.owner).bind(epoch).execute(&mut *tx).await.map_err(store_error)?;
        }
        tx.commit().await.map_err(store_error)?;
        Ok(outcome)
    }
}

impl SqliteDispatchStore {
    pub(super) async fn close_physical_attempt(
        &self,
        claim: &RunClaim,
    ) -> Result<SettleOutcome, DispatchError> {
        let claim = claim.clone();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;
        let row: Option<(Option<String>, Option<i64>)> = sqlx::query_as(&format!(
            "SELECT active_attempt_owner, active_attempt_epoch FROM {p}_dispatch WHERE run_id = ?1"
        ))
        .bind(&claim.run_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let Some((owner, epoch)) = row else {
            return Ok(SettleOutcome::Fenced);
        };
        let epoch = epoch.map(crate::stored_claim_epoch).transpose()?;
        let slot = PhysicalAttemptSlot::from_facts(
            owner.is_some(),
            epoch.is_some(),
            owner.as_deref() == Some(&claim.owner),
            epoch == Some(claim.epoch),
        );
        let outcome = crate::dispatch::decide_physical_attempt_close(slot)?;
        if outcome == SettleOutcome::Applied {
            sqlx::query(&format!(
                "UPDATE {p}_dispatch SET active_attempt_closing = TRUE WHERE run_id = ?1"
            ))
            .bind(&claim.run_id.0)
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        }
        tx.commit().await.map_err(store_error)?;
        Ok(outcome)
    }
}
