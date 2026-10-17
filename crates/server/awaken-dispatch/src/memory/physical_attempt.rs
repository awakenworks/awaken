// @formal surface obligation=session_execution.one_physical_attempt_per_thread
// @formal reason: Static owner: the in-memory adapter transaction for the existing physical-attempt slot;
// @formal reason: it owns no independent admission rule. Dynamic path: while holding the parent
// @formal reason: MemoryDispatchStore authority lock, it projects the exact lease, retired epoch,
// @formal reason: same-Thread peer occupancy, slot and frozen execution-binding facts, delegates admission
// @formal reason: and binding to the canonical Dispatch kernels, then atomically installs only the
// @formal reason: returned claim and binding transition. Kernel and backend tests cross-check these seams;
// @formal reason: mutex scheduling and complete adapter/store refinement remain executable evidence rather
// @formal reason: than a separate formal-proof claim.
// The Memory adapter's existing physical admission transaction. Kept with
// its owning store; all backends share the Dispatch decision kernel.
use super::*;

impl MemoryDispatchStore {
    pub(super) async fn admit_physical_attempt(
        &self,
        claim: &RunClaim,
        now_ms: u64,
        handoff: bool,
        binding: Option<&AttemptExecutionBinding>,
    ) -> Result<AttemptAdmission, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let Some(row) = state.rows.get(&claim.run_id) else {
            return Ok(AttemptAdmission::Fenced);
        };
        let peer_occupied = state.rows.iter().any(|(id, peer)| {
            id != &claim.run_id
                && peer.request.thread_id() == row.request.thread_id()
                && peer.active_attempt.is_some()
        });
        let current = awaken_dispatch_contract::attempt_epoch_is_open(
            claim.epoch,
            row.retired_attempt_epoch,
        ) && row.state == DispatchState::Leased
            && row.lease_epoch == claim.epoch
            && row.lease.as_ref().is_some_and(|lease| {
                lease.owner() == claim.owner
                    && !crate::clock::deadline_has_passed(Some(lease.expires_ms()), now_ms)
            });
        let slot = match row.active_attempt.as_ref() {
            None => PhysicalAttemptSlot::Empty,
            Some(active) if active == claim => PhysicalAttemptSlot::ExactClaim,
            Some(_) => PhysicalAttemptSlot::OtherClaim,
        };
        let (transition, admission) = crate::dispatch::decide_physical_attempt_admission(
            current,
            slot,
            handoff,
            peer_occupied,
            row.active_attempt_closing,
        )?;
        crate::dispatch::verify_physical_attempt_binding(
            transition,
            claim,
            row.assignment.as_ref(),
            binding,
            row.active_attempt_binding.as_ref(),
        )?;
        if matches!(
            transition,
            PhysicalAttemptTransition::Install | PhysicalAttemptTransition::Replace
        ) {
            let operation =
                crate::dispatch::physical_admission_operation(claim, binding, &row.request);
            push_operation(&mut state, operation);
            let row = state.rows.get_mut(&claim.run_id).expect("locked row");
            row.active_attempt = Some(claim.clone());
            row.active_attempt_binding = binding.cloned();
            row.active_attempt_closing = false;
        }
        Ok(admission)
    }
}

