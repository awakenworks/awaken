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
            && row
                .lease
                .as_ref()
                .is_some_and(|lease| lease.owner == claim.owner && lease.expires_ms >= now_ms);
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
        }
        Ok(admission)
    }
}
