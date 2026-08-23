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
use awaken_agent_contract::agent::run::RunState;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_run_ingress::{
    Clock, CompletionSink, DEFAULT_LEASE_MS, DispatchError, DispatchMaintenance, DispatchPool,
    DispatchQueue, DispatchServiceConfig, DispatchWorker, Error, Inbox, ManualClock,
    MemoryDispatchStore, PendingInput, RunDispatch, SystemClock, WakeSignal, WorkerResolver,
};
use awaken_runtime::Runtime;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_store_inmem::MemoryCommitCoordinator;

use harness::{
    FlakyDispatchStore, activation, activation_on, blocking_tool_runtime, text_runtime,
    tool_runtime,
};

type MemWorker = DispatchWorker<MemoryDispatchStore>;
type FlakyWorker = DispatchWorker<FlakyDispatchStore>;

struct BlackholeWake;

#[async_trait]
impl WakeSignal for BlackholeWake {
    async fn publish(&self) -> Result<(), DispatchError> {
        Ok(())
    }

    async fn wait(&self) {
        std::future::pending::<()>().await
    }
}

/// A resolver over a fixed thread → worker map, the test analogue of the host's
/// session lookup. All workers share the pool's store and owner.
struct MapResolver {
    workers: HashMap<String, Arc<MemWorker>>,
}

#[async_trait]
impl WorkerResolver<MemoryDispatchStore> for MapResolver {
    async fn worker_for(
        &self,
        thread_id: &ThreadId,
        _agent_id: Option<&str>,
    ) -> Result<Arc<MemWorker>, Error> {
        self.workers.get(&thread_id.0).cloned().ok_or_else(|| {
            Error::Execution(awaken_runtime_contract::execution::Error::Execution(
                format!("no worker for thread {}", thread_id.0),
            ))
        })
    }
}

struct RejectingResolver {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

struct NotReadyResolver {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl WorkerResolver<MemoryDispatchStore> for NotReadyResolver {
    async fn worker_for(
        &self,
        _thread_id: &ThreadId,
        _agent_id: Option<&str>,
    ) -> Result<Arc<MemWorker>, Error> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(Error::ResolutionNotReady(
            "synthetic Environment Work pressure".into(),
        ))
    }
}

#[async_trait]
impl WorkerResolver<MemoryDispatchStore> for RejectingResolver {
    async fn worker_for(
        &self,
        _thread_id: &ThreadId,
        _agent_id: Option<&str>,
    ) -> Result<Arc<MemWorker>, Error> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(Error::Execution(
            awaken_runtime_contract::execution::Error::Execution(
                "synthetic provisioning failure".into(),
            ),
        ))
    }
}

struct SettlingRejectingResolver {
    worker: Arc<MemWorker>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

struct ExhaustionResolver {
    terminal_worker: Arc<MemWorker>,
    ordinary_calls: Arc<std::sync::atomic::AtomicUsize>,
    terminal_calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl WorkerResolver<MemoryDispatchStore> for ExhaustionResolver {
    async fn worker_for(
        &self,
        _thread_id: &ThreadId,
        _agent_id: Option<&str>,
    ) -> Result<Arc<MemWorker>, Error> {
        self.ordinary_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(Error::Execution(
            awaken_runtime_contract::execution::Error::Execution(
                "retry exhaustion must not resolve an execution worker".into(),
            ),
        ))
    }

    async fn terminalize_retry_exhausted(
        &self,
        claimed: &awaken_run_ingress::Claimed,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        self.terminal_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.terminal_worker
            .terminalize_retry_exhausted(claimed, clock)
            .await
    }
}

#[async_trait]
impl WorkerResolver<MemoryDispatchStore> for SettlingRejectingResolver {
    async fn worker_for(
        &self,
        _thread_id: &ThreadId,
        _agent_id: Option<&str>,
    ) -> Result<Arc<MemWorker>, Error> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(Error::TerminalResolution(
            awaken_runtime_contract::execution::Error::Execution(
                "synthetic deterministic realization failure".into(),
            ),
        ))
    }

    async fn settle_claimed_resolution_failure(
        &self,
        claimed: &awaken_run_ingress::Claimed,
        error: Error,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        self.worker
            .fail_claimed_before_execution(
                claimed,
                "dispatch_resolution_failed",
                error.to_string(),
                clock,
            )
            .await
    }
}

struct FlakyResolver {
    worker: Arc<FlakyWorker>,
}

#[async_trait]
impl WorkerResolver<FlakyDispatchStore> for FlakyResolver {
    async fn worker_for(
        &self,
        _thread_id: &ThreadId,
        _agent_id: Option<&str>,
    ) -> Result<Arc<FlakyWorker>, Error> {
        Ok(self.worker.clone())
    }
}

struct GatedFlakyResolver {
    worker: Option<Arc<FlakyWorker>>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
}

#[async_trait]
impl WorkerResolver<FlakyDispatchStore> for GatedFlakyResolver {
    async fn worker_for(
        &self,
        _thread_id: &ThreadId,
        _agent_id: Option<&str>,
    ) -> Result<Arc<FlakyWorker>, Error> {
        self.entered.notify_one();
        self.release
            .acquire()
            .await
            .expect("resolver release")
            .forget();
        self.worker.clone().ok_or_else(|| {
            Error::Execution(awaken_runtime_contract::execution::Error::Execution(
                "synthetic provisioning failure".into(),
            ))
        })
    }
}

struct BlockingCancelAttemptExecutor {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
}

#[async_trait]
impl awaken_runtime_contract::execution::RunExecutor for BlockingCancelAttemptExecutor {
    async fn execute(
        &self,
        _activation: awaken_runtime_contract::activation::RunActivation,
        _context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        Err(awaken_runtime_contract::execution::Error::Execution(
            "cancellation-only test executor was asked to execute".into(),
        ))
    }
}

#[async_trait]
impl awaken_runtime_contract::execution::RunAttemptExecutor for BlockingCancelAttemptExecutor {
    async fn resume(
        &self,
        _activation: awaken_runtime_contract::activation::RunActivation,
        _command: awaken_runtime_contract::resume::ResumeCommand,
        _context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        Err(awaken_runtime_contract::execution::Error::Execution(
            "cancellation-only test executor was asked to resume".into(),
        ))
    }