impl MemoryDispatchStore {
    pub(super) async fn finish_physical_attempt_with(
        &self,
        claim: &RunClaim,
        purpose: crate::dispatch::AttemptFinish<'_>,
    ) -> Result<SettleOutcome, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let row = state.rows.get(&claim.run_id);
        let slot = match row.and_then(|row| row.active_attempt.as_ref()) {
            None => PhysicalAttemptSlot::Empty,
            Some(active) if active == claim => PhysicalAttemptSlot::ExactClaim,
            Some(_) => PhysicalAttemptSlot::OtherClaim,
        };
        let closing = row.is_some_and(|row| row.active_attempt_closing);
        // Preserve legacy None's original behavior: it need not decode a
        // historical binding. Evidence-bearing ACKs freeze the actual row.
        let original = if purpose.requires_fact() {
            row.map(Row::physical_attempt).transpose()?.flatten()
        } else {
            None
        };
        let (transition, outcome, fact) = crate::dispatch::plan_physical_attempt_finish(
            claim,
            purpose,
            slot,
            closing,
            original.as_ref(),
            state
                .operations
                .iter()
                .filter(|_| purpose.requires_fact())
                .filter_map(|event| event.operation.physical_fact())
                .filter(|fact| fact.attempt().claim.run_id == claim.run_id),
            row.is_some(),
        )?;
        if transition == PhysicalAttemptTransition::Clear {
            // Validate/compile before either mutation; Memory's authority lock
            // is the same consistency boundary as the two native transactions.
            if let Some(fact) = &fact {
                push_operation(&mut state, fact);
            }
            let row = state
                .rows
                .get_mut(&claim.run_id)
                .expect("locked original row");
            row.active_attempt = None;
            row.active_attempt_binding = None;
            row.active_attempt_closing = false;
            row.retired_attempt_epoch = claim.epoch;
        }
        Ok(outcome)
    }

    pub(super) async fn close_physical_attempt(
        &self,
        claim: &RunClaim,
    ) -> Result<SettleOutcome, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let Some(row) = state.rows.get_mut(&claim.run_id) else {
            return Ok(SettleOutcome::Fenced);
        };
        let slot = match row.active_attempt.as_ref() {
            None => PhysicalAttemptSlot::Empty,
            Some(active) if active == claim => PhysicalAttemptSlot::ExactClaim,
            Some(_) => PhysicalAttemptSlot::OtherClaim,
        };
        let outcome = crate::dispatch::decide_physical_attempt_close(slot)?;
        if outcome != SettleOutcome::Applied {
            return Ok(outcome);
        }
        row.active_attempt_closing = true;
        Ok(SettleOutcome::Applied)
    }
}

// The one typed observation of these same retained slot facts. Summary and
// exact reads share it; neither projection can erase orphan/closing residue.
impl Row {
    pub(super) fn physical_attempt(&self) -> Result<Option<PhysicalAttempt>, DispatchError> {
        crate::dispatch::observed_physical_attempt_is_present(
            self.active_attempt.is_some(),
            self.active_attempt.is_some(),
            self.active_attempt_binding.is_some(),
            self.active_attempt_closing,
        )?;
        self.active_attempt
            .as_ref()
            .map(|claim| {
                if let Some(binding) = &self.active_attempt_binding {
                    binding.validate_for(claim)?;
                }
                Ok(PhysicalAttempt {
                    claim: claim.clone(),
                    binding: self.active_attempt_binding.clone(),
                    thread_id: Some(self.request.thread_id().clone()),
                    session_thread_id: self.request.session_thread_id.clone(),
                    closing: self.active_attempt_closing,
                })
            })
            .transpose()
    }
}

// Physical-retirement facts live beside their one atomic mutation owner.
impl MemoryDispatchStore {
    pub(super) async fn read_retained_attempt_closures(
        &self,
        cursor: DispatchCursor,
        limit: usize,
        run: Option<&RunId>,
    ) -> Result<crate::AttemptClosurePage, DispatchError> {
        let events = lock(&self.state)?
            .operation_page(
                cursor,
                limit,
                run,
                crate::dispatch::OperationReadPurpose::AttemptClosure,
            )
            .into_iter()
            .filter_map(crate::dispatch::StoredDispatchEvent::attempt_closure)
            .collect::<Vec<_>>();
        let next_cursor = events.last().map_or(cursor, |event| event.cursor);
        Ok(crate::AttemptClosurePage {
            events,
            next_cursor,
        })
    }
}

