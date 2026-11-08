// @formal entrypoint run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
// @formal entrypoint run_ingress.failed_attempt_release_spends_one_attempt_and_exhausts_through_retry_exhaustion
//! The reference store's dead-letter of one expired lease, shared by operator
//! quarantine and the refusal of a lease generation that has no successor.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

/// Dead-letter one expired `Leased` row under its own expired lease through
/// the kernel's one retry-exhaustion transition, when the shared slot kernel
/// admits it. `reason` names why the lease was lost. Returns the two
/// operational facts to append, or `None` when the row stays.
pub(super) fn dead_letter_expired_lease(
    row: &mut Row,
    reason: LeaseLossReason,
    dead_lettered_at_ms: u64,
) -> Option<[DispatchOperation; 2]> {
    let claim = RunClaim::from(row.lease.as_ref()?);
    // One shared kernel decides the slot disposition here too, so the
    // in-memory backend cannot drift from SQLite/Postgres.
    let slot = PhysicalAttemptSlot::from_facts(
        row.active_attempt.is_some(),
        row.active_attempt.is_some(),
        row.active_attempt.as_ref() == Some(&claim),
        row.active_attempt.as_ref() == Some(&claim),
    );
    if !slot.permits_settlement() && !slot.quarantine_admits_occupation(row.cancellation_requested)
    {
        return None;
    }
    let GuardedTransition::Applied(next) = row.transition().exhaust_retries(claim.epoch, true)
    else {
        return None;
    };
    row.apply_transition(next);
    row.lease = None;
    row.dead_lettered_at = Some(dead_lettered_at_ms);
    Some([
        DispatchOperation::LeaseLost {
            claim: claim.clone(),
            reason,
        },
        DispatchOperation::DeadLettered {
            claim,
            attempt_count: row.attempt_count,
        },
    ])
}

/// Whether this row's persisted lease generation has no successor: the
/// kernel's own claim refusal, never a local restatement of the counter.
pub(super) fn claim_generation_exhausted(row: &Row) -> bool {
    DispatchTransition {
        state: DispatchState::Pending,
        ..row.transition()
    }
    .claim()
    .is_err()
}

/// Settle every expired lease whose generation has no successor before a claim
/// scan selects: each is dead-lettered once, permanently, with `LeaseLost`
/// carrying the integrity code, so no scan ever selects it again and the
/// candidates behind it stay claimable in this same scan.
pub(super) fn refuse_untrustworthy_expired_leases(state: &mut State, now_ms: u64) {
    let now_ms = crate::clock::normalize_millis(now_ms);
    let mut operations = Vec::new();
    for run_id in &state.order {
        let Some(row) = state.rows.get_mut(run_id) else {
            continue;
        };
        if row.state != DispatchState::Leased
            || !crate::clock::deadline_has_passed(
                row.lease.as_ref().map(|lease| lease.expires_ms()),
                now_ms,
            )
            || !claim_generation_exhausted(row)
        {
            continue;
        }
        if let Some(facts) = dead_letter_expired_lease(
            row,
            LeaseLossReason::LeaseGenerationIntegrityViolated,
            now_ms,
        ) {
            crate::observe_untrustworthy_claim_generation(
                &run_id.0,
                crate::UntrustworthyClaimGeneration::DeadLettered,
            );
            operations.extend(facts);
        }
    }
    for operation in operations {
        push_operation(state, operation);
    }
}

