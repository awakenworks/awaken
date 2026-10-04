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
use awaken_lease_testkit::ManualClock;
use awaken_dispatch::{
    ControlDispatchQueue, DispatchQueue, DispatchService, DispatchServiceConfig, DispatchWorker,
    MemoryDispatchStore, RunDispatch,
};
use awaken_store_inmem::MemoryCommitCoordinator;

use harness::{
    FlakyDispatchStore, RecordingMetrics, activation, activation_on,
    all_calls_blocking_tool_runtime_with_metrics, text_runtime, text_runtime_with_metrics,
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

#[tokio::test]
async fn worker_demand_keeps_a_live_lease_and_reclassifies_expiry() {
    // Cause/effect table: Q1 fresh pending -> (runnable=1, leased=0);
    // Q2 valid claim before expiry -> (0, 1); Q3 the same claim at its exact
    // expiry -> (1, 0). Every row has demand=1, so a long executing Run
    // cannot make the only Worker disappear, while expiry never double-counts
    // the same Dispatch. This is observation only; the claim epoch is unchanged.
    let clock = Arc::new(ManualClock::new(0));
    let store = MemoryDispatchStore::new(clock.clone());
    store
        .enqueue(RunDispatch::new(activation("worker-demand-expiry")))
        .await
        .unwrap();
    let depths = store.dispatch_depths().await.unwrap().unwrap();
    assert_eq!(
        (depths.runnable, depths.leased, depths.worker_demand()),
        (1, 0, 1),
        "Q1"
    );
    store
        .claim_run(
            &RunId("worker-demand-expiry".into()),
            "worker",
            1_000,
            &Default::default(),
        )
        .await
        .unwrap()
        .expect("Q2 claim");
    let depths = store.dispatch_depths().await.unwrap().unwrap();
    assert_eq!(
        (depths.runnable, depths.leased, depths.worker_demand()),
        (0, 1, 1),
        "Q2"
    );
    clock.set(1_000);
    let depths = store.dispatch_depths().await.unwrap().unwrap();
    assert_eq!(
        (depths.runnable, depths.leased, depths.worker_demand()),
        (1, 0, 1),
        "Q3"
    );
}

// --- 10. A transient store error is swallowed and the daemon recovers -------

#[tokio::test]
async fn a_transient_store_error_is_swallowed_and_the_daemon_recovers() {
    // The daemon's drain tick treats a store error as transient: it logs and retries
    // on the next tick rather than dying. A store whose first two claims fail must
    // still drain the run once the injected failures are spent.
    let store = Arc::new(FlakyDispatchStore::new(
        2,
        awaken_lease_contract::system_clock(),
    ));
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));
    let worker = Arc::new(DispatchWorker::new(
        text_runtime(),
        store.clone(),
        commit.clone(),
        "solo",
    ));
    let service = DispatchService::spawn(
        worker,
        awaken_lease_contract::system_clock(),
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

#[tokio::test(start_paused = true)]
async fn queue_depth_is_sampled_by_the_process_not_each_drive() {
    // Causes: C1 two Runs are already pending at startup; C2 both drives finish
    // before the 60-second sample interval; C3 that interval later elapses.
    // Effects: E1 one startup observation runs independently of execution; E2 two claims and
    // settlements cause no extra depth reads; E3 the next sample reports 0.
    // Constraint: the operations query stays exact when called, but a failed
    // or delayed gauge observation never participates in claim/commit. Decision
    // table: R1=C1 -> E1; R2=C1+C2 -> E2; R3=C1+C2+C3 -> E3.
    let metrics = Arc::new(RecordingMetrics::default());
    let runtime = text_runtime_with_metrics(metrics.clone() as Arc<_>);
    let store = Arc::new(FlakyDispatchStore::new(
        0,
        awaken_lease_contract::system_clock(),
    ));
    for run in ["sample-1", "sample-2"] {
        store
            .enqueue(RunDispatch::new(activation(run)))
            .await
            .unwrap();
    }
    let worker = Arc::new(DispatchWorker::new(
        runtime,
        store.clone(),
        Arc::new(MemoryCommitCoordinator::new(
            awaken_lease_contract::system_clock(),
        )),
        "sampler",
    ));
    let service = DispatchService::spawn(
        worker,
        awaken_lease_contract::system_clock(),
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(60),
            terminal_reconciliation_interval: None,
            ..Default::default()
        },
    );
    for _ in 0..500 {
        if metrics.claimed.load(Ordering::SeqCst) == 2 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(metrics.claimed.load(Ordering::SeqCst), 2, "R2 claims");
    assert_eq!(store.depth_reads(), 1, "R1/R2 one startup depth query");
    assert_eq!(metrics.queue_depth_samples.load(Ordering::SeqCst), 1, "R1");
    assert!(
        metrics.queue_depth.load(Ordering::SeqCst) <= 2,
        "R1/E1 snapshot"
    );
    tokio::time::advance(Duration::from_secs(60)).await;
    for _ in 0..100 {
        if store.depth_reads() == 2 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(store.depth_reads(), 2, "R3 second timed observation");
    assert_eq!(metrics.queue_depth.load(Ordering::SeqCst), 0, "R3/E3");
    service.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn hosted_depth_owner_disables_only_the_open_sampler() {
    // Causes: C1 a hosted composition owns queue-depth observation; C2 one
    // runnable Run is submitted; C3 more than two default sample periods pass.
    // Effects: E1 the open process issues no exact depth reads; E2 dispatch
    // still claims and commits the Run. Rule H1=C1+C2+C3 -> E1+E2. The hosted
    // observer reads the same queue through its existing ControlDispatch port;
    // this flag grants no execution or fact authority.
    let metrics = Arc::new(RecordingMetrics::default());
    let store = Arc::new(FlakyDispatchStore::new(
        0,
        awaken_lease_contract::system_clock(),
    ));
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));
    let worker = Arc::new(DispatchWorker::new(
        text_runtime_with_metrics(metrics.clone() as Arc<_>),
        store.clone(),
        commit.clone(),
        "hosted-observer",
    ));
    let service = DispatchService::spawn(
        worker,
        awaken_lease_contract::system_clock(),
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(60),
            terminal_reconciliation_interval: None,
            sample_dispatch_queue_depth: false,
            ..Default::default()
        },
    );
    service
        .submit(activation("hosted-depth-owner"))
        .await
        .unwrap();
    for _ in 0..500 {
        if metrics.claimed.load(Ordering::SeqCst) == 1 && commit.commit_count() >= 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(metrics.claimed.load(Ordering::SeqCst), 1, "H1/E2 one lease");
    assert!(commit.commit_count() >= 1, "H1/E2 committed facts");
    tokio::time::advance(Duration::from_secs(120)).await;
    tokio::task::yield_now().await;
    assert_eq!(store.depth_reads(), 0, "H1/E1");
    assert_eq!(
        metrics.queue_depth_samples.load(Ordering::SeqCst),
        0,
        "H1/E1"
    );
    service.shutdown().await;
}

#[tokio::test]
async fn blocked_depth_observation_cannot_delay_a_run_or_shutdown() {
    // Causes: C1 the first exact depth read never returns; C2 a runnable Run
    // exists; C3 shutdown arrives before the read is released. Effects: E1
    // the ordinary Worker commits and settles C2; E2 shutdown joins without
    // waiting for the metrics dependency. Rule B1=C1+C2 -> E1;
    // B2=C1+C3 -> E2. The queue read has no claim, commit, or outbox authority.
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let store = Arc::new(
        FlakyDispatchStore::new(0, awaken_lease_contract::system_clock())
            .with_depth_gate(entered.clone(), release),
    );
    store
        .enqueue(RunDispatch::new(activation("depth-blocked-run")))
        .await
        .unwrap();
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));
    let worker = Arc::new(DispatchWorker::new(
        text_runtime(),
        store.clone(),
        commit.clone(),
        "depth-blocked-owner",
    ));
    let service = DispatchService::spawn(
        worker,
        awaken_lease_contract::system_clock(),
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(60),
            terminal_reconciliation_interval: None,
            ..Default::default()
        },
    );
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("B1 the metric read entered");
    assert!(
        wait_for(|| commit.commit_count() > 0).await,
        "B1 commit progressed"
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if store.list_dispatches().await.unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("B1/E1 the Run settled while the metric read was blocked");
    tokio::time::timeout(Duration::from_secs(2), service.shutdown())
        .await
        .expect("B2/E2 cancels the stuck metrics read");
}

