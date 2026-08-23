//! Two cross-cutting worker/daemon guarantees:
//!
//! - a transient dispatch-store error inside the daemon's drain loop is SWALLOWED
//!   (logged and retried on the next tick), never killing the daemon; and
//! - the worker meters the dispatch lifecycle — one `claim`, one `drive.duration`,
//!   and exactly one `settled{outcome}` — on every drive exit path (a terminal Done,
//!   a Awaiting re-await, and an early terminal-recovery return).

mod harness;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_run_ingress::{
    DispatchQueue, DispatchService, DispatchServiceConfig, DispatchWorker, MemoryDispatchStore,
    RunDispatch, SystemClock,
};
use awaken_store_inmem::MemoryCommitCoordinator;

use harness::{
    FlakyDispatchStore, RecordingMetrics, activation, text_runtime, text_runtime_with_metrics,
    tool_runtime_with_metrics,
};
use std::time::Duration;

async fn wait_for(cond: impl Fn() -> bool) -> bool {
    for _ in 0..600 {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    cond()
}

// --- 10. A transient store error is swallowed and the daemon recovers -------

#[tokio::test]
async fn a_transient_store_error_is_swallowed_and_the_daemon_recovers() {
    // The daemon's drain tick treats a store error as transient: it logs and retries
    // on the next tick rather than dying. A store whose first two claims fail must
    // still drain the run once the injected failures are spent.
    let store = Arc::new(FlakyDispatchStore::new(2));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = Arc::new(DispatchWorker::new(
        text_runtime(),
        store.clone(),
        commit.clone(),
        "solo",
    ));
    let service = DispatchService::spawn(
        worker,
        Arc::new(SystemClock),
        DispatchServiceConfig {
            poll_interval: Duration::from_millis(5),
            ..Default::default()
        },
    );

    service.submit(activation("run-1")).await.expect("submit");

    assert!(
        wait_for(|| commit.commit_count() >= 1).await,
        "the daemon recovered after transient claim failures and drained the run"
    );
    assert_eq!(
        store.remaining_failures(),
        0,
        "the injected transient failures were spent, not skipped"
    );

    service.shutdown().await;
}

// --- 15. The worker meters the dispatch lifecycle on every exit path --------

#[tokio::test]
async fn a_terminal_drive_meters_one_claim_one_drive_and_one_done_settle() {
    // Test design. Causes: one fresh Run reaches NaturalEnd on its first drive.
    // Effects: metrics record one claim, one drive, one Done settle, one applied
    // commit, and zero in-flight/depth. Constraint/Invariant: each lifecycle
    // boundary is metered once. Decision rule: cover the terminal exit partition
    // and assert its complete metric vector.
    let metrics = Arc::new(RecordingMetrics::default());
    let runtime = text_runtime_with_metrics(metrics.clone() as Arc<_>);
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    let worker = DispatchWorker::new(runtime, store, commit, "solo");
    let processed = worker.tick(harness::clock(0)).await.unwrap();
    assert!(matches!(
        processed,
        Some((_, RunState::Ended(EndCause::NaturalEnd)))
    ));

    assert_eq!(
        metrics.claimed.load(Ordering::SeqCst),
        1,
        "one claim metered"
    );
    assert_eq!(
        metrics.drives.load(Ordering::SeqCst),
        1,
        "one drive.duration metered"
    );
    assert_eq!(
        metrics.settled_done.load(Ordering::SeqCst),
        1,
        "exactly one done settle metered"
    );
    assert_eq!(
        metrics.settled_awaiting.load(Ordering::SeqCst),
        0,
        "no awaiting settle for a terminal run"
    );
    assert_eq!(metrics.commits_applied.load(Ordering::SeqCst), 1);
    assert_eq!(metrics.in_flight.load(Ordering::SeqCst), 0);
    assert_eq!(metrics.queue_depth.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_awaiting_drive_meters_one_claim_one_drive_and_one_awaiting_settle() {
    // Test design. Causes: one fresh Run reaches Awaiting on its first drive.
    // Effects: metrics record one claim, one drive, one Awaiting settle, no Done
    // settle, and one retained queue row. Constraint/Invariant: Awaiting is not
    // counted as terminal. Decision rule: cover the nonterminal settle partition
    // and assert its vector against the terminal control above.
    let metrics = Arc::new(RecordingMetrics::default());
    let (runtime, _ran) = tool_runtime_with_metrics(metrics.clone() as Arc<_>);
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    let worker = DispatchWorker::new(runtime, store, commit, "solo");
    let processed = worker.tick(harness::clock(0)).await.unwrap();
    assert!(matches!(processed, Some((_, RunState::Awaiting))));

    assert_eq!(
        metrics.claimed.load(Ordering::SeqCst),
        1,
        "one claim metered"
    );
    assert_eq!(
        metrics.drives.load(Ordering::SeqCst),
        1,
        "one drive.duration metered even though the run awaiting"
    );
    assert_eq!(
        metrics.settled_awaiting.load(Ordering::SeqCst),
        1,
        "exactly one awaiting settle metered"
    );
    assert_eq!(
        metrics.settled_done.load(Ordering::SeqCst),
        0,
        "no done settle for an awaiting run"
    );
}

#[tokio::test]
async fn an_early_terminal_recovery_return_is_still_fully_metered() {
    // Test design. Causes: C1 committed terminal truth predates a fresh/recovered
    // queue drive; C2 Worker takes the early settlement return. Effects: E1 one
    // claim, drive-duration sample, and Done settle are still recorded; E2 no
    // execution occurs. Constraint/Invariant: RAII metering covers every exit.
    // Decision rule: force C1+C2 and assert the full early-return metric vector.
    // The early exit arm: a recovered fresh run whose committed record is ALREADY
    // terminal settles Done and returns early. That path must still meter one claim,
    // one drive.duration (the RAII timer fires on every exit), and one done settle.
    let metrics = Arc::new(RecordingMetrics::default());
    let runtime = text_runtime_with_metrics(metrics.clone() as Arc<_>);
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let run = RunId("run-1".to_string());

    // Commit a terminal record for the run first, then enqueue a fresh dispatch for it
    // and claim-drive: the worker sees no ticket + a terminal record and takes the
    // early-return recovery arm.
    let ctx = awaken_runtime_contract::runtime_context::RuntimeRunContext::new()
        .with_commit(commit.clone());
    use awaken_runtime_contract::execution::RunExecutor;
    let state = runtime.execute(activation("run-1"), ctx).await.unwrap();
    assert!(matches!(state, RunState::Ended(_)));
    assert!(matches!(
        CommittedThreadView::run(&*commit, &run).map(|r| r.state),
        Some(RunState::Ended(_))
    ));
    // Reset the metrics: we only want to measure the recovery drive below.
    metrics.claimed.store(0, Ordering::SeqCst);
    metrics.drives.store(0, Ordering::SeqCst);
    metrics.settled_done.store(0, Ordering::SeqCst);

    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    let worker = DispatchWorker::new(runtime, store.clone(), commit, "recovery");
    let processed = worker.tick(harness::clock(0)).await.unwrap();
    assert!(
        matches!(processed, Some((_, RunState::Ended(_)))),
        "the recovered terminal run settles Done without re-running"
    );
    // The dispatch was settled (removed), via the early recovery arm.
    assert_eq!(store.dispatch_count(), 0);

    assert_eq!(
        metrics.claimed.load(Ordering::SeqCst),
        1,
        "the early-return path still meters a claim"
    );
    assert_eq!(
        metrics.drives.load(Ordering::SeqCst),
        1,
        "the RAII drive timer fired on the early-return exit"
    );
    assert_eq!(
        metrics.settled_done.load(Ordering::SeqCst),
        1,
        "the early recovery settle is metered exactly once"
    );
}

#[tokio::test]
async fn an_expired_lease_reclaim_is_reported_as_recovery() {
    // Test design. Causes: C1 an owner abandons a claim; C2 its lease expires;
    // C3 replacement Worker reclaims. Effects: E1 recovered increments once; E2
    // applied commit increments once; E3 fenced remains zero. Constraint/
    // Invariant: only an expired lease claim is labeled recovery. Decision rule:
    // compare the initial recovered=false claim with C3's recovered drive.
    let metrics = Arc::new(RecordingMetrics::default());
    let runtime = text_runtime_with_metrics(metrics.clone() as Arc<_>);
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    store
        .enqueue(RunDispatch::new(activation("run-recovered")))
        .await
        .unwrap();
    let abandoned = store
        .claim("crashed", 10, 0, &Default::default())
        .await
        .unwrap()
        .unwrap();
    assert!(!abandoned.recovered);

    let worker = DispatchWorker::new(runtime, store, commit, "replacement");
    worker
        .tick(harness::clock(11))
        .await
        .unwrap()
        .expect("recovered drive");

    assert_eq!(metrics.recovered.load(Ordering::SeqCst), 1);
    assert_eq!(metrics.commits_applied.load(Ordering::SeqCst), 1);
    assert_eq!(metrics.fenced.load(Ordering::SeqCst), 0);
    assert_eq!(metrics.in_flight.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_stale_settlement_increments_the_fenced_metric() {
    // Test design. Causes: C1 an old claim is superseded by a newer epoch; C2 the
    // old owner attempts settlement. Effects: E1 C2 is fenced; E2 the fenced
    // metric increments without an applied settlement. Constraint/Invariant:
    // metric classification follows the queue's epoch decision. Decision rule:
    // mint old/current claims, settle old, and assert the fenced vector.
    let metrics = Arc::new(RecordingMetrics::default());
    let runtime = text_runtime_with_metrics(metrics.clone() as Arc<_>);
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    store
        .enqueue(RunDispatch::new(activation("run-fenced")))
        .await
        .unwrap();
    let stale = store
        .claim("old", 10, 0, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let current = store
        .claim("new", 10, 11, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let worker = DispatchWorker::new(runtime, store, commit, "driver");

    worker
        .drive_claimed(current, harness::clock(11))
        .await
        .unwrap()
        .expect("current owner completes");
    assert!(
        worker
            .drive_claimed(stale, harness::clock(11))
            .await
            .unwrap()
            .is_none(),
        "the stale terminal replay is fenced rather than reported as applied"
    );

    assert_eq!(metrics.fenced.load(Ordering::SeqCst), 1);
    assert_eq!(metrics.commits_applied.load(Ordering::SeqCst), 1);
    assert_eq!(metrics.in_flight.load(Ordering::SeqCst), 0);
}
