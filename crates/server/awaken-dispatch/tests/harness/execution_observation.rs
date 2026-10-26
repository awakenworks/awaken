//! One physical-execution read table for Memory, SQLite, and PostgreSQL.

use awaken_agent_contract::{RunId, ThreadId};
use awaken_dispatch_contract::{
    DispatchCursor, DispatchError, DispatchExecutionReads, LeaseRenewalOutcome,
};
use awaken_dispatch_testkit::{ConformanceClock, ConformanceDispatch};
use awaken_session_application_contract::SessionRunExecutionPhase as Phase;

pub async fn assert_observations(
    store: &(impl ConformanceDispatch + DispatchExecutionReads),
    clock: &dyn ConformanceClock,
) {
    // R1 no row -> Unavailable; R2 claimed without a slot -> AdmissionPending,
    // never Admitted from the queue label. R3 live physical -> Admitted;
    // R4 closing -> RecoveryPending despite a live lease; R5 expired lease ->
    // RecoveryPending with the original slot retained. R6 foreign Thread rejects.
    // Every observation, including rejection, preserves the physical identity,
    // queue summaries, and delivery cursor. This does not qualify a slot-change
    // race, provider termination, or an exact database clock boundary.
    let run = RunId("physical-phase".into());
    let thread = ThreadId("physical-phase".into());
    assert_observation(store, &run, &thread, Ok(Phase::Unavailable)).await;
    let (claim, binding, _) =
        super::attempt_binding::bound_claim(store, "physical-phase", clock).await;
    assert_observation(store, &run, &thread, Ok(Phase::AdmissionPending)).await;
    store.begin_attempt(&claim, Some(&binding)).await.unwrap();
    assert_observation(store, &run, &thread, Ok(Phase::Admitted)).await;
    assert_observation(store, &run, &ThreadId("foreign".into()), Err(())).await;
    assert!(
        store.claim_is_current(&claim).await.unwrap(),
        "R4 live lease"
    );
    store.close_attempt(&claim).await.unwrap();
    assert!(
        store.claim_is_current(&claim).await.unwrap(),
        "R4 still live"
    );
    assert_observation(store, &run, &thread, Ok(Phase::RecoveryPending)).await;

    let (expired, binding, _) =
        super::attempt_binding::bound_claim(store, "expired-phase", clock).await;
    let expired_thread = ThreadId("expired-phase".into());
    store.begin_attempt(&expired, Some(&binding)).await.unwrap();
    assert_observation(store, &expired.run_id, &expired_thread, Ok(Phase::Admitted)).await;
    let original = store.load_physical_attempt(&expired.run_id).await.unwrap();
    assert!(original.is_some(), "R5 retains an actual occupied slot");
    assert_eq!(
        store.renew_lease(&expired, 0).await.unwrap(),
        LeaseRenewalOutcome::Current,
        "R5 expire the original claim through its own fenced authority"
    );
    // Manual adapters advance deterministically. PostgreSQL ignores set() and
    // must observe expiry using its actual authority query, with a bounded wait.
    clock.set(60_001);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while store.claim_is_current(&expired).await.unwrap() {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("R5 store authority must observe the shortened lease expiring");
    assert_observation(
        store,
        &expired.run_id,
        &expired_thread,
        Ok(Phase::RecoveryPending),
    )
    .await;
    assert_eq!(
        store.load_physical_attempt(&expired.run_id).await.unwrap(),
        original
    );
}

async fn assert_observation(
    store: &(impl ConformanceDispatch + DispatchExecutionReads),
    run: &RunId,
    thread: &ThreadId,
    expected: Result<Phase, ()>,
) {
    let physical = store.load_physical_attempt(run).await.unwrap();
    let rows = store.list_dispatches().await.unwrap();
    let events = store
        .events_after(DispatchCursor::default(), 128, None)
        .await
        .unwrap();
    let actual = store.observe_execution(run, thread).await;
    match expected {
        Ok(expected) => assert_eq!(actual.unwrap(), expected),
        Err(()) => assert!(matches!(actual, Err(DispatchError::Rejected(_))), "R6"),
    }
    assert_eq!(store.load_physical_attempt(run).await.unwrap(), physical);
    assert_eq!(store.list_dispatches().await.unwrap(), rows);
    assert_eq!(
        store
            .events_after(DispatchCursor::default(), 128, None)
            .await
            .unwrap(),
        events
    );
}
