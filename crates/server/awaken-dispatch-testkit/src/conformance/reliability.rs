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
    // R2b: even an exact current claim may commit logical truth, but its
    // pre-settlement effects and queue settlement are blocked by A's slot.
    let guard = store
        .lock_commit_epoch(&recovered_claim)
        .await
        .expect("R2b guard")
        .expect("R2b current");
    assert!(!guard.permits_settlement(), "R2b no physical evidence");
    guard.release().await.expect("R2b release");
    assert_eq!(
        store
            .settle(&run_id, recovered.lease.epoch, DispatchOutcome::Done, &[])
            .await
            .unwrap(),
        SettleOutcome::Fenced
    );
    let stale_settlement = store
        .settle(&run_id, first.lease.epoch, DispatchOutcome::Done, &[])
        .await
        .expect("R2 stale settlement");
    let predecessor_finish = store
        .finish_attempt(&first_claim)
        .await
        .expect("R3 predecessor finish");
    let guard = store
        .lock_commit_epoch(&recovered_claim)
        .await
        .unwrap()
        .unwrap();
    assert!(
        guard.permits_settlement(),
        "R3 precise ACK enables boundary effects"
    );
    guard.release().await.unwrap();
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

async fn occupied_recovery_does_not_starve_other_threads(
    store: &dyn DispatchQueue,
    namespace: &str,
    clock: &dyn ConformanceClock,
) {
    /* Cause/effect graph: C1 predecessor physical slot empty/occupied; C2
     * cancellation absent/requested; C3 selector local/compatible/placed;
     * C4 expired recovery competes with a fresh different-Thread Run and a
     * same-Thread successor; C5 exact predecessor ACK arrives later.
     * Decision table (C2 x C3 in every row): R1 !C1+C4 -> recovery first;
     * R2 C1+C4 -> different Thread first, same Thread still excluded;
     * R3 C1+no independent work -> recovery remains discoverable, entry blocked;
     * R4 C5 -> exact slot release, recovery settles, same Thread resumes.
     * R5 C1+expired-running -> terminal-only maintenance cannot acquire a
     * useless lease before ACK, unlike ordinary/A2A recovery. Older tests
     * covered this physical fence only for cancelled Pending rows.
     * Constraints: cancellation cannot manufacture quiescence; placement
     * cannot widen Thread exclusion; selection does not erase an old epoch.
     * All 12 partitions run on every adapter through this one contract test. */
    for selector in 0..3 {
        for cancelled in [false, true] {
            for occupied in [false, true] {
                let ns = format!("{namespace}-fair-{selector}-{cancelled}-{occupied}");
                let worker = ready_dispatch_worker(&ns, "candidate");
                async fn pick(
                    store: &dyn DispatchQueue,
                    worker: &WorkerSnapshot,
                    selector: u8,
                    now: u64,
                ) -> awaken_dispatch_contract::Claimed {
                    match selector {
                        0 => {
                            store
                                .claim("fair-local", LEASE_MS, now, &Default::default())
                                .await
                        }
                        1 => store.claim_compatible(worker, LEASE_MS, now).await,
                        _ => {
                            store
                                .claim_placed(
                                    worker,
                                    vec![worker.clone()],
                                    std::sync::Arc::new(LeastLoadedPolicy),
                                    LEASE_MS,
                                    now,
                                )
                                .await
                        }
                    }
                    .expect("select candidate")
                    .expect("eligible candidate")
                }
                clock.set(0);
                let old = dispatch(&ns, "old", "shared");
                store.enqueue(old.clone()).await.unwrap();
                let first = store
                    .claim_run(
                        old.run_id(),
                        "predecessor",
                        LEASE_MS,
                        0,
                        &Default::default(),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                let first_claim = RunClaim::from(&first.lease);
                if occupied {
                    assert_eq!(
                        store.begin_attempt(&first_claim, 0).await.unwrap(),
                        AttemptAdmission::Applied
                    );
                }
                if cancelled {
                    store.cancel(old.run_id()).await.unwrap();
                }
                clock.advance_past(first.lease.expires_ms).await;
                let now = first.lease.expires_ms.saturating_add(1);
                if occupied {
                    // The real observer failure relinquishes a replacement
                    // mutation claim, preserving the predecessor's slot.
                    let replacement = store
                        .claim_run(old.run_id(), "observer", LEASE_MS, now, &Default::default())
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(
                        store
                            .relinquish_claim(&RunClaim::from(&replacement.lease))
                            .await
                            .unwrap(),
                        SettleOutcome::Applied
                    );
                }
                if occupied {
                    assert!(
                        store
                            .claim_for_terminal_recovery(old.run_id(), "maintenance", LEASE_MS, now)
                            .await
                            .unwrap()
                            .is_none(),
                        "R5 expired-running physical fence prevents maintenance lease theft"
                    );
                }
                let peer = dispatch(&ns, "same-thread", "shared");
                let fresh = dispatch(&ns, "independent", "independent");
                store.enqueue(peer.clone()).await.unwrap();
                store.enqueue(fresh.clone()).await.unwrap();
                let selected = pick(store, &worker, selector, now).await;
                let recovered = if occupied {
                    assert_eq!(
                        selected.request.run_id(),
                        fresh.run_id(),
                        "R2 independent progress"
                    );
                    settle_done(store, &selected).await;
                    let recovered = pick(store, &worker, selector, now).await;
                    assert_eq!(
                        recovered.request.run_id(),
                        old.run_id(),
                        "R3 recovery still visible"
                    );
                    assert_eq!(
                        store
                            .begin_attempt(&RunClaim::from(&recovered.lease), now)
                            .await
                            .unwrap(),
                        AttemptAdmission::Blocked,
                        "R3 original slot preserved"
                    );
                    assert_eq!(
                        store.finish_attempt(&first_claim).await.unwrap(),
                        SettleOutcome::Applied,
                        "R4 exact ACK releases old slot"
                    );
                    recovered
                } else {
                    assert_eq!(
                        selected.request.run_id(),
                        old.run_id(),
                        "R1 recovery priority preserved"
                    );
                    selected
                };
                assert!(
                    store
                        .claim_run(
                            peer.run_id(),
                            "same-thread",
                            LEASE_MS,
                            now,
                            &Default::default()
                        )
                        .await
                        .unwrap()
                        .is_none(),
                    "R2/R4 Thread remains exclusive until settlement"
                );
                settle_done(store, &recovered).await;
                for expected in if occupied {
                    vec![peer.run_id()]
                } else {
                    vec![peer.run_id(), fresh.run_id()]
                } {
                    let next = store
                        .claim_run(expected, "finish", LEASE_MS, now, &Default::default())
                        .await
                        .unwrap()
                        .unwrap();
                    settle_done(store, &next).await;
                }
            }
        }
    }
}
