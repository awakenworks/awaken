// @formal entrypoint ingress.pending_input_exactly_once
//! Atomic dispatch and pending-input settlement for the in-memory adapter.
//!
//! This stays inside the Memory store's single authority mutex and consumes the
//! same settlement plan as the two durable SQL adapters.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
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
                        row.cancellation_requested,
                    )
                });
                let retained_request = state
                    .rows
                    .get(run_id)
                    .filter(|row| row.request.needs_background_task_control())
                    .map(|row| Box::new(row.request.clone()));
                state.completions.push(CompletionRecord {
                    receipt: DispatchCompletion {
                        sequence,
                        run_id: run_id.clone(),
                        thread_id: request.as_ref().map(|(thread, _, _, _)| thread.clone()),
                        session_thread_id: request
                            .as_ref()
                            .and_then(|(_, parent, _, _)| parent.clone()),
                        cancellation_requested: request
                            .as_ref()
                            .is_some_and(|(_, _, _, cancelled)| *cancelled),
                        request_fingerprint: request.map(|(_, _, fingerprint, _)| fingerprint),
                    },
                    retained_request,
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

#[cfg(test)]
mod tests {
    use super::super::{MemoryDispatchStore, lock};
    use awaken_dispatch_contract::{
        DispatchClaims as _, DispatchControlReads as _, DispatchEnqueue as _, DispatchError,
        DispatchOutcome, DispatchSettlement as _, RunDispatch, SessionRunAdmissionObservation,
        SessionRunAdmissionReads as _,
    };

    #[tokio::test]
    async fn memory_retained_source_uncertainty_does_not_erase_or_repair_the_completion() {
        // M1 original Task request Done in the SAME record => exact retained
        // value. M2 NULL/foreign retained source => original completion receipt
        // and feed survive, source is absent/Unavailable. M3 caller replay can
        // neither heal it with supplied data nor recreate a claimable row.
        let store = MemoryDispatchStore::new(awaken_time::system_clock());
        let mut activation =
            awaken_dispatch_testkit::worker_http::activation("memory-retained-source");
        activation
            .snapshot
            .resolved_spec
            .plugin_ids
            .push("background_task".into());
        activation.snapshot.recompute_fingerprint().unwrap();
        // C0 final Native/Task inputs publish their intrinsic codec demands
        // through the canonical producer before enqueue. This remains a Task
        // row; the local claim below advertises no registered Worker manifest.
        let request = RunDispatch::new(activation).with_worker_codec_requirements();
        store.enqueue(request.clone()).await.unwrap();
        let claimed = store
            .claim_run(
                request.run_id(),
                "original-source-owner",
                30_000,
                &Default::default(),
            )
            .await
            .unwrap()
            .unwrap();
        store
            .settle(
                request.run_id(),
                claimed.lease.epoch(),
                DispatchOutcome::Done,
                &[],
            )
            .await
            .unwrap();
        let SessionRunAdmissionObservation::Completed {
            completion,
            retained_request,
        } = store
            .observe_session_run_admission(request.run_id())
            .await
            .unwrap()
        else {
            panic!("M1 completion");
        };
        assert_eq!(
            retained_request.unwrap().as_deref(),
            Some(&request),
            "M1 exact original"
        );
        let feed = store.completion_events_after(0, 100).await.unwrap();
        for foreign in [false, true] {
            {
                let mut state = lock(&store.state).unwrap();
                let record = state.completions.last_mut().unwrap();
                record.retained_request = if foreign {
                    let mut wrong = request.clone();
                    wrong.activation.thread_id.0.push_str("-foreign");
                    Some(Box::new(wrong))
                } else {
                    None
                };
            }
            let observed = store
                .observe_session_run_admission(request.run_id())
                .await
                .unwrap();
            let SessionRunAdmissionObservation::Completed {
                completion: receipt,
                retained_request,
            } = &observed
            else {
                panic!("M2 receipt");
            };
            assert_eq!(receipt, &completion, "M2 valid receipt unaffected");
            if foreign {
                assert!(
                    matches!(retained_request, Err(DispatchError::Unavailable(_))),
                    "M2 corrupt source Unknown"
                );
            } else {
                assert_eq!(retained_request, &Ok(None), "M2 absent source not guessed");
            }
            store.enqueue(request.clone()).await.unwrap();
            assert_eq!(
                store
                    .observe_session_run_admission(request.run_id())
                    .await
                    .unwrap(),
                observed,
                "M3 no caller-source repair"
            );
            assert_eq!(
                store.completion_events_after(0, 100).await.unwrap(),
                feed,
                "M3 no feed change"
            );
            assert!(
                store
                    .claim_run(
                        request.run_id(),
                        "foreign-owner",
                        30_000,
                        &Default::default()
                    )
                    .await
                    .unwrap()
                    .is_none(),
                "M3 no resurrection"
            );
        }
    }
}
