// @formal evidence run_ingress.relinquish_requires_exact_current_claim_and_preserves_retry_budget
// @formal evidence run_ingress.failed_attempt_release_spends_one_attempt_and_exhausts_through_retry_exhaustion
//! Blocked recovery re-claims spend no retry budget, within a bounded window,
//! on every dispatch store backend.

use super::*;

/// The store half of `blocked_reclaim_attempt_count`: a predecessor's
/// physical slot stays occupied while its closure progresses, and each
/// recovery re-claim finds it Blocked and defers.
///
/// | Rule | cause | effect |
/// |---|---|---|
/// | S1 | re-claim `k <= BLOCKED_RECLAIM_LIMIT` after the predecessor's epoch is Blocked and defers | `attempt_count` returns to its value before the re-claim; the row stays Leased and recovered |
/// | S2 | re-claim `k > BLOCKED_RECLAIM_LIMIT` is Blocked and defers | the re-claim's attempt stays spent |
/// | S3 | S2 repeats until the budget is spent; the predecessor's slot is still occupied | the retry-exhaustion claim is refused: the budget never overrides an occupied slot (ADR-0078), the row stays Leased |
/// | S4 | throughout | the predecessor's slot is never cleared by a successor |
/// | S5 | the predecessor's own claim then closes its slot (`finish_attempt`) | the retry-exhaustion claim takes the Run at once, with no fresh budget |
///
/// A predecessor whose slot never closes is resolved only by its process-end
/// evidence (exact exit, Superseded) or operator quarantine.
pub async fn assert_blocked_reclaims_spend_no_budget_within_the_window<
    S: awaken_dispatch::ControlDispatch + awaken_dispatch::DispatchReads,
>(
    store: &S,
    clock: &dyn ConformanceClock,
) {
    use awaken_dispatch::{AttemptAdmission, DispatchState, RunClaim, RunDispatch, SettleOutcome};
    use awaken_dispatch_contract::BLOCKED_RECLAIM_LIMIT;

    const MAX_ATTEMPTS: u64 = 3;

    clock.set(0);
    store
        .enqueue(RunDispatch::new(activation("blocked-reclaim-run")))
        .await
        .expect("enqueue blocked re-claim fixture");
    let predecessor = store
        .claim("owner-a", LEASE_MS, &Default::default())
        .await
        .expect("predecessor claim query")
        .expect("predecessor claim");
    let predecessor_claim = RunClaim::from(&predecessor.lease);
    assert_eq!(
        store
            .begin_attempt(&predecessor_claim, None)
            .await
            .expect("predecessor admission"),
        AttemptAdmission::Applied
    );
    clock.advance_past(predecessor.lease.expires_ms()).await;
    clock.set(predecessor.lease.expires_ms().saturating_add(1));

    let mut spent = 0_u64;
    let mut round = 0_u64;
    while spent < MAX_ATTEMPTS {
        round += 1;
        let reclaimed = store
            .claim("owner-b", DEFAULT_LEASE_MS, &Default::default())
            .await
            .expect("re-claim query")
            .unwrap_or_else(|| panic!("round {round}: the deferred Run is due again"));
        assert!(reclaimed.recovered, "round {round}: a recovery re-claim");
        let claim = RunClaim::from(&reclaimed.lease);
        assert_eq!(
            claim.epoch - predecessor_claim.epoch,
            round,
            "round {round}: one claim per round"
        );
        assert_eq!(
            store.begin_attempt(&claim, None).await.expect("admission"),
            AttemptAdmission::Blocked,
            "round {round}: the predecessor still occupies the slot"
        );
        assert_eq!(
            store.defer_claim(&claim, 0).await.expect("defer"),
            SettleOutcome::Applied,
            "round {round}"
        );
        if round > BLOCKED_RECLAIM_LIMIT {
            spent += 1;
        }
        let row = store.list_dispatches().await.expect("rows")[0].clone();
        assert_eq!(
            row.state,
            DispatchState::Leased,
            "round {round}: recovering"
        );
        assert!(row.physical_attempt_active, "S4 round {round}");
        assert_eq!(
            row.attempt_count,
            spent,
            "{} round {round}",
            if round > BLOCKED_RECLAIM_LIMIT {
                "S2"
            } else {
                "S1"
            }
        );
        if spent < MAX_ATTEMPTS {
            assert!(
                store
                    .claim_retry_exhausted("terminalizer", LEASE_MS, MAX_ATTEMPTS)
                    .await
                    .expect("exhaustion query")
                    .is_none(),
                "round {round}: no premature exhaustion"
            );
        }
    }
    assert_eq!(round, BLOCKED_RECLAIM_LIMIT + MAX_ATTEMPTS, "S1+S2 window");
    assert!(
        store
            .claim_retry_exhausted("terminalizer", LEASE_MS, MAX_ATTEMPTS)
            .await
            .expect("exhaustion query")
            .is_none(),
        "S3 a spent budget never terminalizes over an occupied slot"
    );
    let row = store.list_dispatches().await.expect("rows")[0].clone();
    assert_eq!(row.state, DispatchState::Leased, "S3 still recovering");
    assert!(row.physical_attempt_active, "S3/S4 slot still occupied");
    assert_eq!(row.attempt_count, MAX_ATTEMPTS, "S3 budget spent");
    assert_eq!(
        store
            .finish_attempt(&predecessor_claim, None)
            .await
            .expect("predecessor closes its own slot"),
        SettleOutcome::Applied,
        "S5 the predecessor's exact claim closes its slot"
    );
    assert!(
        store
            .claim_retry_exhausted("terminalizer", LEASE_MS, MAX_ATTEMPTS)
            .await
            .expect("exhaustion query")
            .is_some(),
        "S5 once the slot closes, retry exhaustion takes the Run"
    );
}
