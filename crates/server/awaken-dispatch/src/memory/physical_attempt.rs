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
        closure: Option<&crate::AttemptClosureRecord>,
    ) -> Result<SettleOutcome, DispatchError> {
        if let Some(record) = closure {
            record.validate()?;
        }
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        let prior = |state: &State| {
            state
                .operations
                .iter()
                .filter_map(|event| event.operation.closure())
                .filter(|record| record.attempt().claim.run_id == claim.run_id)
                .cloned()
                .collect::<Vec<_>>()
        };
        let Some(row) = state.rows.get(&claim.run_id) else {
            return match closure {
                Some(record) => {
                    crate::dispatch::attempt_closure_replay(record, prior(&state).iter())
                }
                None => Ok(SettleOutcome::Fenced),
            };
        };
        let slot = match row.active_attempt.as_ref() {
            None => PhysicalAttemptSlot::Empty,
            Some(active) if active == claim => PhysicalAttemptSlot::ExactClaim,
            Some(_) => PhysicalAttemptSlot::OtherClaim,
        };
        let (transition, outcome) = crate::dispatch::decide_physical_attempt_finish(
            slot,
            row.active_attempt_closing,
            closure.is_some(),
        )?;
        if let Some(record) = closure {
            if transition == PhysicalAttemptTransition::Stutter {
                return crate::dispatch::attempt_closure_replay(record, prior(&state).iter());
            }
            if transition == PhysicalAttemptTransition::Clear {
                if row.physical_attempt()?.as_ref() != Some(record.attempt()) {
                    return Err(DispatchError::Conflict(
                        "closure subject differs from the frozen physical attempt".into(),
                    ));
                }
                if prior(&state)
                    .iter()
                    .any(|old| old.attempt().claim == claim.clone())
                {
                    return Err(DispatchError::Conflict(
                        "a retired closure subject cannot be asserted for another active entry"
                            .into(),
                    ));
                }
            }
        }
        if transition == PhysicalAttemptTransition::Clear {
            let row = state
                .rows
                .get_mut(&claim.run_id)
                .expect("locked original row");
            row.active_attempt = None;
            row.active_attempt_binding = None;
            row.active_attempt_closing = false;
            row.retired_attempt_epoch = claim.epoch;
            if let Some(record) = closure {
                push_operation(&mut state, record);
            }
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
