//! The process-level dispatch pool (O2) over the in-memory store, and its
//! no-loss guarantees (O5).
//!
//! These prove the pool's defining behaviour — it claims from ONE shared queue
//! and routes each run to the worker that owns its thread, so a run drives on its
//! own thread's runtime (never whichever task claimed it) — and the durability
//! properties the per-session daemon had: a dropped wake still drains on the poll
//! fallback, a crashed lease is recovered and re-driven, and a staged cross-thread
//! delivery is relayed at least once.

mod harness;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::run::Phase;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::store::run_store::RunStore;
use awaken_run_ingress::{
    Clock, CompletionSink, DEFAULT_LEASE_MS, DispatchError, DispatchPool, DispatchQueue,
    DispatchServiceConfig, DispatchWorker, Error, Inbox, ManualClock, MemoryDispatchStore,
    PendingInput, RunExecutionRequest, SystemClock, WakeSignal, WorkerResolver,
};
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::resume::ResumeResult;

use harness::{activation, activation_on, blocking_tool_runtime, text_runtime, tool_runtime};

type MemWorker = DispatchWorker<MemoryDispatchStore>;

/// A resolver over a fixed thread → worker map, the test analogue of the host's
/// session lookup. All workers share the pool's store and owner.
struct MapResolver {
    workers: HashMap<String, Arc<MemWorker>>,
}

#[async_trait]
impl WorkerResolver<MemoryDispatchStore> for MapResolver {
    async fn worker_for(&self, thread_id: &ThreadId) -> Result<Arc<MemWorker>, Error> {
        self.workers.get(&thread_id.0).cloned().ok_or_else(|| {
            Error::Execution(awaken_runtime_contract::execution::Error::Execution(
                format!("no worker for thread {}", thread_id.0),
            ))
        })
    }
}

/// Build a worker over the shared store, with its own runtime + commit boundary.
fn worker_over(
    runtime: Arc<Runtime>,
    store: Arc<MemoryDispatchStore>,
    commit: Arc<MemoryCommitCoordinator>,
) -> Arc<MemWorker> {
    Arc::new(DispatchWorker::new(runtime, store, commit, "pool"))
}

async fn wait_for(cond: impl Fn() -> bool) -> bool {
    for _ in 0..600 {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    cond()
}

/// Routing: two threads, each with its own runtime and commit boundary, share one
/// queue and one pool. Each submitted run must commit to *its own* thread's
/// boundary — proving the pool drove it on the owning session's runtime, not on
/// whichever drain task happened to claim it.
#[tokio::test]
async fn routing_drives_each_run_on_its_own_threads_runtime() {
    let store = Arc::new(MemoryDispatchStore::new());
    let commit_a = Arc::new(MemoryCommitCoordinator::new());
    let commit_b = Arc::new(MemoryCommitCoordinator::new());
    let worker_a = worker_over(text_runtime(), store.clone(), commit_a.clone());
    let worker_b = worker_over(text_runtime(), store.clone(), commit_b.clone());

    let resolver = Arc::new(MapResolver {
        workers: HashMap::from([
            ("thread-a".to_string(), worker_a),
            ("thread-b".to_string(), worker_b),
        ]),
    });
    let pool = DispatchPool::spawn(
        store.clone(),
        Arc::new(SystemClock),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig::default(),
        resolver,
        2,
    );

    pool.submit(activation_on("run-a", "thread-a"))
        .await
        .unwrap();
    pool.submit(activation_on("run-b", "thread-b"))
        .await
        .unwrap();

    assert!(
        wait_for(|| commit_a.commit_count() >= 1 && commit_b.commit_count() >= 1).await,
        "both runs drained"
    );

    // Each run landed on its own thread's boundary, and NOT on the other's.
    assert!(RunStore::get(&*commit_a, &RunId("run-a".into())).is_some());
    assert!(RunStore::get(&*commit_a, &RunId("run-b".into())).is_none());
    assert!(RunStore::get(&*commit_b, &RunId("run-b".into())).is_some());
    assert!(RunStore::get(&*commit_b, &RunId("run-a".into())).is_none());

    pool.shutdown().await;
}

/// The pool drains through an INJECTED wake signal (`spawn_with_wake`) — the seam a
/// fleet fills with a durable/cross-node wake. A recording wake double proves the
/// pool's `submit` publishes on it and the run drains.
#[tokio::test]
async fn pool_uses_the_injected_wake_signal() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RecordingWake {
        published: AtomicUsize,
        notify: tokio::sync::Notify,
    }
    #[async_trait]
    impl WakeSignal for RecordingWake {
        async fn publish(&self) -> Result<(), DispatchError> {
            self.published.fetch_add(1, Ordering::SeqCst);
            self.notify.notify_one();
            Ok(())
        }
        async fn wait(&self) {
            self.notify.notified().await;
        }
    }

    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = worker_over(text_runtime(), store.clone(), commit.clone());
    let resolver = Arc::new(MapResolver {
        workers: HashMap::from([(harness::THREAD.to_string(), worker)]),
    });
    let wake = Arc::new(RecordingWake {
        published: AtomicUsize::new(0),
        notify: tokio::sync::Notify::new(),
    });

    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        Arc::new(SystemClock),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            // A long poll so ONLY the injected wake can drive the drain.
            poll_interval: std::time::Duration::from_secs(3600),
            ..Default::default()
        },
        resolver,
        1,
        wake.clone() as Arc<dyn WakeSignal>,
    );

    pool.submit(activation("run-1")).await.unwrap();

    assert!(
        wait_for(|| wake.published.load(Ordering::SeqCst) >= 1).await,
        "submit published on the injected wake"
    );
    assert!(
        wait_for(|| commit.commit_count() >= 1).await,
        "the run drained via the injected wake (poll is disabled)"
    );

    pool.shutdown().await;
}

