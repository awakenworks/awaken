// @formal evidence run_ingress.relinquish_requires_exact_current_claim_and_preserves_retry_budget
// @formal evidence run_ingress.request_local_resolution_retry_honors_persisted_deadline
//! Capacity deferral conformance shared by the dispatch store backends.

use super::*;

/// ADR-0092 decision 3 on the dispatch state machine: a Worker that met the
/// backend's capacity refusal defers the exact claim; the claim then belongs to
/// whichever eligible Worker asks after the deadline, and no deferral spends
/// retry budget.
///
/// | Rule | cause | effect |
/// |---|---|---|
/// | D1 | before the persisted deadline | no Worker can claim the Run |
/// | D2 | at the deadline | another Worker claims the exact Run with a newer epoch |
/// | D3 | after N capacity deferrals | `attempt_count` equals its value before them |
/// | D4 | a stale claim defers after the hand-off | fenced; the new owner keeps the lease |
pub async fn assert_capacity_deferrals_hand_off_without_spending_budget<
    S: awaken_dispatch::ControlDispatch,
>(
    store: &S,
    clock: &dyn ConformanceClock,
) {
    use awaken_dispatch::{DispatchState, RunClaim, RunDispatch, SettleOutcome};

    assert!(
        clock.exact_boundary_is_controllable(),
        "store clock fixture"
    );
    store
        .enqueue(RunDispatch::new(activation("capacity-run")))
        .await
        .unwrap();
    let attempts_before = store.list_dispatches().await.unwrap()[0].attempt_count;
    let owners = ["worker-ns-a", "worker-ns-b", "worker-ns-a"];
    let mut now = 0;
    let mut previous: Option<RunClaim> = None;
    for (round, owner) in owners.into_iter().enumerate() {
        clock.set(now);
        let claimed = store
            .claim(owner, 1_000, &Default::default())
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("D2 round {round}: due Run is claimable by {owner}"));
        if let Some(previous) = &previous {
            assert!(
                claimed.lease.epoch() > previous.epoch,
                "D2 round {round}: a new claim"
            );
            assert_ne!(claimed.lease.owner(), previous.owner, "D2 another Worker");
        }
        let claim = RunClaim::from(&claimed.lease);
        assert_eq!(
            store.defer_claim(&claim, 100).await.unwrap(),
            SettleOutcome::Applied,
            "capacity refusal defers round {round}"
        );
        let listed = store.list_dispatches().await.unwrap();
        assert_eq!(listed[0].state, DispatchState::Pending, "round {round}");
        assert_eq!(listed[0].attempt_count, attempts_before, "D3 round {round}");
        clock.set(now + 99);
        assert!(
            store
                .claim("worker-ns-c", 1_000, &Default::default())
                .await
                .unwrap()
                .is_none(),
            "D1 round {round}"
        );
        previous = Some(claim);
        now += 100;
    }
    clock.set(now);
    let final_owner = store
        .claim("worker-ns-c", 1_000, &Default::default())
        .await
        .unwrap()
        .expect("D2 the deferred Run completes its journey on another Worker");
    let stale = previous.expect("three deferrals");
    assert_eq!(
        store.defer_claim(&stale, 100).await.unwrap(),
        SettleOutcome::Fenced,
        "D4"
    );
    let listed = store.list_dispatches().await.unwrap();
    assert_eq!(listed[0].state, DispatchState::Leased, "D4");
    assert_eq!(
        listed[0].attempt_count, attempts_before,
        "D3 after hand-off"
    );
    assert_eq!(final_owner.lease.owner(), "worker-ns-c", "D2");
}
