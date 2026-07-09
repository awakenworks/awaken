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
    CompletionSink, DEFAULT_LEASE_MS, DispatchError, DispatchPool, DispatchQueue,
    DispatchServiceConfig, DispatchWorker, Error, Inbox, ManualClock, MemoryDispatchStore,
    PendingInput, RunExecutionRequest, SystemClock, WakeSignal, WorkerResolver,
};
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::resume::ResumeResult;

use harness::{activation, activation_on, text_runtime};

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