    async fn cancel(
        &self,
        _activation: awaken_runtime_contract::activation::RunActivation,
        _context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<()> {
        self.entered.notify_one();
        self.release
            .acquire()
            .await
            .expect("cancellation release")
            .forget();
        Ok(())
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

async fn yield_until(cond: impl Fn() -> bool) -> bool {
    for _ in 0..1_000 {
        if cond() {
            return true;
        }
        tokio::task::yield_now().await;
    }
    cond()
}

#[tokio::test]
async fn idle_pool_uses_one_fallback_poller_independent_of_execution_capacity() {
    // Cause/effect decision table:
    // | Rule | queued work | execution capacity | trigger | queue-claim effect |
    // | P1 | absent | 32 | startup | one recovery poll |
    // | P2 | absent | 32 | one fallback tick | one additional poll |
    // | P3 | present | >1 | successful claim | peer permit scales execution |
    // P3 is covered by the existing two-thread routing test; P1/P2 prevent idle
    // queue traffic from being multiplied by the execution-capacity setting.
    let store = Arc::new(FlakyDispatchStore::new(0));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = Arc::new(DispatchWorker::new(
        text_runtime(),
        store.clone(),
        commit,
        "idle-pool",
    ));
    let interval = Duration::from_secs(1);
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        Arc::new(SystemClock),
        "idle-pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: interval,
            ..Default::default()
        },
        Arc::new(FlakyResolver { worker }),
        32,
        Arc::new(BlackholeWake),
    );

    assert!(wait_for(|| store.claim_attempts() >= 1).await, "P1 poll");
    assert_eq!(store.claim_attempts(), 1, "P1");

    assert!(wait_for(|| store.claim_attempts() >= 2).await, "P2 poll");
    assert_eq!(store.claim_attempts(), 2, "P2");

    pool.shutdown().await;
}

#[tokio::test]
async fn special_claim_error_blocks_the_same_tick_ordinary_claim() {
    // Cause/effect table: B1 special claim errors -> that drain tick returns an
    // error and ordinary claim count stays zero; B2 later special claim succeeds
    // with no exhausted row -> ordinary execution may proceed. This prevents a
    // transient policy/read failure from bypassing retry exhaustion admission.
    let store = Arc::new(FlakyDispatchStore::new(0).with_retry_exhaustion_failures(1));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = Arc::new(DispatchWorker::new(
        text_runtime(),
        store.clone(),
        commit.clone(),
        "pool",
    ));
    store
        .enqueue(RunDispatch::new(activation_on("barrier", "thread-barrier")))
        .await
        .unwrap();
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        Arc::new(ManualClock::new(0)),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_millis(150),
            ..Default::default()
        },
        Arc::new(FlakyResolver { worker }),
        1,
        Arc::new(BlackholeWake),
    );
    assert!(
        wait_for(|| store.retry_exhaustion_claim_attempts() >= 1).await,
        "B1 special attempted"
    );
    assert_eq!(store.claim_attempts(), 0, "B1");
    assert!(wait_for(|| commit.commit_count() >= 1).await, "B2");
    assert!(store.claim_attempts() >= 1, "B2");
    pool.shutdown().await;
}

