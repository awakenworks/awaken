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
use awaken_agent_contract::thread::read::run_store::RunStore;
use awaken_run_ingress::{
    DispatchQueue, DispatchService, DispatchServiceConfig, DispatchWorker, MemoryDispatchStore,
    RunExecutionRequest, SystemClock,
};
use awaken_runtime::memory::MemoryCommitCoordinator;

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
    let metrics = Arc::new(RecordingMetrics::default());
    let runtime = text_runtime_with_metrics(metrics.clone() as Arc<_>);
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    let worker = DispatchWorker::new(runtime, store, commit, "solo");
    let processed = worker.tick(0).await.unwrap();
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
}

#[tokio::test]
async fn an_awaiting_drive_meters_one_claim_one_drive_and_one_awaiting_settle() {
    let metrics = Arc::new(RecordingMetrics::default());
    let (runtime, _ran) = tool_runtime_with_metrics(metrics.clone() as Arc<_>);
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    let worker = DispatchWorker::new(runtime, store, commit, "solo");
    let processed = worker.tick(0).await.unwrap();
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
        RunStore::get(&*commit, &run).map(|r| r.state),
        Some(RunState::Ended(_))
    ));
    // Reset the metrics: we only want to measure the recovery drive below.
    metrics.claimed.store(0, Ordering::SeqCst);
    metrics.drives.store(0, Ordering::SeqCst);
    metrics.settled_done.store(0, Ordering::SeqCst);

    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    let worker = DispatchWorker::new(runtime, store.clone(), commit, "recovery");
    let processed = worker.tick(0).await.unwrap();
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