/// The claim scans' admission of one candidate's generation: a generation with
/// no successor is never selected, and each scan that reaches it observes it.
pub(super) fn claim_generation_admits(row: &Row) -> bool {
    if claim_generation_exhausted(row) {
        crate::observe_untrustworthy_claim_generation(
            &row.request.run_id().0,
            crate::UntrustworthyClaimGeneration::Skipped,
        );
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    //! The reference store's rows of the decision table in
    //! `tests/harness/untrustworthy_generation.rs` (U1-U7), written against
    //! its private state because integration tests cannot reach it.
    use super::super::MemoryDispatchStore;
    use super::*;
    use awaken_dispatch_contract::{
        DispatchClaims as _, DispatchEnqueue as _, DispatchOperationalFeed as _, DispatchReads as _,
    };
    use awaken_lease_contract::fence::EpochIntegrityViolation;
    use awaken_reliability_testkit::events::CapturedEvents;
    use tracing::instrument::WithSubscriber as _;

    const MAX_ATTEMPTS: u64 = 3;

    fn poison(store: &MemoryDispatchStore, tag: &str, expired_attempts: Option<u64>) {
        let mut state = lock(&store.state).unwrap_or_else(|poisoned| panic!("{poisoned}"));
        let row = state
            .rows
            .get_mut(&RunId(format!("run-{tag}")))
            .expect("seeded row");
        row.lease_epoch = u64::MAX;
        if let Some(attempts) = expired_attempts {
            let lease = row.lease.take().expect("a claimed row");
            row.lease = Some(
                DispatchLease::from_persisted(
                    lease.run_id().clone(),
                    lease.owner().to_owned(),
                    0,
                    u64::MAX,
                )
                .unwrap(),
            );
            row.attempt_count = attempts;
        }
    }

    async fn enqueue(store: &MemoryDispatchStore, tag: &str) {
        store
            .enqueue(RunDispatch::new(
                awaken_dispatch_testkit::worker_http::activation(tag),
            ))
            .await
            .unwrap();
    }

    async fn operations(store: &MemoryDispatchStore, tag: &str) -> Vec<DispatchOperation> {
        store
            .events_after(
                DispatchCursor::default(),
                usize::MAX,
                Some(&RunId(format!("run-{tag}"))),
            )
            .await
            .unwrap()
            .events
            .into_iter()
            .map(|event| event.operation)
            .collect()
    }

    fn integrity_events(captured: &CapturedEvents, tag: &str) -> Vec<BTreeMap<String, String>> {
        captured
            .snapshot()
            .into_iter()
            .filter(|event| {
                event.get("target").map(String::as_str)
                    == Some("awaken::lease_generation_integrity")
                    && event.get("run_id") == Some(&format!("run-{tag}"))
            })
            .collect()
    }

    #[tokio::test]
    async fn claim_scans_dead_letter_an_untrustworthy_generation_and_continue() {
        let clock = awaken_dispatch_testkit::ManualStoreClock::new(10);
        let store = MemoryDispatchStore::new(clock.clock());
        let captured = CapturedEvents::default();
        let caps = Default::default();

        // U1/U2: a retry-exhausted head ahead of a healthy Run.
        enqueue(&store, "u1-poisoned").await;
        store
            .claim("crashed-owner", 60_000, &caps)
            .await
            .unwrap()
            .expect("U1 claimed once, honestly");
        poison(&store, "u1-poisoned", Some(MAX_ATTEMPTS));
        enqueue(&store, "u1-healthy").await;
        let exhausted = store
            .claim_retry_exhausted("pool", 60_000, MAX_ATTEMPTS)
            .with_subscriber(captured.clone())
            .await;
        assert!(matches!(exhausted, Ok(None)), "U1: {exhausted:?}");
        let healthy = store
            .claim("pool", 60_000, &caps)
            .with_subscriber(captured.clone())
            .await
            .unwrap()
            .expect("U2");
        assert_eq!(healthy.lease.run_id().0, "run-u1-healthy", "U2");

        // U3: an ordinary recovery head; the same call claims the Run behind it.
        enqueue(&store, "u3-poisoned").await;
        store
            .claim("crashed-owner", 60_000, &caps)
            .await
            .unwrap()
            .expect("U3 claimed once, honestly");
        poison(&store, "u3-poisoned", Some(1));
        enqueue(&store, "u3-healthy").await;
        let healthy = store
            .claim("pool", 60_000, &caps)
            .with_subscriber(captured.clone())
            .await
            .unwrap()
            .expect("U3");
        assert_eq!(healthy.lease.run_id().0, "run-u3-healthy", "U3");

        // U5: later scans neither claim nor record the refused rows again.
        for _ in 0..3 {
            assert!(
                store
                    .claim_retry_exhausted("pool", 60_000, MAX_ATTEMPTS)
                    .with_subscriber(captured.clone())
                    .await
                    .unwrap()
                    .is_none(),
                "U5"
            );
            assert!(
                store
                    .claim("pool", 60_000, &caps)
                    .with_subscriber(captured.clone())
                    .await
                    .unwrap()
                    .is_none(),
                "U5"
            );
        }
        let dead = store.dead_letters().await.unwrap();
        for tag in ["u1-poisoned", "u3-poisoned"] {
            assert!(dead.contains(&RunId(format!("run-{tag}"))), "U4 {tag}");
            let operations = operations(&store, tag).await;
            let [
                DispatchOperation::Claimed { claim: first },
                DispatchOperation::LeaseLost { claim, reason },
                DispatchOperation::DeadLettered {
                    claim: lettered, ..
                },
            ] = operations.as_slice()
            else {
                panic!("U4/U5 {tag}: one claim, one integrity dead letter: {operations:?}");
            };
            assert_eq!(first.epoch, 1, "U4 {tag}");
            assert_eq!(
                *reason,
                LeaseLossReason::LeaseGenerationIntegrityViolated,
                "U4 {tag}"
            );
            assert_eq!(claim.epoch, u64::MAX, "U4 {tag}");
            assert_eq!(lettered, claim, "U4 {tag}");
            let events = integrity_events(&captured, tag);
            assert_eq!(events.len(), 1, "U6 {tag}: {events:?}");
            assert_eq!(events[0]["code"], EpochIntegrityViolation::CODE, "U6 {tag}");
            assert_eq!(events[0]["disposition"], "dead_lettered", "U6 {tag}");
        }

        // U7: a pending generation with no successor and no expired lease.
        enqueue(&store, "u7-poisoned").await;
        poison(&store, "u7-poisoned", None);
        enqueue(&store, "u7-healthy").await;
        let healthy = store
            .claim("pool", 60_000, &caps)
            .with_subscriber(captured.clone())
            .await
            .unwrap()
            .expect("U7");
        assert_eq!(healthy.lease.run_id().0, "run-u7-healthy", "U7");
        assert!(operations(&store, "u7-poisoned").await.is_empty(), "U7");
        let skipped = integrity_events(&captured, "u7-poisoned");
        assert!(
            !skipped.is_empty()
                && skipped
                    .iter()
                    .all(|event| event["disposition"] == "skipped"),
            "U7: {skipped:?}"
        );
    }
}