/// Exact-claim renewal ownership cause/effect decision table. C1 is the Pool
/// claim, C2 is resolver completion, C3 is the exact Worker drive, C4 is
/// cancellation, C5 is resolver failure, and C6 is expiry/replacement. E1 is one
/// renewal write per interval, E2 is guard transfer without a second Tokio task,
/// E3 is guard shutdown, E4 is immediate relinquish, and E5 is fail-closed stale
/// execution.
///
/// | Rule | Resolver | Exact operation | Authority | Expected effect |
/// |---|---|---|---|---|
/// | RG1 | succeeds | normal execute | current | E1+E2, then E3 |
/// | RG2 | succeeds | cancel | current | E1+E2, then E3 |
/// | RG3 | fails | none | current | E1+E4, then E3 |
/// | RG4 | blocked | none | expired/replaced | one failed E1, E3+E5 |
///
/// The count is the observable task cardinality: the pre-fix Pool and Worker
/// tasks both woke on the same interval, producing two renewal writes for RG1
/// and RG2. One transferred guard produces exactly one.
/// Constraint/Invariant: exactly one renewal guard follows the exact claim from
/// Pool through Worker and stops at every terminal/failure exit. Decision rule:
/// this test owns RG1; the adjacent tests own RG2-RG4.
#[tokio::test(start_paused = true)]
async fn pool_transfers_one_renewal_guard_into_a_normal_drive() {
    use std::sync::atomic::Ordering;

    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, ran) = blocking_tool_runtime(release.clone());
    let store = Arc::new(FlakyDispatchStore::new(0));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let lease_ms = 90;
    let worker = Arc::new(
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "pool").with_lease_ms(lease_ms),
    );
    store
        .enqueue(RunDispatch::new(activation("one-renewal-normal")))
        .await
        .unwrap();
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        Arc::new(ManualClock::new(0)),
        "pool",
        lease_ms,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(3_600),
            ..Default::default()
        },
        Arc::new(FlakyResolver { worker }),
        1,
        Arc::new(BlackholeWake),
    );

    assert!(
        yield_until(|| ran.load(Ordering::SeqCst) == 1).await,
        "RG1 drive entered"
    );
    assert_eq!(store.renewal_attempts(), 0, "RG1 before first interval");
    tokio::time::advance(Duration::from_millis(30)).await;
    assert!(
        yield_until(|| store.renewal_attempts() >= 1).await,
        "RG1 first renewal"
    );
    assert_eq!(store.renewal_attempts(), 1, "RG1/E1+E2");
    tokio::time::advance(Duration::from_millis(30)).await;
    assert!(
        yield_until(|| store.renewal_attempts() >= 2).await,
        "RG1 second renewal"
    );
    assert_eq!(store.renewal_attempts(), 2, "RG1 one task per interval");

    release.add_permits(1);
    assert!(
        yield_until(|| commit.commit_count() >= 1).await,
        "RG1 drive settled"
    );
    let settled_count = store.renewal_attempts();
    tokio::time::advance(Duration::from_millis(300)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(store.renewal_attempts(), settled_count, "RG1/E3");
    pool.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn pool_transfers_one_renewal_guard_into_cancellation() {
    // Cause/effect: C1 is an accepted durable cancellation and C2 is a Worker
    // whose terminal effect is blocked. E1 is that admission returns before C2;
    // E2 is one pool-owned renewal guard while the ordinary drainer owns the
    // claim; E3 is no renewal after settlement.
    //
    // | Rule | C1 | C2 | admission | renewal/terminal effect |
    // |---|---|---|---|---|
    // | RG2 | true | blocked | returns true immediately | one guard, then stop |
    // Constraint/Invariant: accepting cancellation does not create a synchronous
    // second driver or renewal task. Decision rule: RG2 requires immediate
    // admission, one renewal while blocked, and zero renewal after settlement.
    let store = Arc::new(FlakyDispatchStore::new(0));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let lease_ms = 90;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let executor = Arc::new(BlockingCancelAttemptExecutor {
        entered: entered.clone(),
        release: release.clone(),
    });
    let worker = Arc::new(
        DispatchWorker::new(text_runtime(), store.clone(), commit.clone(), "pool")
            .with_lease_ms(lease_ms),
    );
    worker.install_attempt_executor(executor);
    let pool = DispatchPool::spawn(
        store.clone(),
        Arc::new(ManualClock::new(0)),
        "pool",
        lease_ms,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(3_600),
            ..Default::default()
        },
        Arc::new(FlakyResolver { worker }),
        1,
    );
    store
        .enqueue(RunDispatch::new(activation("one-renewal-cancel")))
        .await
        .unwrap();

    let run_id = RunId("one-renewal-cancel".into());
    assert!(pool.cancel(&run_id).await.expect("RG2 admission"), "RG2/E1");
    entered.notified().await;
    // The cancellation executor can be reached in the same scheduler pass as
    // guard creation; give the transferred renewal task one poll to arm its
    // first interval before advancing virtual time.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_millis(30)).await;
    assert!(
        yield_until(|| store.renewal_attempts() >= 1).await,
        "RG2 first renewal"
    );
    assert_eq!(store.renewal_attempts(), 1, "RG2/E2");

    release.add_permits(1);
    assert!(
        yield_until(|| {
            CommittedThreadView::run(commit.as_ref(), &run_id)
                .is_some_and(|run| matches!(run.state, RunState::Ended(_)))
        })
        .await,
        "RG2 cancellation settles"
    );
    let settled_count = store.renewal_attempts();
    tokio::time::advance(Duration::from_millis(300)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(store.renewal_attempts(), settled_count, "RG2/E3");
    pool.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn pool_stops_its_only_renewal_guard_after_resolution_failure() {
    // Cause/effect rule RG3: C1 the Pool owns one renewal guard while Worker
    // resolution is gated; C2 resolution returns no Worker. Effects: E1 exactly
    // one renewal occurs while blocked; E2 the failed resolution relinquishes
    // the claim and drops that guard; E3 later virtual time adds no renewal.
    // Decision rule RG3=C1+C2=>E1+E2+E3.
    // Constraint/Invariant: resolution failure relinquishes the same exact claim
    // and terminates its sole guard before another Worker may claim it.
    let store = Arc::new(FlakyDispatchStore::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    store
        .enqueue(RunDispatch::new(activation("one-renewal-failure")))
        .await
        .unwrap();
    let entered_wait = entered.notified();
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        Arc::new(ManualClock::new(0)),
        "pool",
        90,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(3_600),
            ..Default::default()
        },
        Arc::new(GatedFlakyResolver {
            worker: None,
            entered: entered.clone(),
            release: release.clone(),
        }),
        1,
        Arc::new(BlackholeWake),
    );
    entered_wait.await;
    tokio::time::advance(Duration::from_millis(30)).await;
    assert!(
        yield_until(|| store.renewal_attempts() >= 1).await,
        "RG3 first renewal"
    );
    assert_eq!(store.renewal_attempts(), 1, "RG3/E1");

    release.add_permits(1);
    assert!(
        yield_until(|| store.claim_attempts() >= 1).await,
        "RG3 resolver returned to drain"
    );
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    let failed_count = store.renewal_attempts();
    tokio::time::advance(Duration::from_millis(300)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(store.renewal_attempts(), failed_count, "RG3/E3+E4");
    pool.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn expired_replaced_claim_stops_the_only_renewal_and_executes_no_effect() {
    // Cause/effect rule RG4: C1 Worker resolution is blocked; C2 the edge clock
    // expires the Pool claim; C3 a peer replaces it before resolution resumes.
    // Effects: E1 the sole guard observes one renewal loss and stops; E2 later
    // time creates no renewal; E3 the stale Worker ownership check commits no
    // effect. Decision rule RG4=C1+C2+C3=>E1+E2+E3.
    // Constraint/Invariant: replaced ownership fails closed before execution and
    // cannot leave a detached renewal task.
    let store = Arc::new(FlakyDispatchStore::new(0));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = Arc::new(
        DispatchWorker::new(text_runtime(), store.clone(), commit.clone(), "pool")
            .with_lease_ms(90),
    );
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let clock = Arc::new(ManualClock::new(0));
    store
        .enqueue(RunDispatch::new(activation("one-renewal-stale")))
        .await
        .unwrap();
    let entered_wait = entered.notified();
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        clock.clone(),
        "pool",
        90,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(3_600),
            ..Default::default()
        },
        Arc::new(GatedFlakyResolver {
            worker: Some(worker),
            entered: entered.clone(),
            release: release.clone(),
        }),
        1,
        Arc::new(BlackholeWake),
    );
    entered_wait.await;
    clock.set(91);
    assert!(
        store
            .claim("replacement", 90, 91, &Default::default())
            .await
            .unwrap()
            .is_some(),
        "RG4 claim replaced after expiry"
    );
    tokio::time::advance(Duration::from_millis(30)).await;
    assert!(
        yield_until(|| store.renewal_attempts() >= 1).await,
        "RG4 failed renewal observed"
    );
    assert_eq!(store.renewal_attempts(), 1, "RG4 one task");
    tokio::time::advance(Duration::from_millis(300)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(store.renewal_attempts(), 1, "RG4/E3");

    release.add_permits(1);
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(commit.commit_count(), 0, "RG4/E5");
    pool.shutdown().await;
}

/// Durable cancellation uses the same exact-claim WorkerResolver as ordinary
/// pool draining; the admitting caller does not reconstruct or synchronously
/// drive a second per-Session worker path.
#[tokio::test]
async fn pool_cancellation_is_admitted_then_drained_by_the_frozen_claim() {
    use awaken_agent_contract::agent::run::EndCause;

    // Test design. Causes: C1 a durable Run is pending; C2 cancellation is
    // admitted; C3 the Pool's ordinary resolver drains its frozen exact claim.
    // Effects: E1 C2 returns without synchronous execution; E2 C3 commits
    // Cancelled and settles once. Constraint/Invariant: cancellation uses the
    // same resolver/queue path as normal work, never a per-Session worker path.
    // Decision rule: exercise C1+C2+C3 and require one terminal commit.
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = worker_over(text_runtime(), store.clone(), commit.clone());
    let resolver = Arc::new(MapResolver {
        workers: HashMap::from([("cancel-thread".to_string(), worker)]),
    });
    let pool = DispatchPool::spawn(
        store.clone(),
        Arc::new(SystemClock),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(3600),
            ..Default::default()
        },
        resolver,
        1,
    );

    // Cause/effect: the request only admits a durable intent; the already-owned
    // drainer later resolves the frozen activation and commits the terminal fact.
    // Missing rows fail closed without creating work.
    //
    // | Rule | dispatch row | admission | eventual pool effect |
    // |---|---|---|---|
    // | C1 | absent | false | absent |
    // | C2 | queued/frozen activation | true | exactly one Cancelled + settle |
    assert!(!pool.cancel(&RunId("unknown".into())).await.unwrap(), "C1");
    store
        .enqueue(RunDispatch::new(activation_on(
            "cold-cancel",
            "cancel-thread",
        )))
        .await
        .unwrap();

    assert!(
        pool.cancel(&RunId("cold-cancel".into())).await.unwrap(),
        "C2"
    );
    assert!(
        yield_until(|| store.dispatch_count() == 0).await,
        "C2 pool settlement"
    );
    assert_eq!(
        CommittedThreadView::run(commit.as_ref(), &RunId("cold-cancel".into()))
            .expect("C2 terminal")
            .state,
        RunState::Ended(EndCause::Cancelled),
        "C2"
    );
    assert_eq!(store.dispatch_count(), 0, "C2 settled");
    pool.shutdown().await;
}

/// A resolver that delegates to a real worker map but RECORDS the `agent_id` the
/// pool forwarded for each claimed run — the seam a cold worker needs so it opens the
/// session bound to the run's own agent (its published config) rather than the host
/// default.
struct RecordingResolver {
    inner: MapResolver,
    seen: Arc<std::sync::Mutex<Vec<Option<String>>>>,
}

#[async_trait]
impl WorkerResolver<MemoryDispatchStore> for RecordingResolver {
    async fn worker_for(
        &self,
        thread_id: &ThreadId,
        agent_id: Option<&str>,
    ) -> Result<Arc<MemWorker>, Error> {
        self.seen.lock().unwrap().push(agent_id.map(str::to_string));
        self.inner.worker_for(thread_id, agent_id).await
    }
}

/// The pool must hand a claimed run's OWN agent identity (its activation snapshot's
/// `root_agent_id`) to `worker_for`, so a cold worker opens the session bound to that
/// agent's published config instead of the host default. The harness snapshot's
/// `root_agent_id = "agent-1"`.
#[tokio::test]
async fn the_pool_forwards_a_claimed_runs_agent_to_the_resolver() {
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = worker_over(text_runtime(), store.clone(), commit.clone());
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let resolver = Arc::new(RecordingResolver {
        inner: MapResolver {
            workers: HashMap::from([("thread-m".to_string(), worker)]),
        },
        seen: seen.clone(),
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

    pool.submit(activation_on("run-m", "thread-m"))
        .await
        .unwrap();
    assert!(
        wait_for(|| commit.commit_count() >= 1).await,
        "the run drained"
    );
    pool.shutdown().await;

    // The pool extracted the activation snapshot's root_agent_id ("agent-1") and
    // forwarded it, so the worker opens the session bound to that agent.
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter().any(|a| a.as_deref() == Some("agent-1")),
        "worker_for received the run's agent id; got {seen:?}"
    );
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
    assert!(CommittedThreadView::run(&*commit_a, &RunId("run-a".into())).is_some());
    assert!(CommittedThreadView::run(&*commit_a, &RunId("run-b".into())).is_none());
    assert!(CommittedThreadView::run(&*commit_b, &RunId("run-b".into())).is_some());
    assert!(CommittedThreadView::run(&*commit_b, &RunId("run-a".into())).is_none());

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

/// Metamorphic (S6): a wake hint is NON-authoritative — losing it must not change
/// the committed outcome, only defer the drain to the poll fallback. Metamorphic
/// relation: `drain(wake delivered) ≡ drain(wake lost)` in committed truth. Here the
/// wake NEVER delivers (publish is a no-op, wait blocks forever), simulating total
/// wake loss across a node fleet; the poll fallback alone must still claim, drive,
/// and commit the run to the SAME terminal state the wake-driven path reaches.
#[tokio::test]
async fn a_lost_wake_still_drains_through_the_poll_fallback_to_the_same_outcome() {
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = worker_over(text_runtime(), store.clone(), commit.clone());
    let resolver = Arc::new(MapResolver {
        workers: HashMap::from([(harness::THREAD.to_string(), worker)]),
    });

    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        Arc::new(SystemClock),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            // A short poll so the fallback fires quickly — the wake never will.
            poll_interval: Duration::from_millis(20),
            ..Default::default()
        },
        resolver,
        1,
        Arc::new(BlackholeWake) as Arc<dyn WakeSignal>,
    );

    pool.submit(activation("run-1")).await.unwrap();

    // Same committed outcome as the wake-driven path: the run drains to a terminal
    // commit and its dispatch row is gone — correctness never depended on the hint.
    assert!(
        wait_for(|| commit.commit_count() >= 1).await,
        "a lost wake still drains via the poll fallback"
    );
    assert!(
        wait_for(|| store.dispatch_count() == 0).await,
        "the poll-drained run settled and was removed, same as with a wake"
    );
    assert_eq!(
        commit.committed().messages.last().unwrap().text_content(),
        "done",
        "the committed reply is identical to the wake-driven outcome",
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
        settled: Mutex<Vec<(String, RunState)>>,
    }
    impl CompletionSink for RecordingSink {
        fn settled(&self, run_id: &RunId, state: &RunState) {
            self.settled
                .lock()
                .unwrap()
                .push((run_id.0.clone(), state.clone()));
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

    // The sink is notified with the run and its settled (Ended) state.
    assert!(
        wait_for(|| !sink.settled.lock().unwrap().is_empty()).await,
        "the pool signalled completion"
    );
    let recorded = sink.settled.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].0, "run-1");
    assert!(
        matches!(recorded[0].1, RunState::Ended(_)),
        "signalled the settled Ended state, got {:?}",
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
        .enqueue(RunDispatch::new(activation("run-1")))
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
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    assert!(
        store
            .claim("dead-worker", 1_000, 0, &Default::default())
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
    // Test design. Causes: C1 a cross-Thread message is staged; C2 the Pool
    // maintenance loop polls at least once. Effects: E1 C2 relays it into target
    // pending input; E2 repeated polls do not duplicate the stable message id.
    // Constraint/Invariant: the Outbox row is the sole relay authority. Decision rule:
    // wait through multiple polls and require one pending delivery.
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
        result: ResumeResult::allow(),
        context_messages: Vec::new(),
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
/// tick fails, and the task logs-and-backs-off rather than dying. The exact claim is
/// relinquished to the tail without spending crash budget, and the pool stays live —
/// a later run on a mapped thread still drains.
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
    // errors. The claim is real, but admission rollback returns it to Pending and
    // rotates it behind later work rather than preserving a dead lease.
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
    assert!(
        claimed,
        "the orphan run remains pending for a later compatible runtime"
    );
    assert_eq!(
        commit.commit_count(),
        0,
        "nothing was driven for the orphan"
    );

    // The drain did NOT die: a run on a mapped thread still drains.
    pool.submit(activation_on("good-run", "thread-good"))
        .await
        .unwrap();
    assert!(
        wait_for(|| commit.commit_count() >= 1).await,
        "the drain survived the resolver error and drove a later run"
    );

    // The orphan dispatch is still present and un-settled (left for later admission).
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
    // The drive committed a mid-flight `Running` step but has NOT reached an end.
    let run = RunId("run-1".to_string());
    assert!(
        matches!(
            CommittedThreadView::run(&*commit, &run).map(|r| r.state),
            Some(RunState::Running)
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
            CommittedThreadView::run(&*commit, &run).map(|r| r.state),
            Some(RunState::Ended(_))
        ),
        "the in-flight run was driven to completion, not dropped by shutdown"
    );
}

/// The completion sink is signalled on EVERY settle — including a `Awaiting` re-await.
/// A run that awaits on an awaiting ticket settles `Awaiting`, and the sink must be
/// notified with the `Awaiting` state (not only on a terminal `Ended`).
#[tokio::test]
async fn completion_sink_is_signalled_for_an_awaiting_run() {
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingSink {
        settled: Mutex<Vec<(String, RunState)>>,
    }
    impl CompletionSink for RecordingSink {
        fn settled(&self, run_id: &RunId, state: &RunState) {
            self.settled
                .lock()
                .unwrap()
                .push((run_id.0.clone(), state.clone()));
        }
    }

    let (runtime, _ran) = tool_runtime(); // awaits on the suspend gate
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
        "the pool signalled completion for the awaiting run"
    );
    let recorded = sink.settled.lock().unwrap().clone();
    assert_eq!(recorded[0].0, "run-1");
    assert!(
        matches!(recorded[0].1, RunState::Awaiting),
        "signalled the Awaiting state on an await, got {:?}",
        recorded[0].1
    );

    pool.shutdown().await;
}

/// Claim renewal cause/effect graph and FMECA. Causes: C1 a claimed Run is still
/// driving; C2 execution exceeds the base lease; C3 a peer tries to recover it.
/// Effects: E1 the exact claim is renewed; E2 the peer cannot reclaim; E3 renewal
/// stops when drive settles. Decision rule R1=C1+C2+C3=>E1+E2, then !C1=>E3.
/// Failure mode: caller-owned renewal omitted foreground child Runs and could
/// duplicate effects after lease expiry (critical); the worker-owned guard makes
/// every pool/service/foreground drive follow this one rule.
#[tokio::test]
async fn every_drive_renews_its_exact_claim_until_settlement() {
    use std::sync::atomic::Ordering;

    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, ran) = blocking_tool_runtime(release.clone());
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = Arc::new(
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "pool").with_lease_ms(100),
    );
    let resolver = Arc::new(MapResolver {
        workers: HashMap::from([(harness::THREAD.to_string(), worker)]),
    });
    let clock = Arc::new(SystemClock);
    // A short lease proves the worker derives a safe renewal cadence for the
    // exact drive; the pool owns no parallel heartbeat.
    let lease_ms = 100;
    let pool = DispatchPool::spawn(
        store.clone(),
        clock.clone(),
        "pool",
        lease_ms,
        DispatchServiceConfig {
            poll_interval: Duration::from_millis(10),
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
            store
                .claim("thief", lease_ms, now, &Default::default())
                .await
                .unwrap()
                .is_none(),
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

/// Resolution is part of the claimed effect even though the concrete Worker
/// does not exist yet. Slow sandbox/credential preparation must renew the claim.
#[tokio::test]
async fn pool_renews_claim_while_worker_resolution_is_blocked() {
    struct SlowResolver {
        worker: Arc<MemWorker>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Semaphore>,
    }

    #[async_trait]
    impl WorkerResolver<MemoryDispatchStore> for SlowResolver {
        async fn worker_for(
            &self,
            _thread_id: &ThreadId,
            _agent_id: Option<&str>,
        ) -> Result<Arc<MemWorker>, Error> {
            self.entered.notify_one();
            self.release
                .acquire()
                .await
                .expect("resolution release")
                .forget();
            Ok(self.worker.clone())
        }
    }

    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let lease_ms = 90;
    let worker = Arc::new(
        DispatchWorker::new(text_runtime(), store.clone(), commit.clone(), "pool")
            .with_lease_ms(lease_ms),
    );
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let clock = Arc::new(SystemClock);
    let pool = DispatchPool::spawn(
        store.clone(),
        clock.clone(),
        "pool",
        lease_ms,
        DispatchServiceConfig {
            poll_interval: Duration::from_millis(10),
            ..Default::default()
        },
        Arc::new(SlowResolver {
            worker,
            entered: entered.clone(),
            release: release.clone(),
        }),
        1,
    );

    pool.submit(activation("slow-resolution")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("resolver entered");
    tokio::time::sleep(Duration::from_millis(lease_ms * 3)).await;
    assert!(
        store
            .claim("thief", lease_ms, clock.now_ms(), &Default::default())
            .await
            .unwrap()
            .is_none(),
        "R1: resolution renews the exact claim beyond its base lease"
    );

    release.add_permits(1);
    assert!(
        wait_for(|| commit.commit_count() >= 1).await,
        "R2: the original claim executes after resolution"
    );
    pool.shutdown().await;
}

#[tokio::test]
async fn renewal_stops_when_claim_resolution_fails() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Cause/effect decision table:
    // | Rule | claim | resolver/drive | local activity | renewal/recovery effect |
    // | R1 | live | blocked | present | exact lease renews (covered above) |
    // | R2 | live | fails | removed | exact claim is relinquished; peer claims |
    // | R3 | absent | n/a | absent | no lease write (idle-poller test) |
    // R2 drops the exact guard instead of indefinitely preserving an unsettled
    // claim after provisioning or Runtime construction has failed; relinquish
    // also avoids waiting a full lease or charging crash recovery.
    let store = Arc::new(MemoryDispatchStore::new());
    store
        .enqueue(RunDispatch::new(activation("resolver-failure")))
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let lease_ms = 100;
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        Arc::new(SystemClock),
        "failed-owner",
        lease_ms,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(3600),
            ..Default::default()
        },
        Arc::new(RejectingResolver {
            calls: calls.clone(),
        }),
        1,
        Arc::new(BlackholeWake),
    );

    assert!(
        wait_for(|| calls.load(Ordering::SeqCst) == 1).await,
        "R2 resolver failure observed"
    );
    let reclaimed = store
        .claim(
            "recovery-owner",
            lease_ms,
            SystemClock.now_ms(),
            &Default::default(),
        )
        .await
        .unwrap();
    let reclaimed = reclaimed.expect("R2 failed claim is immediately available");
    assert!(
        !reclaimed.recovered,
        "R2 is admission rollback, not a crash"
    );

    pool.shutdown().await;
}

#[tokio::test]
async fn session_work_backpressure_relinquishes_once_then_retries_with_a_floor() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Cause/effect graph: C1 a valid Run claim reaches a Session whose one
    // Environment Work slot is occupied; C2 the resolver returns typed NotReady;
    // C3 queue polling is configured below the admission floor. C1+C2 -> E1
    // relinquish without crash accounting; C2+C3 -> E2 no warning-speed hot
    // reacquisition. Once the slot changes, the same durable row remains eligible.
    //
    // | Rule | resolver | poll | first 100 ms effect | durable row effect |
    // | B1 | NotReady | 10 ms | one resolution attempt | immediately reclaimable |
    // | B2 | fault | any | warning/backoff path | covered by R2 above |
    //
    // FMECA: treating expected Environment serialization as a fault produced
    // ~20 claim/relinquish cycles per second, inflated lease epochs, and hid real
    // Worker failures. Holding the lease instead would starve the rightful next
    // owner, so the canonical pool relinquishes once and rate-limits retry.
    let store = Arc::new(MemoryDispatchStore::new());
    store
        .enqueue(RunDispatch::new(activation("session-work-pressure")))
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        Arc::new(SystemClock),
        "waiting-owner",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_millis(10),
            ..Default::default()
        },
        Arc::new(NotReadyResolver {
            calls: calls.clone(),
        }),
        1,
        Arc::new(BlackholeWake),
    );

    assert!(
        wait_for(|| calls.load(Ordering::SeqCst) == 1).await,
        "B1/E1"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "B1/E2");
    assert!(
        store
            .claim(
                "ready-owner",
                DEFAULT_LEASE_MS,
                SystemClock.now_ms(),
                &Default::default(),
            )
            .await
            .unwrap()
            .is_some(),
        "B1 remains durable and immediately reclaimable"
    );
    pool.shutdown().await;
}