// --- 15. The worker meters the dispatch lifecycle on every exit path --------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_worker_drives_count_only_current_executions_not_queued_runs() {
    // Causes: C1 four independent Threads enter the existing blocking tool
    // barrier under four current Run claims; C2 one fifth Run is only queued;
    // C3 all four tool calls are released. Effects: E1 the shared Worker metric
    // counts four in-flight drives, not five accepted Runs; E2 the exact queue
    // depth counts only the fifth runnable Run; E3 four distinct terminal Runs
    // settle and the in-flight count returns to zero. Decision rules:
    // A1=C1->E1, A2=C1+C2->E1+E2, A3=C1+C2+C3->E2+E3.
    // The deployed Provider hold/SQL observer owns the cross-process partition;
    // this test pins the open runtime's metric meaning without another ledger.
    let metrics = Arc::new(RecordingMetrics::default());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, entered) =
        all_calls_blocking_tool_runtime_with_metrics(release.clone(), metrics.clone() as Arc<_>);
    let store = Arc::new(MemoryDispatchStore::new(
        awaken_lease_contract::system_clock(),
    ));
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));
    for index in 0..4 {
        store
            .enqueue(RunDispatch::new(activation_on(
                &format!("active-{index}"),
                &format!("active-thread-{index}"),
            )))
            .await
            .unwrap();
    }
    let mut drives = tokio::task::JoinSet::new();
    for index in 0..4 {
        // One direct Worker drive owns one claim; independent owners exercise
        // the same aggregate metric without assuming a single tick loops four
        // times while its first call is blocked.
        let worker = DispatchWorker::new(
            runtime.clone(),
            store.clone(),
            commit.clone(),
            format!("held-worker-{index}"),
        );
        drives.spawn(async move { worker.tick(harness::clock(0)).await });
    }
    assert!(
        wait_for(|| entered.load(Ordering::SeqCst) == 4).await,
        "A1 four real tool entries"
    );
    assert_eq!(metrics.in_flight.load(Ordering::SeqCst), 4, "A1/E1");
    store
        .enqueue(RunDispatch::new(activation_on(
            "queued-fifth",
            "queued-thread",
        )))
        .await
        .unwrap();
    assert_eq!(metrics.in_flight.load(Ordering::SeqCst), 4, "A2/E1");
    assert_eq!(store.runnable_depth().await.unwrap(), Some(1), "A2/E2");
    // C1 four live leased Runs; C2 one independent queued Run; C3 all four
    // settle. D1(C1+C2) -> runnable=1, leased=4, Worker demand=5.
    // D2(C2+C3) -> runnable=1, leased=0, demand=1. This is the exact
    // scale-down boundary that a runnable-only gauge previously missed.
    let depths = store.dispatch_depths().await.unwrap().unwrap();
    assert_eq!(
        (depths.runnable, depths.leased, depths.worker_demand()),
        (1, 4, 5),
        "D1"
    );
    release.add_permits(4);
    let mut winners = std::collections::HashSet::new();
    while let Some(result) = tokio::time::timeout(Duration::from_secs(10), drives.join_next())
        .await
        .expect("A3 held drives terminate")
    {
        let (run_id, state) = result.unwrap().unwrap().expect("A3 one claim per drive");
        assert!(
            matches!(state, RunState::Ended(EndCause::NaturalEnd)),
            "A3 terminal"
        );
        assert!(winners.insert(run_id.0), "A3 no duplicate execution");
    }
    assert_eq!(winners.len(), 4, "A3/E3");
    assert_eq!(metrics.in_flight.load(Ordering::SeqCst), 0, "A3/E3");
    assert_eq!(store.runnable_depth().await.unwrap(), Some(1), "A3/E2");
    let depths = store.dispatch_depths().await.unwrap().unwrap();
    assert_eq!(
        (depths.runnable, depths.leased, depths.worker_demand()),
        (1, 0, 1),
        "D2"
    );
}

