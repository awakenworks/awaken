// @formal evidence run_ingress.failed_attempt_release_spends_one_attempt_and_exhausts_through_retry_exhaustion
// @formal surface exclude=test_support_crate
// @formal owner crates/platform/dispatch/awaken-dispatch-testkit/src/lib.rs
// @formal reason: Static owner: the failed-attempt retry budget rule of the shared dispatch conformance.
// @formal reason: Dynamic path: the tooling-only testkit includes this file for every store backend; it is
// @formal reason: absent from the product runtime graph. Evidence: crates/platform/dispatch/awaken-dispatch-testkit/src/lib.rs.
/// Failed-attempt retry budget decision table (ADR-0015 D1). A claim returned
/// after a classified failed attempt spends one attempt; the attempt that
/// reaches `max_attempts` is terminalized by the same retry-exhaustion claim as
/// a crashed attempt; a deferral never spends budget. The rows hold for any
/// store clock: a zero delay is due at once and an hour-long one is not.
///
/// | Rule | cause | effect |
/// |---|---|---|
/// | B1 | failed attempt, budget remaining | Pending with one attempt spent; due after the delay; no exhaustion claim |
/// | B2 | deferral between failed attempts | Pending, attempt count unchanged |
/// | B3 | failed attempt reaching `max_attempts` (last) | Leased under an expired lease with one attempt spent; ordinary claims wait for the delay; retry exhaustion claims it at once with a newer epoch |
/// | B4 | failed attempt past `max_attempts` (an exhausted row an ordinary claim recovered) | as B3 |
/// | B5 | stale claim after the hand-off | fenced, attempt count unchanged |
async fn failed_attempts_spend_the_budget_and_exhaust_like_a_crash(
    store: &dyn ConformanceDispatch,
    ns: &str,
) {
    const MAX_ATTEMPTS: u64 = 3;
    const LATER_MS: u64 = 3_600_000;
    async fn row(store: &dyn ConformanceDispatch, run: &RunId) -> (DispatchState, u64) {
        let summary = store
            .list_dispatches()
            .await
            .expect("list dispatches")
            .into_iter()
            .find(|summary| &summary.run_id == run)
            .expect("the dispatch row is retained");
        (summary.state, summary.attempt_count)
    }
    async fn claim(store: &dyn ConformanceDispatch, run: &RunId, owner: &str) -> RunClaim {
        let claimed = store
            .claim_run(run, owner, LEASE_MS, &Default::default())
            .await
            .expect("claim")
            .expect("the Run is due");
        RunClaim::from(&claimed.lease)
    }
    async fn retry(
        store: &dyn ConformanceDispatch,
        claim: &RunClaim,
        retry_after_ms: u64,
        max_attempts: u64,
    ) -> SettleOutcome {
        store
            .retry_claim(claim, retry_after_ms, max_attempts)
            .await
            .expect("retry claim")
    }

    let request = dispatch(ns, "failed-attempt-budget", "failed-attempt-budget-thread");
    let run = request.run_id().clone();
    store.enqueue(request).await.expect("enqueue");
    let mut stale = Vec::new();
    for spent in 1..MAX_ATTEMPTS {
        let failed = claim(store, &run, "failing-worker").await;
        assert_eq!(
            retry(store, &failed, 0, MAX_ATTEMPTS).await,
            SettleOutcome::Applied,
            "B1 attempt {spent}"
        );
        assert_eq!(
            row(store, &run).await,
            (DispatchState::Pending, spent),
            "B1 attempt {spent}"
        );
        assert!(
            store
                .claim_retry_exhausted("terminalizer", LEASE_MS, MAX_ATTEMPTS)
                .await
                .expect("B1 exhaustion query")
                .is_none(),
            "B1 attempt {spent}: budget remains"
        );
        let deferred = claim(store, &run, "busy-worker").await;
        assert_eq!(
            store.defer_claim(&deferred, 0).await.expect("B2 defer"),
            SettleOutcome::Applied,
            "B2"
        );
        assert_eq!(
            row(store, &run).await,
            (DispatchState::Pending, spent),
            "B2 a deferral keeps the budget"
        );
        stale.extend([failed, deferred]);
    }

    let last = claim(store, &run, "failing-worker").await;
    assert_eq!(
        retry(store, &last, LATER_MS, MAX_ATTEMPTS).await,
        SettleOutcome::Applied,
        "B3"
    );
    assert_eq!(
        row(store, &run).await,
        (DispatchState::Leased, MAX_ATTEMPTS),
        "B3 left for retry exhaustion"
    );
    assert!(
        store
            .claim_run(&run, "other-worker", LEASE_MS, &Default::default())
            .await
            .expect("B3 ordinary claim")
            .is_none(),
        "B3 ordinary recovery waits for the retry deadline"
    );
    let terminal = store
        .claim_retry_exhausted("terminalizer", LATER_MS, MAX_ATTEMPTS)
        .await
        .expect("B3 exhaustion query")
        .expect("B3 retry exhaustion claims the exhausted Run at once");
    assert_eq!(terminal.lease.run_id(), &run, "B3");
    assert!(terminal.lease.epoch() > last.epoch, "B3 a newer epoch");
    stale.push(last);
    // The exhaustion claim is itself a recovery re-claim, as for a crash.
    let exhausted = row(store, &run).await;
    for claim in &stale {
        assert_eq!(
            retry(store, claim, 0, MAX_ATTEMPTS).await,
            SettleOutcome::Fenced,
            "B5"
        );
    }
    assert_eq!(row(store, &run).await, exhausted, "B5 unchanged");
    assert_eq!(
        store
            .settle(&run, terminal.lease.epoch(), DispatchOutcome::Done, &[])
            .await
            .expect("B3 settle"),
        SettleOutcome::Applied,
        "B3 the terminal claim settles"
    );

    let request = dispatch(
        ns,
        "failed-attempt-overrun",
        "failed-attempt-overrun-thread",
    );
    let run = request.run_id().clone();
    store.enqueue(request).await.expect("enqueue");
    for _ in 0..MAX_ATTEMPTS {
        let failed = claim(store, &run, "failing-worker").await;
        assert_eq!(
            retry(store, &failed, 0, MAX_ATTEMPTS).await,
            SettleOutcome::Applied,
            "B4 setup"
        );
    }
    let recovered = store
        .claim_run(&run, "other-worker", LEASE_MS, &Default::default())
        .await
        .expect("B4 ordinary claim")
        .expect("B4 an expired exhausted lease stays recoverable");
    assert!(recovered.recovered, "B4 a recovery claim");
    assert_eq!(
        row(store, &run).await,
        (DispatchState::Leased, MAX_ATTEMPTS + 1),
        "B4 the recovery spent one more attempt"
    );
    assert_eq!(
        retry(store, &RunClaim::from(&recovered.lease), 0, MAX_ATTEMPTS).await,
        SettleOutcome::Applied,
        "B4"
    );
    assert_eq!(
        row(store, &run).await,
        (DispatchState::Leased, MAX_ATTEMPTS + 2),
        "B4 one attempt spent, left for retry exhaustion"
    );
    let terminal = store
        .claim_retry_exhausted("terminalizer", LATER_MS, MAX_ATTEMPTS)
        .await
        .expect("B4 exhaustion query")
        .expect("B4 retry exhaustion claims it");
    assert_eq!(terminal.lease.run_id(), &run, "B4");
    assert_eq!(
        store
            .settle(&run, terminal.lease.epoch(), DispatchOutcome::Done, &[])
            .await
            .expect("B4 settle"),
        SettleOutcome::Applied,
        "B4 the terminal claim settles"
    );
}
