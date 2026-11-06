// @formal evidence ingress.epoch_reclaim
// @formal evidence ingress.pending_input_exactly_once
// @formal evidence ingress.settle_fence
// @formal evidence remote_worker.single_live_claim_owner
// @formal evidence run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
// @formal surface exclude=test_support_crate
// @formal owner crates/platform/dispatch/awaken-dispatch/tests/dispatch_conformance.rs
// @formal reason: Static owner: normalized crash-recovery history for adapter differential tests. Dynamic
// @formal reason: path: the tooling-only testkit drives production DispatchStore implementations and
// @formal reason: records observations; it neither transitions nor persists product state itself.
// @formal reason: Evidence:
// @formal reason: run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing.
/// Settlement-fence and pending-input cause/effect table shared by every store.
/// C1 an Awaiting Run has one bound input; C2 claimant A disappears; C3 B reclaims
/// at a newer epoch and another input arrives; C4 stale A settles Awaiting; C5
/// current B settles Awaiting naming only the first id; C6 C wakes on the retained
/// input; C7 stale B/current C settle Done; C8 unbound input is read and a settle
/// omits/names its id. E1 C2/C3 replay the exact first input; E2 C4 leaves dispatch,
/// pending, completion and wake state unchanged; E3 C5 clears B's lease, consumes
/// only the named id, publishes no completion and makes C6 wakeable; E4 stale C7
/// leaves every projection unchanged; E5 current C7 removes the row and remaining
/// run-bound input, publishes one tombstone and prevents another wake; E6 C8 keeps
/// omitted ids and consumes named ids. Constraints: the current epoch and empty
/// physical slot are the sole mutation authority; all four observable projections
/// come from the existing store ports. Rules: SF1=C1+C2+C3 -> E1;
/// SF2=SF1+C4 -> E2; SF3=SF1+C5+C6 -> E3; SF4=C6+C7(stale/current)
/// -> E4+E5; PI1=C8(omit/name) -> E6.
async fn settlement_fence_and_pending_input_are_atomic<S: ConformanceDispatch + Inbox + Outbox>(
    store: &S,
    ns: &str,
    capabilities: ConformanceCapabilities,
    clock: &dyn ConformanceClock,
) {
    let completion_cursor = if capabilities.completion_events {
        store
            .completion_events_after(0, usize::MAX)
            .await
            .expect("SF1 completion baseline")
            .last()
            .map_or(0, |event| event.sequence)
    } else {
        0
    };
    let request = dispatch(ns, "pending-replay", "pending-replay-thread");
    let run_id = request.run_id().clone();
    let bound_thread = request.thread_id().clone();
    clock.set(300_000);
    store.enqueue(request).await.expect("SF1 enqueue");
    let initial = store
        .claim_run(&run_id, "pending-initial", LEASE_MS, &Default::default())
        .await
        .expect("SF1 initial claim")
        .expect("SF1 initial Run is claimable");
    assert_eq!(
        store
            .settle(
                &run_id,
                initial.lease.epoch(),
                DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("SF1 establish Awaiting"),
        SettleOutcome::Applied,
        "SF1 setup"
    );
    let input = PendingInput {
        message_id: format!("{ns}-pending-replay-input"),
        run_id: run_id.clone(),
        thread_id: bound_thread.clone(),
        correlation_id: format!("{ns}-pending-replay-ticket"),
        available_at_ms: None,
        context_messages: Vec::new(),
        result: ResumeResult::Input("replay after crash".to_string()),
    };
    assert!(store.append(input.clone()).await.expect("SF1 append"));
    clock.set(300_001);
    let crashed = store
        .claim_run(&run_id, "pending-crashed", LEASE_MS, &Default::default())
        .await
        .expect("SF1 crash-window claim")
        .expect("SF1 pending input wakes Awaiting Run");
    assert_eq!(crashed.pending, vec![input.clone()], "SF1/E1 exact handoff");

    clock.advance_past(crashed.lease.expires_ms()).await;
    clock.set(crashed.lease.expires_ms().saturating_add(1));
    let recovered = store
        .claim_run(&run_id, "pending-recovered", LEASE_MS, &Default::default())
        .await
        .expect("SF1 recovery claim")
        .expect("SF1 expired claim is recoverable");
    assert_eq!(recovered.lease.epoch(), crashed.lease.epoch() + 1, "SF1/E1");
    assert_eq!(recovered.pending, vec![input.clone()], "SF1/E1");
    let arrived_during_attempt = PendingInput {
        message_id: format!("{ns}-pending-mid-attempt-input"),
        run_id: run_id.clone(),
        thread_id: bound_thread.clone(),
        correlation_id: format!("{ns}-pending-next-ticket"),
        available_at_ms: None,
        context_messages: Vec::new(),
        result: ResumeResult::Input("retain for the next wake".to_string()),
    };
    assert!(
        store
            .append(arrived_during_attempt.clone())
            .await
            .expect("SF1 append mid-attempt input")
    );
    let before_stale_awaiting = store
        .list_dispatches()
        .await
        .expect("SF2 dispatch snapshot");
    let pending_before_stale_awaiting = store
        .list(&bound_thread)
        .await
        .expect("SF2 pending snapshot");
    let completions_before_stale_awaiting = if capabilities.completion_events {
        store
            .completion_events_after(completion_cursor, usize::MAX)
            .await
            .expect("SF2 completion snapshot")
    } else {
        Vec::new()
    };
    assert_eq!(
        store
            .settle(
                &run_id,
                crashed.lease.epoch(),
                DispatchOutcome::Awaiting,
                &[
                    input.message_id.clone(),
                    arrived_during_attempt.message_id.clone()
                ],
            )
            .await
            .expect("SF2 stale Awaiting settlement"),
        SettleOutcome::Fenced,
        "SF2/E2"
    );
    assert_eq!(
        store
            .list_dispatches()
            .await
            .expect("SF2 dispatch after stale Awaiting"),
        before_stale_awaiting,
        "SF2/E2 stale Awaiting preserves the complete dispatch projection"
    );
    assert_eq!(
        store
            .list(&bound_thread)
            .await
            .expect("SF2 pending after stale Awaiting"),
        pending_before_stale_awaiting,
        "SF2/E2 stale Awaiting consumes no pending input"
    );
    if capabilities.completion_events {
        assert_eq!(
            store
                .completion_events_after(completion_cursor, usize::MAX)
                .await
                .expect("SF2 completion after stale Awaiting"),
            completions_before_stale_awaiting,
            "SF2/E2 stale Awaiting publishes no completion"
        );
    }
    assert_eq!(
        store
            .settle(
                &run_id,
                recovered.lease.epoch(),
                DispatchOutcome::Awaiting,
                std::slice::from_ref(&input.message_id),
            )
            .await
            .expect("SF3 current Awaiting settlement"),
        SettleOutcome::Applied,
        "SF3/E3"
    );
    let after_current_awaiting = store
        .list_dispatches()
        .await
        .expect("SF3 dispatch after current Awaiting");
    assert_eq!(
        after_current_awaiting
            .iter()
            .find(|summary| summary.run_id == run_id)
            .expect("SF3 retained dispatch")
            .state,
        DispatchState::Awaiting,
        "SF3/E3 current Awaiting publishes the exact state"
    );
    let after_current_pending = store
        .list(&bound_thread)
        .await
        .expect("SF3 pending after current Awaiting");
    assert_eq!(after_current_pending.len(), 1, "SF3/E3 one input remains");
    assert_eq!(
        after_current_pending[0].input, arrived_during_attempt,
        "SF3/E3 only the named input is consumed"
    );
    if capabilities.completion_events {
        assert!(
            store
                .completion_events_after(completion_cursor, usize::MAX)
                .await
                .expect("SF3 completion after current Awaiting")
                .is_empty(),
            "SF3/E3 Awaiting publishes no completion"
        );
    }
    clock.set(recovered.lease.expires_ms().saturating_add(1));
    let next = store
        .claim_run(&run_id, "pending-next", LEASE_MS, &Default::default())
        .await
        .expect("SF3 next wake")
        .expect("SF3 retained input wakes the Awaiting row");
    assert_eq!(
        next.pending,
        vec![arrived_during_attempt.clone()],
        "SF3/E3 wake hands only the retained input"
    );
    let before_stale_done = store
        .list_dispatches()
        .await
        .expect("SF4 dispatch snapshot");
    let pending_before_stale_done = store
        .list(&bound_thread)
        .await
        .expect("SF4 pending snapshot");
    let completions_before_stale_done = if capabilities.completion_events {
        store
            .completion_events_after(completion_cursor, usize::MAX)
            .await
            .expect("SF4 completion snapshot")
    } else {
        Vec::new()
    };
    assert_eq!(
        store
            .settle(
                &run_id,
                recovered.lease.epoch(),
                DispatchOutcome::Done,
                std::slice::from_ref(&arrived_during_attempt.message_id),
            )
            .await
            .expect("SF4 stale Done settlement"),
        SettleOutcome::Fenced,
        "SF4/E4"
    );
    assert_eq!(
        store
            .list_dispatches()
            .await
            .expect("SF4 dispatch after stale Done"),
        before_stale_done,
        "SF4/E4 stale Done preserves the complete dispatch projection"
    );
    assert_eq!(
        store
            .list(&bound_thread)
            .await
            .expect("SF4 pending after stale Done"),
        pending_before_stale_done,
        "SF4/E4 stale Done consumes no pending input"
    );
    if capabilities.completion_events {
        assert_eq!(
            store
                .completion_events_after(completion_cursor, usize::MAX)
                .await
                .expect("SF4 completion after stale Done"),
            completions_before_stale_done,
            "SF4/E4 stale Done publishes no completion"
        );
    }
    assert_eq!(
        store
            .settle(&run_id, next.lease.epoch(), DispatchOutcome::Done, &[])
            .await
            .expect("SF4 current Done settlement"),
        SettleOutcome::Applied,
        "SF4/E5"
    );
    assert!(
        store
            .list_dispatches()
            .await
            .expect("SF4 dispatch after current Done")
            .iter()
            .all(|summary| summary.run_id != run_id),
        "SF4/E5 current Done removes the dispatch"
    );
    assert!(
        store
            .list(&bound_thread)
            .await
            .expect("SF4 pending after current Done")
            .is_empty(),
        "SF4/E5 current Done removes remaining run-bound input"
    );
    if capabilities.completion_events {
        let completions = store
            .completion_events_after(completion_cursor, usize::MAX)
            .await
            .expect("SF4 completion after current Done");
        assert_eq!(completions.len(), 1, "SF4/E5 one completion tombstone");
        assert_eq!(completions[0].run_id, run_id, "SF4/E5 exact completion");
    }
    clock.set(next.lease.expires_ms().saturating_add(1));
    assert!(
        store
            .claim_run(&run_id, "pending-after-done", LEASE_MS, &Default::default(),)
            .await
            .expect("SF4 wake after current Done")
            .is_none(),
        "SF4/E5 current Done prevents another wake"
    );

    let unbound_thread = thread_id(ns, "pending-unbound-thread");
    let unbound = PendingInput {
        message_id: format!("{ns}-pending-unbound-input"),
        run_id: RunId(String::new()),
        thread_id: unbound_thread.clone(),
        correlation_id: String::new(),
        available_at_ms: None,
        context_messages: Vec::new(),
        result: ResumeResult::Input("unbound exact consumption".to_string()),
    };
    assert!(store.append(unbound.clone()).await.expect("PI1 append"));
    assert_eq!(
        store.list(&unbound_thread).await.expect("PI1 first read"),
        store
            .list(&unbound_thread)
            .await
            .expect("PI1 replayed read"),
        "PI1/E6 repeated reads do not consume"
    );
    let omitted = dispatch(ns, "pending-unbound-omit", "pending-unbound-thread");
    store
        .enqueue(omitted.clone())
        .await
        .expect("PI1 enqueue omit");
    clock.set(400_000);
    let omitted_claim = store
        .claim_run(
            omitted.run_id(),
            "pending-unbound-omit",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("PI1 claim omit")
        .expect("PI1 omitted Run is claimable");
    assert_eq!(
        store
            .settle(
                omitted.run_id(),
                omitted_claim.lease.epoch(),
                DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("PI1 omit id"),
        SettleOutcome::Applied
    );
    let after_omit = store
        .list(&unbound_thread)
        .await
        .expect("PI1 omitted id survives");
    assert_eq!(after_omit.len(), 1, "PI1/E6 one omitted row remains");
    assert_eq!(after_omit[0].input, unbound, "PI1/E6 omitted id is exact");
    let named = dispatch(ns, "pending-unbound-name", "pending-unbound-thread");
    store
        .enqueue(named.clone())
        .await
        .expect("PI1 enqueue named");
    clock.set(400_001);
    let named_claim = store
        .claim_run(
            named.run_id(),
            "pending-unbound-name",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("PI1 claim named")
        .expect("PI1 named Run is claimable");
    assert_eq!(
        store
            .settle(
                named.run_id(),
                named_claim.lease.epoch(),
                DispatchOutcome::Done,
                &[format!("{ns}-pending-unbound-input")],
            )
            .await
            .expect("PI1 consume named id"),
        SettleOutcome::Applied
    );
    assert!(
        store
            .list(&unbound_thread)
            .await
            .expect("PI1 named id absent")
            .is_empty(),
        "PI1/E6 a named id is consumed exactly once"
    );
}

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
    store: &dyn ConformanceDispatch,
    namespace: &str,
    clock: &dyn ConformanceClock,
) -> DispatchRecoveryHistory {
    /* Cause/effect graph: C1 a fresh Run is claimed by predecessor A; C2 A
     * enters its physical attempt; C3 A's lease expires and B reclaims; C4 A
     * has not quiesced; C5 A later quiesces. Effects: E1 B receives a strictly
     * newer epoch; E2 B is blocked while C4; E3 A's stale settlement is fenced;
     * E4 C5 clears only A's physical slot; E5 B then enters and settles once;
     * E6 no live dispatch remains. Decision trace R1=C1+C2; R2=C3+C4->E1-E3;
     * R3=C5->E4-E6. Retry dimension C6 duplicates begin/finish after a lost
     * response. R4=C2+C6 -> AlreadyApplied, without a second execution slot;
     * R5=C5+C6 before B enters -> Applied (empty-slot ACK is idempotent);
     * R6=C5+C6 after B enters -> Fenced, preserving B's slot. C7 is a slow
     * live backend: B's lease must cover the deliberate R2-R6 trace, or an
     * unrelated expiry would turn R6 into a different rule. The recovery lease
     * is long only for this shared test history; the first lease remains short
     * so the real PostgreSQL clock still proves expiration and reclaim. */
    let request = dispatch(namespace, "history", "history-thread");
    let run_id = request.run_id().clone();
    clock.set(0);
    store.enqueue(request).await.expect("R1 enqueue");
    let first = store
        .claim_run(&run_id, "history-a", LEASE_MS, &Default::default())
        .await
        .expect("R1 claim")
        .expect("R1 claimable");
    let first_claim = RunClaim::from(&first.lease);
    let first_attempt = store
        .begin_attempt(&first_claim, None)
        .await
        .expect("R1 begin attempt");
    assert_eq!(
        store.begin_attempt(&first_claim, None).await.unwrap(),
        AttemptAdmission::AlreadyApplied,
        "R4 lost begin response retries admission, not execution"
    );
    let recovery_now = first.lease.expires_ms().saturating_add(1);
    // Deterministic adapters advance immediately; a live PostgreSQL adapter
    // waits for its authoritative database/wall clock to cross the persisted
    // deadline. Keeping that distinction behind ConformanceClock lets every
    // backend execute this one history without giving PostgreSQL a test-only
    // clock or duplicating the recovery oracle.
    clock.advance_past(first.lease.expires_ms()).await;
    clock.set(recovery_now);
    let recovered = store
        .claim_run(&run_id, "history-b", LEASE_MS * 60, &Default::default())
        .await
        .expect("R2 recover")
        .expect("R2 expired claim recovers");
    let recovered_claim = RunClaim::from(&recovered.lease);
    clock.set(recovery_now);
    let blocked_successor = store
        .begin_attempt(&recovered_claim, None)
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
            .settle(&run_id, recovered.lease.epoch(), DispatchOutcome::Done, &[])
            .await
            .unwrap(),
        SettleOutcome::Fenced
    );
    let stale_settlement = store
        .settle(&run_id, first.lease.epoch(), DispatchOutcome::Done, &[])
        .await
        .expect("R2 stale settlement");
    let predecessor_finish = store
        .finish_attempt(&first_claim, None)
        .await
        .expect("R3 predecessor finish");
    assert_eq!(
        store.finish_attempt(&first_claim, None).await.unwrap(),
        SettleOutcome::Applied,
        "R5 lost finish response is an idempotent ACK"
    );
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
    clock.set(recovery_now);
    let successor_attempt = store
        .begin_attempt(&recovered_claim, None)
        .await
        .expect("R3 successor admission");
    assert_eq!(
        store.finish_attempt(&first_claim, None).await.unwrap(),
        SettleOutcome::Fenced,
        "R6 a delayed old ACK cannot clear the successor slot"
    );
    clock.set(recovery_now);
    assert_eq!(
        store.begin_attempt(&recovered_claim, None).await.unwrap(),
        AttemptAdmission::AlreadyApplied,
        "R6 the successor slot survives the delayed old ACK"
    );
    let successor_finish = store
        .finish_attempt(&recovered_claim, None)
        .await
        .expect("R3 successor finish");
    let successor_settlement = store
        .settle(&run_id, recovered.lease.epoch(), DispatchOutcome::Done, &[])
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
        first_epoch: first.lease.epoch(),
        recovered_epoch: recovered.lease.epoch(),
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
    store: &dyn ConformanceDispatch,
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
                    store: &dyn ConformanceDispatch,
                    worker: &WorkerSnapshot,
                    selector: u8,
                    now: u64,
                    clock: &dyn ConformanceClock,
                ) -> awaken_dispatch_contract::Claimed {
                    clock.set(now);
                    match selector {
                        0 => {
                            store
                                .claim("fair-local", LEASE_MS, &Default::default())
                                .await
                        }
                        1 => store.claim_compatible(worker, LEASE_MS).await,
                        _ => {
                            store
                                .claim_placed(
                                    worker,
                                    vec![worker.clone()],
                                    std::sync::Arc::new(LeastLoadedPolicy),
                                    LEASE_MS,
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
                    .claim_run(old.run_id(), "predecessor", LEASE_MS, &Default::default())
                    .await
                    .unwrap()
                    .unwrap();
                let first_claim = RunClaim::from(&first.lease);
                if occupied {
                    clock.set(0);
                    assert_eq!(
                        store.begin_attempt(&first_claim, None).await.unwrap(),
                        AttemptAdmission::Applied
                    );
                }
                if cancelled {
                    store.cancel(old.run_id()).await.unwrap();
                }
                clock.advance_past(first.lease.expires_ms()).await;
                let now = first.lease.expires_ms().saturating_add(1);
                if occupied {
                    // The real observer failure relinquishes a replacement
                    // mutation claim, preserving the predecessor's slot.
                    clock.set(now);
                    let replacement = store
                        .claim_run(old.run_id(), "observer", LEASE_MS, &Default::default())
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
                    clock.set(now);
                    assert!(
                        store
                            .claim_for_terminal_recovery(old.run_id(), "maintenance", LEASE_MS)
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
                let selected = pick(store, &worker, selector, now, clock).await;
                let recovered = if occupied {
                    assert_eq!(
                        selected.request.run_id(),
                        fresh.run_id(),
                        "R2 independent progress"
                    );
                    settle_done(store, &selected).await;
                    let recovered = pick(store, &worker, selector, now, clock).await;
                    assert_eq!(
                        recovered.request.run_id(),
                        old.run_id(),
                        "R3 recovery still visible"
                    );
                    clock.set(now);
                    assert_eq!(
                        store
                            .begin_attempt(&RunClaim::from(&recovered.lease), None)
                            .await
                            .unwrap(),
                        AttemptAdmission::Blocked,
                        "R3 original slot preserved"
                    );
                    assert_eq!(
                        store.finish_attempt(&first_claim, None).await.unwrap(),
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
                clock.set(now);
                assert!(
                    store
                        .claim_run(peer.run_id(), "same-thread", LEASE_MS, &Default::default())
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
                    clock.set(now);
                    let next = store
                        .claim_run(expected, "finish", LEASE_MS, &Default::default())
                        .await
                        .unwrap()
                        .unwrap();
                    settle_done(store, &next).await;
                }
            }
        }
    }
}
