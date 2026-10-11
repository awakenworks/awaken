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
    assert_eq!(store.finish_attempt(&a).await.unwrap(), S::Applied);
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
    assert_eq!(store.finish_attempt(&a).await.unwrap(), S::Applied, "R3");
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
        store.finish_attempt(&a).await.unwrap(),
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
    assert_eq!(store.finish_attempt(&c).await.unwrap(), S::Applied);
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
    assert_eq!(store.finish_attempt(&b).await.unwrap(), S::Applied);
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
        store.finish_attempt(&b).await.unwrap(),
        S::Fenced,
        "R4 third generation"
    );
    assert_eq!(store.finish_attempt(&d).await.unwrap(), S::Applied);
}

use awaken_dispatch_testkit::ConformanceDispatch;