/// Event-driven completion: the pool signals a `CompletionSink` the instant it
/// settles a run — the mechanism that lets a foreground submitter wait by event,
/// not by polling committed truth.
#[tokio::test]
async fn pool_signals_completion_sink_on_settle() {
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingSink {
        settled: Mutex<Vec<(String, Phase)>>,
    }
    impl CompletionSink for RecordingSink {
        fn settled(&self, run_id: &RunId, phase: &Phase) {
            self.settled
                .lock()
                .unwrap()
                .push((run_id.0.clone(), phase.clone()));
        }
    }

    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = worker_over(text_runtime(), store.clone(), commit.clone());
    let resolver = Arc::new(MapResolver {
        workers: HashMap::from([(harness::THREAD.to_string(), worker)]),
    });
    let sink = Arc::new(RecordingSink::default());

    let pool = DispatchPool::spawn_with_completion(
        store.clone(),
        Arc::new(SystemClock),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig::default(),
        resolver,
        1,
        sink.clone() as Arc<dyn CompletionSink>,
    );

    pool.submit(activation("run-1")).await.unwrap();

    // The sink is notified with the run and its settled (Ended) phase.
    assert!(
        wait_for(|| !sink.settled.lock().unwrap().is_empty()).await,
        "the pool signalled completion"
    );
    let recorded = sink.settled.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].0, "run-1");
    assert!(
        matches!(recorded[0].1, Phase::Ended(_)),
        "signalled the settled Ended phase, got {:?}",
        recorded[0].1
    );

    pool.shutdown().await;
}

/// No-loss via the poll fallback: a run enqueued straight to the store — with no
/// `submit`, so no wake is ever published — is still drained by the pool on its
/// poll cadence. A lost wake only delays a run to the poll; it never loses it.
#[tokio::test]
async fn dropped_wake_still_drains_via_poll() {
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = worker_over(text_runtime(), store.clone(), commit.clone());
    let resolver = Arc::new(MapResolver {
        workers: HashMap::from([(harness::THREAD.to_string(), worker)]),
    });
    let pool = DispatchPool::spawn(
        store.clone(),
        Arc::new(SystemClock),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_millis(20),
            ..Default::default()
        },
        resolver,
        1,
    );

    // Enqueue directly — bypassing the pool's submit(), so no wake is delivered.
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();

    assert!(
        wait_for(|| commit.commit_count() >= 1).await,
        "the poll fallback drained a run whose wake never arrived"
    );

    pool.shutdown().await;
}

