// @formal entrypoint ingress.pending_input_exactly_once
//! Atomic dispatch and pending-input settlement for the in-memory adapter.
//!
//! This stays inside the Memory store's single authority mutex and consumes the
//! same settlement plan as the two durable SQL adapters.

use super::*;

impl MemoryDispatchStore {
    pub(super) async fn settle_dispatch(
        &self,
        run_id: &RunId,
        epoch: u64,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<SettleOutcome, DispatchError> {
        let _authority = self.authority.lock().await;
        let mut state = lock(&self.state)?;
        // Fence: apply only while the caller still holds the current epoch. A stale
        // owner (lower epoch, or a gone row) changes nothing — the reclaimer's
        // in-flight state is inviolate.
        let Some((plan, lease)) = state.rows.get(run_id).and_then(|row| {
            let slot = PhysicalAttemptSlot::from_facts(
                row.active_attempt.is_some(),
                row.active_attempt.is_some(),
                false,
                false,
            );
            let plan = pending_input_settlement_plan(
                row.transition(),
                epoch,
                outcome,
                slot,
                run_id,
                consumed,
            );
            row.lease.clone().map(|lease| (plan, lease))
        }) else {
            return Ok(SettleOutcome::Fenced);
        };
        if matches!(plan, PendingInputSettlementPlan::Fenced) {
            return Ok(SettleOutcome::Fenced);
        }
        let operation = DispatchOperation::Settled {
            claim: RunClaim::from(&lease),
            outcome,
        };
        match plan {
            PendingInputSettlementPlan::Done { .. } => {
                let sequence = state.completions.len() as u64 + 1;
                let request = state.rows.get(run_id).map(|row| {
                    (
                        row.request.thread_id().clone(),
                        row.request.session_thread_id.clone(),
                        row.request.admission_fingerprint(),
                    )
                });
                state.completions.push(DispatchCompletion {
                    sequence,
                    run_id: run_id.clone(),
                    thread_id: request.as_ref().map(|(thread, _, _)| thread.clone()),
                    session_thread_id: request.as_ref().and_then(|(_, parent, _)| parent.clone()),
                    request_fingerprint: request.map(|(_, _, fingerprint)| fingerprint),
                });
                state.rows.remove(run_id);
                state.order.retain(|r| r != run_id);
                // Drop the run's own pending and anything else the worker consumed
                // this attempt (e.g. unbound idle-thread input, ADR-0021).
                state
                    .pending
                    .retain(|p| !plan.removes(&p.input.run_id, &p.input.message_id));
            }
            PendingInputSettlementPlan::Awaiting { next, .. } => {
                if let Some(row) = state.rows.get_mut(run_id) {
                    row.apply_transition(next);
                    row.lease = None;
                    // Reaching a checkpoint refreshes the crash-retry budget.
                    row.attempt_count = 0;
                }
                // Drop only what the worker consumed; input that arrived during
                // the attempt stays for the next wake.
                state
                    .pending
                    .retain(|p| !plan.removes(&p.input.run_id, &p.input.message_id));
            }
            PendingInputSettlementPlan::Fenced => unreachable!("handled before mutation"),
        }
        push_operation(&mut state, operation);
        Ok(SettleOutcome::Applied)
    }
}
