use super::*;
use awaken_dispatch_contract::{
    DispatchSettlementError, DispatchSettlementObserver, SessionChildAdmission,
};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct PendingOnce(AtomicUsize);

#[async_trait::async_trait]
impl DispatchSettlementObserver for PendingOnce {
    async fn before_settle(
        &self,
        _dispatch: &crate::RunDispatch,
        _claim: &RunClaim,
        _committed_state: &RunState,
        _cancellation_requested: bool,
    ) -> Result<(), DispatchSettlementError> {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(DispatchSettlementError(
                "committed lifecycle boundary is not yet visible".into(),
            ))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn terminal_observation_pending_defers_only_the_exact_claim() {
    // Causes: C1 an Awaiting dispatch has an exact committed terminal and
    // scanner claim A; C2 the Session observer is pending once, then ready; C3 the retry
    // deadline is before/after expiry; C4 A tries to settle after B recovers.
    // Effects: E1 pending writes no Done receipt and releases only A's claim;
    // E2 neither scanner nor B can claim before the deadline; E3 B gets a higher epoch and
    // settles the same terminal exactly once; E4 A is fenced. Rules:
    // P1=C1+pending=>E1+E2; P2=C1+ready+expired=>E3; P3=C4=>E4.
    // The observer returns before any physical effect; other error classes
    // retain the existing claim-fenced failure behavior.
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let run_id = RunId("terminal-pending-run".into());
    let thread_id = ThreadId("terminal-pending-thread".into());
    let terminal = RunState::Ended(EndCause::NaturalEnd);
    let activation = RunActivation::new(
        run_id.clone(),
        thread_id.clone(),
        ExecutableAgentSnapshot::builder("terminal-pending-snapshot")
            .model(
                awaken_runtime_contract::resolved::ModelBinding::parse("test", "test", "native")
                    .expect("P1 complete model binding"),
            )
            .build(),
        Vec::new(),
    );
    store
        .enqueue_session_child(
            crate::RunDispatch::new(activation)
                .for_session(ThreadId("terminal-pending-session".into()))
                .with_session_activity_epoch(1),
            SessionChildAdmission::new(25, Vec::new()),
        )
        .await
        .expect("P1 admit the exact Session child");
    let parked = store
        .claim("worker-a", 1_000, &Default::default())
        .await
        .expect("P1 initial claim")
        .expect("P1 runnable dispatch");
    assert_eq!(
        store
            .settle(&run_id, parked.lease.epoch, DispatchOutcome::Awaiting, &[])
            .await
            .expect("P1 park for terminal recovery"),
        crate::SettleOutcome::Applied,
    );
    let first = store
        .claim_for_terminal_recovery(&run_id, "scanner-a", 1_000)
        .await
        .expect("P1 terminal recovery claim")
        .expect("P1 scanner claims Awaiting dispatch");
    let first_epoch = first.lease.epoch;
    let reader: Arc<dyn CommittedThreadView> = Arc::new(TerminalView(RunRecord {
        id: run_id.clone(),
        thread_id,
        state: terminal.clone(),
    }));
    let observer = Arc::new(PendingOnce::default());
    let worker = |owner| {
        DispatchWorker::from_parts(
            Arc::new(Runtime::new()),
            store.clone(),
            Arc::new(MemoryCommitCoordinator::new(
                awaken_lease_contract::system_clock(),
            )),
            reader.clone(),
            owner,
        )
        .with_settlement_observer(observer.clone())
    };

    let pending = worker("scanner-a")
        .settle_claimed_terminal(first)
        .await
        .expect_err("P1 pending observation cannot publish Done");
    assert!(pending.is_settlement_observation_pending(), "P1/E1");
    assert_eq!(
        store
            .dispatch_summary(&run_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        crate::DispatchState::Pending,
        "P1/E1 exact claim released"
    );
    assert!(
        store
            .completion_events_after(0, 10)
            .await
            .unwrap()
            .is_empty(),
        "P1/E1 pending observation cannot publish Done"
    );
    assert!(
        store
            .claim_for_terminal_recovery(&run_id, "scanner-a", 1_000)
            .await
            .unwrap()
            .is_none(),
        "P1/E2 terminal scanner observes retry deadline"
    );
    assert!(
        store
            .claim("worker-b", 1_000, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "P1/E2 ordinary Worker observes retry deadline"
    );

    let retry_after_ms = crate::pool::claim_failure_backoff(first_epoch).as_millis() as u64;
    clock.advance(retry_after_ms + 1);
    let second = store
        .claim("worker-b", 1_000, &Default::default())
        .await
        .expect("P2 claim B")
        .expect("P2 retry became eligible");
    assert!(second.lease.epoch > first_epoch, "P2/E3 higher epoch");
    assert_eq!(
        worker("worker-b")
            .settle_claimed_terminal(second)
            .await
            .expect("P2 exact terminal settlement"),
        Some((run_id.clone(), terminal)),
        "P2/E3 one committed terminal"
    );
    assert_eq!(
        observer.0.load(Ordering::SeqCst),
        2,
        "P2/E3 observer retried once"
    );
    assert_eq!(
        store.completion_events_after(0, 10).await.unwrap().len(),
        1,
        "P2/E3 one completion tombstone"
    );
    assert_eq!(
        store
            .settle(&run_id, first_epoch, DispatchOutcome::Done, &[])
            .await
            .unwrap(),
        crate::SettleOutcome::Fenced,
        "P3/E4 old owner fenced"
    );
}