/// No-loss across a crash: a run whose owner claimed it and then vanished (lease
/// held, nothing driven) is reclaimed by the pool once the lease expires, and
/// re-driven to completion — recovery is driven by the pool's clock.
#[tokio::test]
async fn crashed_lease_is_recovered_and_redriven() {
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = worker_over(text_runtime(), store.clone(), commit.clone());
    let resolver = Arc::new(MapResolver {
        workers: HashMap::from([(harness::THREAD.to_string(), worker)]),
    });

    // A crashed worker: the run is claimed with a 1s lease at t=0, nothing driven.
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    assert!(
        store
            .claim("dead-worker", 1_000, 0)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(commit.commit_count(), 0);

    // A hand-driven clock so only the advance recovers it, not a fast poll.
    let clock = Arc::new(ManualClock::new(0));
    let pool = DispatchPool::spawn(
        store.clone(),
        clock.clone(),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(10),
            ..Default::default()
        },
        resolver,
        1,
    );

    // Past the crashed lease, nudge: the pool reclaims and drives the dispatch.
    clock.set(2_000);
    pool.notify().await;
    assert!(
        wait_for(|| commit.commit_count() >= 1).await,
        "the pool recovered and re-drove the crashed run"
    );

    pool.shutdown().await;
}

/// At-least-once outbox relay: a staged cross-thread delivery is moved into the
/// target thread's pending input by the pool's maintenance loop, exactly once.
#[tokio::test]
async fn staged_cross_thread_delivery_is_relayed_at_least_once() {
    let store = Arc::new(MemoryDispatchStore::new());
    let resolver = Arc::new(MapResolver {
        workers: HashMap::new(),
    });
    let pool = DispatchPool::spawn(
        store.clone(),
        Arc::new(SystemClock),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_millis(20),
            ..Default::default()
        },
        resolver,
        1,
    );

    let thread = ThreadId(harness::THREAD.to_string());
    let staged = PendingInput {
        message_id: "xthread-1".to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: thread.clone(),
        correlation_id: harness::TICKET.to_string(),
        available_at_ms: None,
        result: ResumeResult::Decision {
            allow: true,
            note: None,
        },
    };
    pool.send(staged).await.unwrap();

    // The maintenance loop relays the outbox into the thread's pending inbox.
    let mut relayed = false;
    for _ in 0..600 {
        let records = store.list(&thread).await.unwrap();
        if records.iter().any(|r| r.input.message_id == "xthread-1") {
            relayed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        relayed,
        "the pool relayed the staged delivery into pending input"
    );

    // Idempotent: it appears exactly once, never duplicated by repeated relays.
    let records = store.list(&thread).await.unwrap();
    assert_eq!(
        records
            .iter()
            .filter(|r| r.input.message_id == "xthread-1")
            .count(),
        1
    );

    pool.shutdown().await;
}

/// A drain task must SWALLOW a transient claim/drive error and keep going: when the
/// resolver has no worker for a claimed run's thread, `worker_for` errors, the drive
/// tick fails, and the task logs-and-backs-off rather than dying. The dispatch is
/// left un-settled (still claimed), and the pool stays live — a later run on a mapped
/// thread still drains.
#[tokio::test]
async fn a_resolver_error_is_swallowed_and_the_drain_survives() {
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let good_worker = worker_over(text_runtime(), store.clone(), commit.clone());
    // Only "thread-good" resolves; a run on "orphan-thread" has no worker.
    let resolver = Arc::new(MapResolver {
        workers: HashMap::from([("thread-good".to_string(), good_worker)]),
    });
    let pool = DispatchPool::spawn(
        store.clone(),
        Arc::new(SystemClock),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_millis(10),
            ..Default::default()
        },
        resolver,
        1,
    );

    // An orphan run whose thread has no worker: the pool claims it, then `worker_for`
    // errors. The claim is real (the row goes Running) but nothing is ever driven.
    pool.submit(activation_on("orphan-run", "orphan-thread"))
        .await
        .unwrap();
    let claimed = wait_for_async(|| {
        let store = store.clone();
        async move {
            store
                .list_dispatches()
                .await
                .unwrap()
                .iter()
                .any(|d| d.run_id.0 == "orphan-run")
        }
    })
    .await;
    assert!(claimed, "the orphan run is present (claimed but never driven)");
    assert_eq!(commit.commit_count(), 0, "nothing was driven for the orphan");

    // The drain did NOT die: a run on a mapped thread still drains.
    pool.submit(activation_on("good-run", "thread-good"))
        .await
        .unwrap();
    assert!(
        wait_for(|| commit.commit_count() >= 1).await,
        "the drain survived the resolver error and drove a later run"
    );

    // The orphan dispatch is still present and un-settled (left for recovery).
    assert!(
        store
            .list_dispatches()
            .await
            .unwrap()
            .iter()
            .any(|d| d.run_id.0 == "orphan-run"),
        "the un-drivable orphan dispatch is left un-settled"
    );

    pool.shutdown().await;
}