// This existing Execution role belongs with the physical-attempt transaction
// owner; no duplicate port implementation or return policy remains in parent.
#[async_trait]
impl awaken_dispatch_contract::DispatchExecution for MemoryDispatchStore {
    async fn record_credential_realization(
        &self,
        claim: &RunClaim,
        receipt: CredentialRealizationReceipt,
    ) -> Result<SettleOutcome, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let Some(row) = state.rows.get_mut(&claim.run_id) else {
            return Ok(SettleOutcome::Fenced);
        };
        if row.state != DispatchState::Leased
            || row.lease_epoch != claim.epoch
            || row
                .lease
                .as_ref()
                .is_none_or(|lease| lease.owner() != claim.owner)
        {
            return Ok(SettleOutcome::Fenced);
        }
        verify_credential_realization_receipt(&row.credential_bindings, &receipt)
            .map_err(|error| DispatchError::Rejected(error.to_string()))?;
        let existing = row
            .credential_receipts
            .iter()
            .find(|existing| existing.candidate_fingerprint == receipt.candidate_fingerprint);
        match credential_realization_persistence_decision(existing, &receipt) {
            CredentialRealizationPersistenceDecision::ExactReplay => {
                return Ok(SettleOutcome::Applied);
            }
            CredentialRealizationPersistenceDecision::Collision => {
                return Err(DispatchError::Rejected(
                    "credential realization receipt conflicts with committed evidence".to_string(),
                ));
            }
            CredentialRealizationPersistenceDecision::Insert => {}
        }
        row.credential_receipts.push(receipt);
        Ok(SettleOutcome::Applied)
    }

    async fn bind_sandbox(
        &self,
        claim: &RunClaim,
        sandbox_ref: &str,
    ) -> Result<SettleOutcome, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        if let Some(row) = state.rows.get_mut(&claim.run_id)
            && row.state == DispatchState::Leased
            && row.lease_epoch == claim.epoch
            && row
                .lease
                .as_ref()
                .is_some_and(|lease| lease.owner() == claim.owner)
        {
            row.sandbox = Some(sandbox_ref.to_string());
            return Ok(SettleOutcome::Applied);
        }
        Ok(SettleOutcome::Fenced)
    }

    async fn renew_lease(
        &self,
        claim: &RunClaim,
        lease_ms: u64,
    ) -> Result<LeaseRenewalOutcome, DispatchError> {
        let now_ms = self.clock.now_ms();
        let mut state = lock(&self.state)?;
        let Some(row) = state.rows.get_mut(&claim.run_id) else {
            return Ok(LeaseRenewalOutcome::Lost);
        };
        let current_claim = matches!(
            row.state,
            DispatchState::ReservationLeased | DispatchState::Leased
        ) && row
            .lease
            .as_ref()
            .is_some_and(|lease| lease.owner() == claim.owner && lease.epoch() == claim.epoch);
        let cancelled_physical_attempt = row.cancellation_requested
            && row
                .active_attempt
                .as_ref()
                .is_some_and(|active| active == claim);
        let outcome = decide_lease_renewal(current_claim, cancelled_physical_attempt);
        if outcome == LeaseRenewalOutcome::Current
            && let Some(lease) = row.lease.as_mut()
        {
            *lease = lease.with_deadline(crate::clock::deadline_millis(now_ms, lease_ms));
        }
        Ok(outcome)
    }

    async fn begin_attempt(
        &self,
        claim: &RunClaim,
        _binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
    ) -> Result<AttemptAdmission, DispatchError> {
        let now_ms = self.clock.now_ms();
        self.admit_physical_attempt(claim, now_ms, false, _binding)
            .await
    }

    async fn handoff_attempt(
        &self,
        claim: &RunClaim,
        _binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
    ) -> Result<AttemptAdmission, DispatchError> {
        let now_ms = self.clock.now_ms();
        self.admit_physical_attempt(claim, now_ms, true, _binding)
            .await
    }

    async fn close_attempt(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        self.close_physical_attempt(claim).await
    }

    async fn finish_attempt(
        &self,
        claim: &RunClaim,
        cause: Option<awaken_dispatch_contract::AttemptReturnCause>,
    ) -> Result<SettleOutcome, DispatchError> {
        self.finish_physical_attempt_with(claim, crate::dispatch::AttemptFinish::Ordinary(cause))
            .await
    }

    async fn relinquish_claim(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        release_claim(&mut state, claim, None, ReleasedAttempt::Unspent)
    }

    async fn defer_claim(
        &self,
        claim: &RunClaim,
        retry_after_ms: u64,
    ) -> Result<SettleOutcome, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        release_claim(
            &mut state,
            claim,
            Some(crate::clock::deadline_millis(
                self.clock.now_ms(),
                retry_after_ms,
            )),
            ReleasedAttempt::Unspent,
        )
    }

    async fn retry_claim(
        &self,
        claim: &RunClaim,
        retry_after_ms: u64,
        max_attempts: u64,
    ) -> Result<SettleOutcome, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        release_claim(
            &mut state,
            claim,
            Some(crate::clock::deadline_millis(
                self.clock.now_ms(),
                retry_after_ms,
            )),
            ReleasedAttempt::Failed { max_attempts },
        )
    }
}
