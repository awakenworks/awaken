/// Normalized history of the claim/physical-attempt crash boundary. It contains
/// no backend identifiers or timestamps, so two adapters can be compared
/// directly without constructing a parallel Dispatch state model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchRecoveryHistory {
    pub first_epoch: u64,
    pub recovered_epoch: u64,
    pub first_attempt: AttemptAdmission,
    pub blocked_successor: AttemptAdmission,
    pub stale_settlement: SettleOutcome,
    pub predecessor_finish: SettleOutcome,
    pub successor_attempt: AttemptAdmission,
    pub successor_finish: SettleOutcome,
    pub successor_settlement: SettleOutcome,
    pub live_rows_after_settlement: usize,
}

/// Drive one exact predecessor-crash/successor-recovery history through a real
/// backend. The production Dispatch transition and physical-attempt reducers
/// remain authoritative; this helper merely records their observable trace.
pub async fn record_dispatch_recovery_history(
    store: &dyn DispatchQueue,
    namespace: &str,
    clock: &dyn ConformanceClock,
) -> DispatchRecoveryHistory {
    /* Cause/effect graph: C1 a fresh Run is claimed by predecessor A; C2 A
     * enters its physical attempt; C3 A's lease expires and B reclaims; C4 A
     * has not quiesced; C5 A later quiesces. Effects: E1 B receives a strictly
     * newer epoch; E2 B is blocked while C4; E3 A's stale settlement is fenced;
     * E4 C5 clears only A's physical slot; E5 B then enters and settles once;
     * E6 no live dispatch remains. Decision trace R1=C1+C2; R2=C3+C4->E1-E3;
     * R3=C5->E4-E6. */
    let request = dispatch(namespace, "history", "history-thread");
    let run_id = request.run_id().clone();
    clock.set(0);
    store.enqueue(request).await.expect("R1 enqueue");
    let first = store
        .claim_run(&run_id, "history-a", LEASE_MS, 0, &Default::default())
        .await
        .expect("R1 claim")
        .expect("R1 claimable");
    let first_claim = RunClaim::from(&first.lease);
    let first_attempt = store
        .begin_attempt(&first_claim, 0)
        .await
        .expect("R1 begin attempt");
    let recovery_now = first.lease.expires_ms.saturating_add(1);
    // Deterministic adapters advance immediately; a live PostgreSQL adapter
    // waits for its authoritative database/wall clock to cross the persisted
    // deadline. Keeping that distinction behind ConformanceClock lets every
    // backend execute this one history without giving PostgreSQL a test-only
    // clock or duplicating the recovery oracle.
    clock.advance_past(first.lease.expires_ms).await;
    let recovered = store
        .claim_run(
            &run_id,
            "history-b",
            LEASE_MS,
            recovery_now,
            &Default::default(),
        )
        .await
        .expect("R2 recover")
        .expect("R2 expired claim recovers");
    let recovered_claim = RunClaim::from(&recovered.lease);
    let blocked_successor = store
        .begin_attempt(&recovered_claim, recovery_now)
        .await
        .expect("R2 successor admission");
    let stale_settlement = store
        .settle(&run_id, first.lease.epoch, DispatchOutcome::Done, &[])
        .await
        .expect("R2 stale settlement");
    let predecessor_finish = store
        .finish_attempt(&first_claim)
        .await
        .expect("R3 predecessor finish");
    let successor_attempt = store
        .begin_attempt(&recovered_claim, recovery_now)
        .await
        .expect("R3 successor admission");
    let successor_finish = store
        .finish_attempt(&recovered_claim)
        .await
        .expect("R3 successor finish");
    let successor_settlement = store
        .settle(&run_id, recovered.lease.epoch, DispatchOutcome::Done, &[])
        .await
        .expect("R3 successor settlement");
    let live_rows_after_settlement = store
        .list_dispatches()
        .await
        .expect("R3 list")
        .into_iter()
        .filter(|row| row.run_id == run_id)
        .count();

    let history = DispatchRecoveryHistory {
        first_epoch: first.lease.epoch,
        recovered_epoch: recovered.lease.epoch,
        first_attempt,
        blocked_successor,
        stale_settlement,
        predecessor_finish,
        successor_attempt,
        successor_finish,
        successor_settlement,
        live_rows_after_settlement,
    };
    assert_eq!(history.first_attempt, AttemptAdmission::Applied, "R1");
    assert_eq!(history.recovered_epoch, history.first_epoch + 1, "R2/E1");
    assert_eq!(
        history.blocked_successor,
        AttemptAdmission::Blocked,
        "R2/E2"
    );
    assert_eq!(history.stale_settlement, SettleOutcome::Fenced, "R2/E3");
    assert_eq!(history.predecessor_finish, SettleOutcome::Applied, "R3/E4");
    assert_eq!(
        history.successor_attempt,
        AttemptAdmission::Applied,
        "R3/E5"
    );
    assert_eq!(history.successor_finish, SettleOutcome::Applied, "R3/E5");
    assert_eq!(
        history.successor_settlement,
        SettleOutcome::Applied,
        "R3/E5"
    );
    assert_eq!(history.live_rows_after_settlement, 0, "R3/E6");
    history
}