/// Concurrency > 1 drives DISTINCT runs in parallel: with two drain tasks and two
/// runs on two threads, both runs must be in-flight *simultaneously*. Each run's tool
/// blocks on a shared gate, so both counters can only both reach 1 if two drives run
/// at once — a single drain would deadlock (the first drive blocks forever, so the
/// second run is never claimed).
#[tokio::test]
async fn concurrency_drives_distinct_runs_in_parallel() {
    use std::sync::atomic::Ordering;

    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (rt_a, ran_a) = blocking_tool_runtime(release.clone());
    let (rt_b, ran_b) = blocking_tool_runtime(release.clone());
    let store = Arc::new(MemoryDispatchStore::new());
    let commit_a = Arc::new(MemoryCommitCoordinator::new());
    let commit_b = Arc::new(MemoryCommitCoordinator::new());
    let worker_a = worker_over(rt_a, store.clone(), commit_a.clone());
    let worker_b = worker_over(rt_b, store.clone(), commit_b.clone());
    let resolver = Arc::new(MapResolver {
        workers: HashMap::from([
            ("thread-a".to_string(), worker_a),
            ("thread-b".to_string(), worker_b),
        ]),
    });
    let pool = DispatchPool::spawn(
        store.clone(),
        Arc::new(SystemClock),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig::default(),
        resolver,
        2,
    );

    pool.submit(activation_on("run-a", "thread-a"))
        .await
        .unwrap();
    pool.submit(activation_on("run-b", "thread-b"))
        .await
        .unwrap();

    // Both runs reach their blocking tool at the same time — only possible with two
    // parallel drains. (Distinct claims: no double-claim, since each thread's run
    // landed on its own runtime.)
    assert!(
        wait_for(|| ran_a.load(Ordering::SeqCst) >= 1 && ran_b.load(Ordering::SeqCst) >= 1).await,
        "both runs were driven in parallel (concurrency = 2)"
    );

    // Release both blocked tools; both drives complete on their own boundaries.
    release.add_permits(2);
    assert!(
        wait_for(|| commit_a.commit_count() >= 1 && commit_b.commit_count() >= 1).await,
        "both parallel runs settled"
    );

    pool.shutdown().await;
}