#[tokio::test]
async fn returned_resolution_failure_commits_and_settles_when_resolver_owns_that_capability() {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Cause/effect graph: C1=the process crashes while resolving (no returned
    // value); C2=Session realization returns a typed terminal error after its
    // application failure commit; C3=the resolver owns
    // a claim-fenced commit adapter; C4=the exact claim is still current;
    // C5=commit/settle authority was replaced; C6=an ordinary retryable resolver
    // error is returned. E1=lease expiry/reclaim handles
    // recovery; E2=one Error Run is committed; E3=the dispatch settles Done and
    // foreground completion wakes; E4=fencing rejects stale effects. Constraints:
    // C1 and C2 are exclusive; E2/E3 require C2+C3+C4. Decision table:
    // | Rule | C1 | C2 | C3 | C4 | C5 | C6 | Effect |
    // | F1   | T  | F  | -  | -  | -  | F  | E1     |
    // | F2   | F  | F  | -  | -  | -  | T  | E1     |
    // | F3   | F  | T  | T  | T  | F  | F  | E2,E3  |
    // | F4   | F  | T  | T  | F  | T  | F  | E4     |
    // F1/F2 are covered by `renewal_stops_when_claim_resolution_fails`; claim
    // replacement fencing is covered by the worker/store fencing suites. This
    // case proves F3 and prevents a deterministic provisioning error from being
    // misclassified as a crash that leaves the foreground request waiting.
    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<(String, RunState)>>);
    impl CompletionSink for RecordingSink {
        fn settled(&self, run_id: &RunId, state: &RunState) {
            self.0
                .lock()
                .unwrap()
                .push((run_id.0.clone(), state.clone()));
        }
    }

    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = worker_over(text_runtime(), store.clone(), commit.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let sink = Arc::new(RecordingSink::default());
    let pool = DispatchPool::spawn_with_completion(
        store.clone(),
        Arc::new(SystemClock),
        "settling-owner",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig::default(),
        Arc::new(SettlingRejectingResolver {
            worker,
            calls: calls.clone(),
        }),
        1,
        sink.clone(),
    );

    pool.submit(activation("resolution-failure")).await.unwrap();
    assert!(
        wait_for(|| !sink.0.lock().unwrap().is_empty()).await,
        "F3 foreground completion is notified"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "F3 resolves once");
    assert_eq!(store.dispatch_count(), 0, "F3 dispatch settled Done");
    let state = commit
        .run_state(&RunId("resolution-failure".into()))
        .expect("F3 terminal Run commit");
    assert!(
        matches!(
            state,
            RunState::Ended(awaken_agent_contract::agent::run::EndCause::Error(_))
        ),
        "F3 deterministic resolution error is committed as a Run failure"
    );

    pool.shutdown().await;
}

/// Pool drain cause/effect table: P1 expired+exhausted -> special claim; P2 the
/// special claim -> canonical boundary Worker terminal commit and Done settle;
/// P3 this path -> ordinary execution resolution is never invoked; P4 automatic
/// terminalization -> no manual dead-letter quarantine.
#[tokio::test]
async fn drain_terminalizes_retry_exhaustion_without_execution_resolution() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    store
        .enqueue(RunDispatch::new(activation_on("poison", "thread-poison")))
        .await
        .unwrap();
    assert!(
        store
            .claim("failed-worker", 1, 0, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .claim("failed-worker", 1, 2, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .claim("failed-worker", 1, 4, &Default::default())
            .await
            .unwrap()
            .is_some()
    );

    let ordinary_calls = Arc::new(AtomicUsize::new(0));
    let terminal_calls = Arc::new(AtomicUsize::new(0));
    let terminal_worker = worker_over(text_runtime(), store.clone(), commit.clone());
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        Arc::new(ManualClock::new(6)),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(60),
            max_attempts: 2,
            ..Default::default()
        },
        Arc::new(ExhaustionResolver {
            terminal_worker,
            ordinary_calls: ordinary_calls.clone(),
            terminal_calls: terminal_calls.clone(),
        }),
        1,
        Arc::new(BlackholeWake),
    );

    assert!(
        wait_for(|| commit.run_state(&RunId("poison".into()))
            == Some(RunState::Ended(
                awaken_agent_contract::agent::run::EndCause::Indeterminate
            )))
        .await,
        "P1/P2"
    );
    assert_eq!(store.dispatch_count(), 0, "P2");
    assert_eq!(ordinary_calls.load(Ordering::SeqCst), 0, "P3");
    assert_eq!(terminal_calls.load(Ordering::SeqCst), 1, "P2");
    assert!(store.dead_letters().await.unwrap().is_empty(), "P4");
    pool.shutdown().await;
}