#[tokio::test]
async fn a_terminal_drive_meters_one_claim_one_drive_and_one_done_settle() {
    // Test design. Causes: one fresh Run reaches NaturalEnd on its first drive.
    // Effects: metrics record one claim, one drive, one Done settle, one applied
    // commit, and zero in-flight/depth. Constraint/Invariant: each lifecycle
    // boundary is metered once. Decision rule: cover the terminal exit partition
    // and assert its complete metric vector.
    let metrics = Arc::new(RecordingMetrics::default());
    let runtime = text_runtime_with_metrics(metrics.clone() as Arc<_>);
    let store = Arc::new(MemoryDispatchStore::new(
        awaken_lease_contract::system_clock(),
    ));
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));

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
    let store = Arc::new(MemoryDispatchStore::new(
        awaken_lease_contract::system_clock(),
    ));
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));

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
    let store = Arc::new(MemoryDispatchStore::new(
        awaken_lease_contract::system_clock(),
    ));
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));
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
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));

    store
        .enqueue(RunDispatch::new(activation("run-recovered")))
        .await
        .unwrap();
    clock.set(0);
    let abandoned = store
        .claim("crashed", 10, &Default::default())
        .await
        .unwrap()
        .unwrap();
    assert!(!abandoned.recovered);

    let worker = DispatchWorker::new(runtime, store, commit, "replacement");
    worker
        .tick(harness::at(&clock, 11))
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
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));

    store
        .enqueue(RunDispatch::new(activation("run-fenced")))
        .await
        .unwrap();
    clock.set(0);
    let stale = store
        .claim("old", 10, &Default::default())
        .await
        .unwrap()
        .unwrap();
    clock.set(11);
    let current = store
        .claim("new", 10, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let worker = DispatchWorker::new(runtime, store, commit, "driver");

    worker
        .drive_claimed(current, harness::at(&clock, 11))
        .await
        .unwrap()
        .expect("current owner completes");
    assert!(
        worker
            .drive_claimed(stale, harness::at(&clock, 11))
            .await
            .unwrap()
            .is_none(),
        "the stale terminal replay is fenced rather than reported as applied"
    );

    assert_eq!(metrics.fenced.load(Ordering::SeqCst), 1);
    assert_eq!(metrics.commits_applied.load(Ordering::SeqCst), 1);
    assert_eq!(metrics.in_flight.load(Ordering::SeqCst), 0);
}
