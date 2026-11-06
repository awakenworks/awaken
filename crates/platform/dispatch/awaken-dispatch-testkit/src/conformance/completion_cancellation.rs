// @formal surface exclude=test_support_crate
// @formal owner crates/platform/dispatch/awaken-dispatch-testkit/src/lib.rs
// @formal reason: Static owner: the completion cancel-fact rule of the shared dispatch conformance.
// @formal reason: Dynamic path: the tooling-only testkit includes this file for every store backend; it is
// @formal reason: absent from the product runtime graph. Evidence: crates/platform/dispatch/awaken-dispatch-testkit/src/lib.rs.
/// Completion cancel-fact decision table (ADR-0060, 2026-10-31 amendment). The
/// row's durable cancel request is copied onto the completion tombstone in the
/// same settlement transaction that removes the row, so an Outcome reading
/// after settle still sees it. Both completion reads return the same fact.
///
/// | Rule | cause | effect |
/// |---|---|---|
/// | K1 | cancel requested on the leased row, cancellation claim settles `Done` | completion carries `cancellation_requested = true` |
/// | K2 | ordinary claim settles `Done`, no cancel request | completion carries `cancellation_requested = false` |
/// | K3 | either completion read again (feed page and admission observation) | identical value; the fact never changes after settle |
async fn completion_retains_the_settled_cancel_request(store: &dyn ConformanceDispatch, ns: &str) {
    async fn settle_done(store: &dyn ConformanceDispatch, run: &RunId, owner: &str) {
        let claimed = store
            .claim_run(run, owner, LEASE_MS, &Default::default())
            .await
            .expect("claim")
            .expect("the Run is due");
        assert_eq!(
            store
                .settle(run, claimed.lease.epoch(), DispatchOutcome::Done, &[])
                .await
                .expect("settle"),
            SettleOutcome::Applied
        );
    }
    async fn completion(
        store: &dyn ConformanceDispatch,
        run: &RunId,
    ) -> awaken_dispatch_contract::DispatchCompletion {
        let fed = store
            .completion_events_after(0, usize::MAX)
            .await
            .expect("completion feed")
            .into_iter()
            .find(|completion| &completion.run_id == run)
            .expect("settled Run has one completion tombstone");
        assert_eq!(
            store
                .observe_session_run_admission(run)
                .await
                .expect("admission observation"),
            awaken_dispatch_contract::SessionRunAdmissionObservation::Completed {
                completion: fed.clone(),
                retained_request: Ok(None)
            },
            "K3 both completion reads agree"
        );
        fed
    }

    let cancelled = dispatch(
        ns,
        "completion-cancel-requested",
        "completion-cancel-requested-thread",
    );
    let cancelled_run = cancelled.run_id().clone();
    store.enqueue(cancelled).await.expect("enqueue cancelled");
    store
        .claim_run(
            &cancelled_run,
            "interrupted-worker",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("first claim")
        .expect("the Run is due");
    assert!(
        store
            .cancel(&cancelled_run)
            .await
            .expect("cancel leased Run")
            .is_some(),
        "K1 a leased Run accepts the durable cancel request"
    );
    settle_done(store, &cancelled_run, "cancelling-worker").await;
    assert!(
        completion(store, &cancelled_run)
            .await
            .cancellation_requested,
        "K1 the cancel request survives settlement"
    );

    let plain = dispatch(ns, "completion-plain", "completion-plain-thread");
    let plain_run = plain.run_id().clone();
    store.enqueue(plain).await.expect("enqueue plain");
    settle_done(store, &plain_run, "plain-worker").await;
    assert!(
        !completion(store, &plain_run).await.cancellation_requested,
        "K2 an uncancelled settle records no cancel request"
    );
    assert!(
        completion(store, &cancelled_run)
            .await
            .cancellation_requested,
        "K3 the settled fact is stable"
    );
}