/// Drainer-ownership decision table: M1 coordinator maintenance + no drainer ->
/// preserve the expired row and create neither Run truth nor quarantine; M2 a
/// Worker drainer returns -> its first special-first tick commits Indeterminate
/// and Done; M3 throughout -> no ordinary execution resolution.
#[tokio::test]
async fn coordinator_maintenance_preserves_exhaustion_until_a_drainer_returns() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    store
        .enqueue(RunDispatch::new(activation_on("poison", "thread-poison")))
        .await
        .unwrap();
    assert!(
        store
            .claim("dead", 1, 0, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .claim("dead", 1, 2, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    let ordinary_calls = Arc::new(AtomicUsize::new(0));
    let terminal_calls = Arc::new(AtomicUsize::new(0));
    let maintenance = DispatchMaintenance::spawn(
        store.clone(),
        Arc::new(ManualClock::new(4)),
        Arc::new(BlackholeWake),
        DispatchServiceConfig {
            poll_interval: Duration::from_millis(5),
            max_attempts: 1,
            ..Default::default()
        },
        Arc::new(ExhaustionResolver {
            terminal_worker: worker_over(text_runtime(), store.clone(), commit.clone()),
            ordinary_calls: ordinary_calls.clone(),
            terminal_calls: terminal_calls.clone(),
        }),
        None,
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(commit.run_state(&RunId("poison".into())).is_none(), "M1");
    assert_eq!(store.dispatch_count(), 1, "M1");
    assert_eq!(ordinary_calls.load(Ordering::SeqCst), 0, "M1/M3");
    assert_eq!(terminal_calls.load(Ordering::SeqCst), 0, "M1");
    assert!(store.dead_letters().await.unwrap().is_empty(), "M1");
    maintenance.shutdown().await;

    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        Arc::new(ManualClock::new(4)),
        "remote-worker",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(60),
            max_attempts: 1,
            ..Default::default()
        },
        Arc::new(ExhaustionResolver {
            terminal_worker: worker_over(text_runtime(), store.clone(), commit.clone()),
            ordinary_calls: ordinary_calls.clone(),
            terminal_calls: terminal_calls.clone(),
        }),
        1,
        Arc::new(BlackholeWake),
    );
    assert!(
        wait_for(|| commit.run_state(&RunId("poison".into()))
            == Some(RunState::Ended(
                awaken_agent_contract::agent::run::EndCause::Indeterminate
            )))
        .await,
        "M2"
    );
    assert_eq!(ordinary_calls.load(Ordering::SeqCst), 0, "M3");
    assert_eq!(terminal_calls.load(Ordering::SeqCst), 1, "M2");
    assert!(store.dead_letters().await.unwrap().is_empty(), "M3");
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

/// Graceful drain (P2): `begin_drain` stops the pool CLAIMING new work without
/// consuming the pool. A run submitted after the drain begins is never claimed — it
/// stays queued — so the worker can be scaled in while its in-flight runs finish.
#[tokio::test]
async fn begin_drain_stops_claiming_new_work() {
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = worker_over(text_runtime(), store.clone(), commit.clone());
    let resolver = Arc::new(MapResolver {
        workers: HashMap::from([("thread-d".to_string(), worker)]),
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

    // A run submitted while the pool is live drains normally.
    pool.submit(activation_on("run-live", "thread-d"))
        .await
        .unwrap();
    assert!(
        wait_for(|| commit.commit_count() >= 1).await,
        "the pre-drain run drained"
    );

    // Begin draining: the pool reports draining and its claim loops stop.
    assert!(!pool.is_draining(), "not draining before the request");
    pool.begin_drain().await;
    assert!(pool.is_draining(), "draining after begin_drain");

    // A run submitted AFTER the drain is never claimed — it sits in the queue.
    pool.submit(activation_on("run-after", "thread-d"))
        .await
        .unwrap();
    // Give the (now-stopped) drain loops ample time to (not) pick it up.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let summaries = store.list_dispatches().await.unwrap();
    assert!(
        summaries.iter().any(|s| s.run_id.0 == "run-after"
            && matches!(s.state, awaken_run_ingress::DispatchState::Pending)),
        "the post-drain run stays PENDING (never claimed); got {summaries:?}"
    );

    pool.shutdown().await;
}