/// Graceful shutdown AWAITS an in-flight drive: `shutdown()` cancels the drains but
/// must not return until a drive already running to completion finishes — it never
/// drops a half-driven run. The drive is frozen in its tool; shutdown stays pending
/// until the tool is released, then the run settles.
#[tokio::test]
async fn shutdown_awaits_an_in_flight_drive() {
    use std::sync::atomic::Ordering;

    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, ran) = blocking_tool_runtime(release.clone());
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = worker_over(runtime, store.clone(), commit.clone());
    let resolver = Arc::new(MapResolver {
        workers: HashMap::from([(harness::THREAD.to_string(), worker)]),
    });
    let pool = DispatchPool::spawn(
        store.clone(),
        Arc::new(SystemClock),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig::default(),
        resolver,
        1,
    );

    pool.submit(activation("run-1")).await.unwrap();
    assert!(
        wait_for(|| ran.load(Ordering::SeqCst) >= 1).await,
        "the drive reached and blocked in its tool (in-flight)"
    );
    // The drive committed a mid-flight `Running` step but has NOT reached a terminus.
    let run = RunId("run-1".to_string());
    assert!(
        matches!(
            RunStore::get(&*commit, &run).map(|r| r.phase),
            Some(Phase::Running)
        ),
        "the run is in-flight (Running), not yet settled"
    );

    // Shutdown is initiated while the drive is frozen; it must NOT complete until the
    // in-flight drive finishes.
    let shutdown = tokio::spawn(async move { pool.shutdown().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !shutdown.is_finished(),
        "shutdown is still awaiting the in-flight drive"
    );

    // Release the tool: the drive runs to completion, then shutdown returns.
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), shutdown)
        .await
        .expect("shutdown returned once the drive finished")
        .expect("shutdown task did not panic");
    assert!(
        matches!(
            RunStore::get(&*commit, &run).map(|r| r.phase),
            Some(Phase::Ended(_))
        ),
        "the in-flight run was driven to completion, not dropped by shutdown"
    );
}

/// The completion sink is signalled on EVERY settle — including a `Parked` re-park.
/// A run that parks on a waiting ticket settles `Parked`, and the sink must be
/// notified with the `Waiting` phase (not only on a terminal `Ended`).
#[tokio::test]
async fn completion_sink_is_signalled_with_waiting_on_a_park() {
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingSink {
        settled: Mutex<Vec<(String, Phase)>>,
    }
    impl CompletionSink for RecordingSink {
        fn settled(&self, run_id: &RunId, phase: &Phase) {
            self.settled
                .lock()
                .unwrap()
                .push((run_id.0.clone(), phase.clone()));
        }
    }

    let (runtime, _ran) = tool_runtime(); // parks on the suspend gate
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = worker_over(runtime, store.clone(), commit.clone());
    let resolver = Arc::new(MapResolver {
        workers: HashMap::from([(harness::THREAD.to_string(), worker)]),
    });
    let sink = Arc::new(RecordingSink::default());
    let pool = DispatchPool::spawn_with_completion(
        store.clone(),
        Arc::new(SystemClock),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig::default(),
        resolver,
        1,
        sink.clone() as Arc<dyn CompletionSink>,
    );

    pool.submit(activation("run-1")).await.unwrap();
    assert!(
        wait_for(|| !sink.settled.lock().unwrap().is_empty()).await,
        "the pool signalled completion for the parked run"
    );
    let recorded = sink.settled.lock().unwrap().clone();
    assert_eq!(recorded[0].0, "run-1");
    assert!(
        matches!(recorded[0].1, Phase::Waiting),
        "signalled the Waiting phase on a park, got {:?}",
        recorded[0].1
    );

    pool.shutdown().await;
}

/// The renewal heartbeat keeps a long run's lease alive: while a drive is frozen in
/// its tool for far longer than the base lease, the pool's renewal loop keeps
/// extending the lease so a peer's recovery claim can never steal the run.
#[tokio::test]
async fn the_renewal_loop_keeps_a_long_run_from_being_reclaimed() {
    use std::sync::atomic::Ordering;

    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, ran) = blocking_tool_runtime(release.clone());
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = worker_over(runtime, store.clone(), commit.clone());
    let resolver = Arc::new(MapResolver {
        workers: HashMap::from([(harness::THREAD.to_string(), worker)]),
    });
    let clock = Arc::new(SystemClock);
    // A short 100ms lease with a 20ms renewal cadence (well under half the lease).
    let lease_ms = 100;
    let pool = DispatchPool::spawn(
        store.clone(),
        clock.clone(),
        "pool",
        lease_ms,
        DispatchServiceConfig {
            poll_interval: Duration::from_millis(10),
            lease_renewal_interval: Some(Duration::from_millis(20)),
            ..Default::default()
        },
        resolver,
        1,
    );

    pool.submit(activation("run-1")).await.unwrap();
    assert!(
        wait_for(|| ran.load(Ordering::SeqCst) >= 1).await,
        "the drive is in-flight, holding the pool's lease"
    );

    // Across a window far beyond the base lease, a thief owner can never reclaim it —
    // the renewal loop keeps the lease from expiring.
    for _ in 0..30 {
        let now = clock.now_ms();
        assert!(
            store.claim("thief", lease_ms, now).await.unwrap().is_none(),
            "the renewed lease is never reclaimable while the run is in-flight"
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }

    release.add_permits(1);
    assert!(
        wait_for(|| commit.commit_count() >= 1).await,
        "the run finished once released"
    );
    pool.shutdown().await;
}

