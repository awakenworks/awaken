// @formal evidence session_execution.one_physical_attempt_per_thread
use awaken_dispatch::{
    AttemptAdmission as A, LeaseRenewalOutcome as L, RunClaim, RunDispatch, SettleOutcome as S,
};

/// One oracle for Memory, SQLite and PostgreSQL; real stores, no shadow reducer.
pub async fn assert_retirement(
    store: &impl ConformanceDispatch,
    now: u64,
    clock: &dyn awaken_dispatch_testkit::ConformanceClock,
) {
    // Causes: C1 current/expired claim; C2 empty/live/retired slot;
    // C3 ordinary begin/durable handoff; C4 exact/delayed predecessor ACK.
    // R1 live exact retry => AlreadyApplied, not a fresh execution.
    // R2 finish then same-epoch begin/handoff => Fenced (prevents ACK ABA).
    // R3 duplicate ACK => Applied with no new execution.
    // R4 new claim epoch => may execute; old ACK => Fenced, slot preserved.
    // R5 exact B renewal => Current; after its zero-duration lease expires, a
    // new epoch remains Blocked while B's physical attempt is still active.
    // R6 another Thread => independent admission. No rule grants replay of an
    // unknown external effect; these tests own only physical identity retirement.
    store
        .enqueue(RunDispatch::new(super::activation("retirement")))
        .await
        .unwrap();
    clock.set(now);
    let a = store
        .claim("worker-a", 60_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let a = RunClaim::from(&a.lease);
    clock.set(now);
    assert_eq!(store.begin_attempt(&a, None).await.unwrap(), A::Applied);
    clock.set(now);
    assert_eq!(
        store.begin_attempt(&a, None).await.unwrap(),
        A::AlreadyApplied,
        "R1"
    );
    assert_eq!(store.finish_attempt(&a, None).await.unwrap(), S::Applied);
    clock.set(now);
    assert_eq!(
        store.begin_attempt(&a, None).await.unwrap(),
        A::Fenced,
        "R2 begin"
    );
    clock.set(now);
    assert_eq!(
        store.handoff_attempt(&a, None).await.unwrap(),
        A::Fenced,
        "R2 handoff"
    );
    assert_eq!(
        store.finish_attempt(&a, None).await.unwrap(),
        S::Applied,
        "R3"
    );
    store.relinquish_claim(&a).await.unwrap();
    clock.set(now);
    let b = store
        .claim("worker-b", 60_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let b = RunClaim::from(&b.lease);
    assert!(b.epoch > a.epoch);
    clock.set(now);
    assert_eq!(
        store.begin_attempt(&b, None).await.unwrap(),
        A::Applied,
        "R4"
    );
    assert_eq!(
        store.finish_attempt(&a, None).await.unwrap(),
        S::Fenced,
        "R4 delayed ACK"
    );
    clock.set(now);
    assert_eq!(
        store.begin_attempt(&b, None).await.unwrap(),
        A::AlreadyApplied,
        "R4 preserved"
    );
    store
        .enqueue(RunDispatch::new(super::activation_on(
            "independent",
            "another-thread",
        )))
        .await
        .unwrap();
    clock.set(now);
    let c = store
        .claim("worker-c", 60_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let c = RunClaim::from(&c.lease);
    clock.set(now);
    assert_eq!(
        store.begin_attempt(&c, None).await.unwrap(),
        A::Applied,
        "R6"
    );
    assert_eq!(store.finish_attempt(&c, None).await.unwrap(), S::Applied);
    clock.set(now);
    assert_eq!(
        store.renew_lease(&b, 0).await.unwrap(),
        L::Current,
        "R5 exact current claim receives the typed renewal outcome"
    );
    // Expire through the store-owned lease clock; expiry must retain B's slot.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    clock.set(now + 1);
    let d = store
        .claim("worker-d", 60_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let d = RunClaim::from(&d.lease);
    assert_eq!(d.run_id, b.run_id);
    clock.set(now + 1);
    assert_eq!(
        store.begin_attempt(&d, None).await.unwrap(),
        A::Blocked,
        "R5"
    );
    assert_eq!(store.finish_attempt(&b, None).await.unwrap(), S::Applied);
    clock.set(now);
    assert_eq!(
        store.begin_attempt(&b, None).await.unwrap(),
        A::Fenced,
        "R2 successor"
    );
    clock.set(now + 1);
    assert_eq!(
        store.begin_attempt(&d, None).await.unwrap(),
        A::Applied,
        "R5 after ACK"
    );
    assert_eq!(
        store.finish_attempt(&b, None).await.unwrap(),
        S::Fenced,
        "R4 third generation"
    );
    assert_eq!(store.finish_attempt(&d, None).await.unwrap(), S::Applied);
}

use awaken_dispatch_testkit::ConformanceDispatch;

/// GC is not an execution-close acknowledgement. One matrix runs against
/// Memory, SQLite and PostgreSQL through their existing TTL test cases.
pub async fn assert_purge_retains_occupied(
    store: &impl ConformanceDispatch,
    clock: &dyn awaken_dispatch_testkit::ConformanceClock,
) {
    // Causes: C1 a cancelled, expired row is dead-lettered; C2 its original
    // physical slot is bound/unbound; C3 its entry is open/closing; C4 the
    // cutoff is before/at its recorded dead-letter time; C5 an exact independent
    // closure ACK has retired that slot. Effects: E1 ordinary empty rows obey
    // the original TTL boundary; E2 either GC entry point retains every occupied
    // row and the exact binding/Worker/epoch/physical reference; E3 only the
    // original close+finish transition makes it eligible for GC.
    //
    // | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
    // | G1 | yes | empty | open | before | no | E1 retain younger empty |
    // | G2 | yes | empty | open | at | no | E1 remove only empty in mixed set |
    // | G3 | yes | bound/unbound | open/closing | any | no | E2 retain exact slot |
    // | G4 | yes | bound/unbound | closing | at | yes | E3 GC after exact ACK |
    // These are storage-retention assertions, not fabricated provider stop,
    // request-join, resource disposal, or whole-Session closure evidence.
    let mut retained = Vec::new();
    for (name, bound, closing) in [
        ("purge-bound", true, false),
        ("purge-bound-closing", true, true),
        ("purge-unbound", false, false),
        ("purge-unbound-closing", false, true),
    ] {
        let (claim, binding, worker) = if bound {
            let (claim, binding, worker) =
                super::attempt_binding::bound_claim(store, name, clock).await;
            (claim, Some(binding), Some(worker))
        } else {
            store
                .enqueue(RunDispatch::new(super::activation_on(name, name)))
                .await
                .unwrap();
            clock.set(0);
            let leased = store
                .claim_run(
                    &awaken_agent_contract::agent::run::Id(name.into()),
                    name,
                    60_000,
                    &Default::default(),
                )
                .await
                .unwrap()
                .unwrap();
            (RunClaim::from(&leased.lease), None, None)
        };
        assert_eq!(
            store.begin_attempt(&claim, binding.as_ref()).await.unwrap(),
            A::Applied
        );
        if closing {
            assert_eq!(store.close_attempt(&claim).await.unwrap(), S::Applied);
        }
        store.cancel(&claim.run_id).await.unwrap();
        clock.set(0);
        let successor = if let Some(worker) = worker {
            store
                .claim_run_compatible(&claim.run_id, &worker, 1)
                .await
                .unwrap()
                .unwrap()
        } else {
            store
                .claim_run(&claim.run_id, "purge-successor", 1, &Default::default())
                .await
                .unwrap()
                .unwrap()
        };
        clock.advance_past(successor.lease.expires_ms()).await;
        assert_eq!(
            store
                .quarantine_retry_exhausted(0, successor.lease.expires_ms().saturating_add(1))
                .await
                .unwrap(),
            1
        );
        let original = store
            .load_physical_attempt(&claim.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(original.claim, claim);
        assert_eq!(
            original.binding, binding,
            "G3 original UID/provider/Worker reference"
        );
        assert_eq!(original.closing, closing);
        retained.push((claim, original));
    }

    // Mix one collectible row with all four occupied shapes. The cutoff is
    // taken from the actual lease and the existing dead-letter command, so the
    // live PostgreSQL clock never borrows a test-only admission rule.
    let (_, dead_lettered_at) = dead_letter_empty(store, "purge-empty-mixed", clock).await;
    assert_eq!(
        store
            .purge_dead_letters_before(dead_lettered_at - 1)
            .await
            .unwrap(),
        0,
        "G1"
    );
    assert_eq!(
        store
            .purge_dead_letters_before(dead_lettered_at)
            .await
            .unwrap(),
        1,
        "G2 mixed set"
    );
    assert_eq!(
        store.purge_dead_letters().await.unwrap(),
        0,
        "G3 unlimited GC"
    );
    assert_eq!(
        store.purge_dead_letters_before(u64::MAX).await.unwrap(),
        0,
        "G3 expired GC"
    );
    for (claim, original) in &retained {
        assert_eq!(
            store
                .load_physical_attempt(&claim.run_id)
                .await
                .unwrap()
                .as_ref(),
            Some(original),
            "G3 exact original survives both entry points"
        );
        assert!(
            awaken_dispatch::has_active_physical_attempt_for_owner(store, &claim.owner)
                .await
                .unwrap(),
            "G3 GC cannot manufacture false occupancy"
        );
    }
    for (claim, _) in &retained {
        assert_eq!(store.close_attempt(claim).await.unwrap(), S::Applied);
        assert_eq!(
            store
                .finish_attempt_closure(
                    &awaken_dispatch::AttemptClosureRecord::try_new(
                        store
                            .load_physical_attempt(&claim.run_id)
                            .await
                            .unwrap()
                            .unwrap(),
                        awaken_dispatch::AttemptClosureKind::ControlledExecutionClosed,
                        "storage-retention-exact-closure-ack",
                    )
                    .unwrap()
                )
                .await
                .unwrap(),
            S::Applied,
            "G4 exact original closure ACK"
        );
        assert!(
            store
                .load_physical_attempt(&claim.run_id)
                .await
                .unwrap()
                .is_none()
        );
    }
    for (claim, _) in &retained {
        assert!(
            !awaken_dispatch::has_active_physical_attempt_for_owner(store, &claim.owner)
                .await
                .unwrap(),
            "G4 exact retired facts are truly absent; reads do not re-admit or mutate"
        );
    }
    assert_eq!(
        store.purge_dead_letters_before(u64::MAX).await.unwrap(),
        retained.len(),
        "G4 retired rows are now collectible"
    );
    assert!(store.dead_letters().await.unwrap().is_empty());
}

/// Create one genuinely empty dead letter through the existing Store commands.
/// Used by the mixed GC rule and raw-storage corruption injections; it neither
/// bypasses quarantine nor supplies any physical closure fact.
pub async fn dead_letter_empty(
    store: &impl ConformanceDispatch,
    name: &str,
    clock: &dyn awaken_dispatch_testkit::ConformanceClock,
) -> (awaken_agent_contract::agent::run::Id, u64) {
    let run = awaken_agent_contract::agent::run::Id(name.into());
    store
        .enqueue(RunDispatch::new(super::activation_on(name, name)))
        .await
        .unwrap();
    clock.set(0);
    let leased = store
        .claim_run(&run, "purge-empty", 1, &Default::default())
        .await
        .unwrap()
        .unwrap();
    clock.advance_past(leased.lease.expires_ms()).await;
    let dead_lettered_at = leased.lease.expires_ms().saturating_add(1);
    assert_eq!(
        store
            .quarantine_retry_exhausted(0, dead_lettered_at)
            .await
            .unwrap(),
        1
    );
    (run, dead_lettered_at)
}