/// The maintenance loop REAPS a poison run (past its crash-retry budget) and later
/// GCs the dead-letter. The single drain is kept busy on a blocking run so it cannot
/// reclaim the poison first, making the reap deterministic; then advancing the clock
/// past the ttl lets the same loop purge the dead-letter.
#[tokio::test]
async fn the_maintenance_loop_reaps_a_poison_run_then_gcs_it() {
    use std::sync::atomic::Ordering;

    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (busy_rt, busy_ran) = blocking_tool_runtime(release.clone());
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let busy_worker = worker_over(busy_rt, store.clone(), commit.clone());

    // A "busy" run enqueued first (so it is the OLDEST recovery candidate) and held
    // under a LONG lease, so it does not interfere while the poison is driven to its
    // budget below. The pool's single drain will claim it first and block in its tool,
    // so the drain can never reach the poison.
    store
        .enqueue(RunExecutionRequest::new(activation_on("busy", "thread-busy")))
        .await
        .unwrap();
    assert!(store.claim("dead-a", 1_000, 0).await.unwrap().is_some());

    // A poison run pre-driven to its crash-retry budget (attempt_count == 2), with an
    // expired lease. `busy`'s lease is still live during these claims, so each
    // recovery pick lands on the poison, not on `busy`.
    store
        .enqueue(RunExecutionRequest::new(activation_on(
            "poison",
            "thread-poison",
        )))
        .await
        .unwrap();
    assert!(store.claim("dead-b", 1, 5).await.unwrap().is_some()); // fresh -> attempt 0
    assert!(store.claim("dead-b", 1, 10).await.unwrap().is_some()); // recovery -> attempt 1
    assert!(store.claim("dead-b", 1, 15).await.unwrap().is_some()); // recovery -> attempt 2

    let resolver = Arc::new(MapResolver {
        workers: HashMap::from([("thread-busy".to_string(), busy_worker)]),
    });
    // Past both leases (busy expires at 1000, poison at 16): both are recovery-eligible,
    // but the drain claims the older `busy` first.
    let clock = Arc::new(ManualClock::new(1_500));
    let ttl = Duration::from_millis(50);
    let pool = DispatchPool::spawn(
        store.clone(),
        clock.clone(),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_millis(5),
            max_attempts: 2,
            dead_letter_ttl: Some(ttl),
            ..Default::default()
        },
        resolver,
        1,
    );

    // The drain grabs the older busy run and blocks; the maintenance loop reaps the
    // poison (attempt 2 >= max 2, lease expired) deterministically.
    assert!(
        wait_for(|| busy_ran.load(Ordering::SeqCst) >= 1).await,
        "the drain is busy on the blocking run"
    );
    let dead_lettered = wait_for_async(|| {
        let store = store.clone();
        async move {
            store
                .dead_letters()
                .await
                .unwrap()
                .contains(&RunId("poison".to_string()))
        }
    })
    .await;
    assert!(dead_lettered, "the maintenance loop reaped the poison run");

    // Advance the clock past the ttl: the maintenance loop GCs the dead-letter.
    clock.set(1_500 + ttl.as_millis() as u64 + 50);
    let gced = wait_for_async(|| {
        let store = store.clone();
        async move { store.dead_letters().await.unwrap().is_empty() }
    })
    .await;
    assert!(gced, "the maintenance loop GC'd the aged dead-letter");

    release.add_permits(1);
    pool.shutdown().await;
}

/// Poll an async predicate up to ~3s — the async analogue of [`wait_for`], for
/// conditions that require an `await` (a store query) to evaluate.
async fn wait_for_async<F, Fut>(cond: F) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..600 {
        if cond().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    cond().await
}
