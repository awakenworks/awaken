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
use awaken_dispatch::{
    Clock, CompletionSink, DEFAULT_LEASE_MS, DispatchError, DispatchPool, DispatchQueue,
    DispatchReconciler, DispatchServiceConfig, DispatchSettlementError, DispatchSettlementObserver,
    DispatchWorker, Error, Inbox, LocalWakeSignal, ManualClock, MemoryDispatchStore, PendingInput,
    RunClaim, RunDispatch, SessionChildAdmission, SystemClock, WakeSignal, WorkerResolver,
};
use awaken_dispatch_testkit::ConformanceClock as _;
use awaken_runtime::Runtime;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_store_inmem::MemoryCommitCoordinator;

use harness::{
    FlakyDispatchStore, activation, activation_on, blocking_tool_runtime,
    blocking_tool_runtime_with_entry_signal, concurrency_tracking_tool_runtime,
    counting_text_runtime, multi_tool_blocking_runtime, text_runtime, tool_runtime,
};

type MemWorker = DispatchWorker<MemoryDispatchStore>;
type FlakyWorker = DispatchWorker<FlakyDispatchStore>;

struct DetachedInference {
    return_unknown_error: bool,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
    calls: std::sync::atomic::AtomicUsize,
    live: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl awaken_runtime_contract::llm::LlmExecutor for DetachedInference {
    async fn infer(
        &self,
        _: awaken_runtime_contract::llm::ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<awaken_runtime_contract::llm::ChatResponse> {
        use std::sync::atomic::Ordering::SeqCst;
        self.calls.fetch_add(1, SeqCst);
        let release = self.release.clone();
        let live = self.live.clone();
        let entered = self.entered.clone();
        // This independent test task models a provider that continues computing
        // after its HTTP client Future is dropped. The test explicitly releases
        // it; dropping a JoinHandle deliberately does not cancel remote work.
        let remote = tokio::spawn(async move {
            live.fetch_add(1, SeqCst);
            entered.notify_one();
            release.acquire().await.unwrap().forget();
            live.fetch_sub(1, SeqCst);
        });
        if self.return_unknown_error {
            tokio::time::timeout(Duration::from_millis(5), remote)
                .await
                .map_err(|_| {
                    awaken_runtime_contract::llm::Error::Timeout(
                        "adapter deadline before remote termination".into(),
                    )
                })?
                .unwrap();
        } else {
            remote.await.unwrap();
        }
        Ok(awaken_runtime_contract::llm::ChatResponse {
            output: awaken_runtime_contract::llm::AssistantOutput::text("late response"),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn native_watchdog_retains_slot_while_detached_provider_is_still_running() {
    assert_native_uncertainty_retains_physical_slot(false).await;
}

#[tokio::test]
async fn native_returned_error_retains_slot_while_detached_provider_is_still_running() {
    assert_native_uncertainty_retains_physical_slot(true).await;
}

async fn assert_native_uncertainty_retains_physical_slot(return_unknown_error: bool) {
    // Q1 Native model entered; Q2 remote execution outlives local watchdog;
    // Q2b adapter's own deadline returns Err normally while remote work lives.
    // Q3 inner retry and outer tolerance enabled; Q4 new owner reclaims lease.
    // E1 exactly one physical provider invocation; E2 durable slot retained;
    // E3 replacement cannot begin, even after logical Indeterminate commits.
    // Rules: Q1+(Q2|Q2b)+Q3=>E1/E2; Q4+E2=>E3. Unlike a blocking custom Attempt,
    // this crosses the actual Native inference/timeout/journal/Worker seam.
    use std::sync::atomic::Ordering::SeqCst;
    let model = Arc::new(DetachedInference {
        return_unknown_error,
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Semaphore::new(0)),
        calls: std::sync::atomic::AtomicUsize::new(0),
        live: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });
    let runtime = Arc::new(
        Runtime::new()
            .with_llm(model.clone())
            .with_max_consecutive_inference_failures(3)
            .with_retry_policy(awaken_runtime::LlmRetryPolicy {
                attempt_timeout: Duration::from_millis(30),
                max_retries: 3,
                ..Default::default()
            }),
    );
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = Arc::new(MemoryDispatchStore::new(clock.clock()));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "owner-a").with_lease_ms(90);
    store
        .enqueue(RunDispatch::new(activation("native-unconfirmed")))
        .await
        .unwrap();
    clock.set(0);
    let claim = store
        .claim("owner-a", 90, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let driven = tokio::spawn(async move {
        worker
            .drive_claimed(claim, Arc::new(ManualClock::new(0)))
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), model.entered.notified())
        .await
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(2), driven)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome, None,
        "physical exclusion prevents final Dispatch settlement"
    );
    assert_eq!(
        commit.committed().latest_run.unwrap().state,
        awaken_agent_contract::agent::run::RunState::Ended(
            awaken_agent_contract::agent::run::EndCause::Indeterminate
        ),
        "logical uncertainty is committed while external work remains live"
    );
    assert_eq!(model.calls.load(SeqCst), 1);
    assert_eq!(
        model.live.load(SeqCst),
        1,
        "provider is still executing, not merely a retained claim"
    );
    assert!(store.list_dispatches().await.unwrap()[0].physical_attempt_active);
    clock.set(91);
    let replacement = store
        .claim("owner-b", 90, &Default::default())
        .await
        .unwrap()
        .unwrap();
    clock.set(91);
    assert_eq!(
        store
            .begin_attempt(&RunClaim::from(&replacement.lease), None)
            .await
            .unwrap(),
        awaken_dispatch::AttemptAdmission::Blocked
    );
    model.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), async {
        while model.live.load(SeqCst) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        store.list_dispatches().await.unwrap()[0].physical_attempt_active,
        "test-observed provider completion is not an authenticated recovery receipt"
    );
}

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

#[derive(Default)]
struct ObservedWake {
    local: LocalWakeSignal,
    waiting: tokio::sync::Notify,
}

#[async_trait]
impl WakeSignal for ObservedWake {
    async fn publish(&self) -> Result<(), DispatchError> {
        self.local.publish().await
    }
    async fn wait(&self) {
        self.waiting.notify_one();
        self.local.wait().await;
    }
}

#[tokio::test(start_paused = true)]
async fn standalone_reconciler_consumes_hints_and_recovers_lost_hints_by_polling() {
    // Causes W1 coordinator without drainers; W2 startup before input;
    // W3 delivered/duplicate/missing hint; W4 60s fallback poll.
    // Effects: W1+W2 consumes the external port; W3 delivered => relay before
    // W4; duplicates => one pending record; missing => relay at W4, not loss.
    // Virtual time and a wait-entry barrier distinguish real wiring from a
    // test that merely succeeds at the fallback timer.
    use awaken_dispatch::Outbox;
    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let wake = Arc::new(ObservedWake::default());
    let reconciler = DispatchReconciler::spawn(
        store.clone(),
        clock,
        wake.clone(),
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(60),
            terminal_reconciliation_interval: None,
            ..Default::default()
        },
        Arc::new(MapResolver {
            workers: HashMap::new(),
        }),
        None,
    );
    tokio::time::timeout(Duration::from_millis(1), wake.waiting.notified())
        .await
        .expect("W1 consumes the external wake port");
    let input = harness::pending("wake-1", "wake-run", harness::TICKET, ResumeResult::allow());
    store.stage(input.clone()).await.unwrap();
    wake.publish().await.unwrap();
    tokio::time::timeout(Duration::from_millis(1), wake.waiting.notified())
        .await
        .expect("W3 relay before fallback");
    assert_eq!(store.list(&input.thread_id).await.unwrap().len(), 1);
    wake.publish().await.unwrap();
    wake.publish().await.unwrap();
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(
        store.list(&input.thread_id).await.unwrap().len(),
        1,
        "duplicate hint"
    );
    store
        .stage(harness::pending(
            "wake-2",
            "wake-run",
            harness::TICKET,
            ResumeResult::allow(),
        ))
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        store.list(&input.thread_id).await.unwrap().len(),
        2,
        "W4 lost hint"
    );
    reconciler.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn dispatch_shutdown_does_not_depend_on_non_authoritative_publication() {
    // Causes S1 Service/Pool/standalone Reconciler; S2 publish never returns;
    // S3 no in-flight mutation. Effect S1+S2+S3 => local cancellation and join
    // finish, with zero publish calls. Pool begin_drain obeys the same rule.
    // This does not assert that in-flight storage operations are preemptible.
    struct UnavailableWake(std::sync::atomic::AtomicUsize);
    #[async_trait]
    impl WakeSignal for UnavailableWake {
        async fn publish(&self) -> Result<(), DispatchError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::pending().await
        }
        async fn wait(&self) {
            std::future::pending().await
        }
    }
    for topology in 0..3 {
        let clock = Arc::new(ManualClock::new(0));
        let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
        let wake = Arc::new(UnavailableWake(std::sync::atomic::AtomicUsize::new(0)));
        let config = DispatchServiceConfig::default();
        let resolver = Arc::new(MapResolver {
            workers: HashMap::new(),
        });
        tokio::time::timeout(Duration::from_millis(1), async {
            match topology {
                0 => {
                    awaken_dispatch::DispatchService::spawn_with_wake(
                        worker_over(
                            text_runtime(),
                            store,
                            Arc::new(MemoryCommitCoordinator::new()),
                        ),
                        clock,
                        config,
                        wake.clone(),
                    )
                    .shutdown()
                    .await
                }
                1 => {
                    let pool = DispatchPool::spawn_with_wake(
                        store,
                        clock,
                        "pool",
                        DEFAULT_LEASE_MS,
                        config,
                        resolver,
                        1,
                        wake.clone(),
                    );
                    let _local_fence = pool.begin_drain().await;
                    pool.shutdown().await;
                }
                _ => {
                    DispatchReconciler::spawn(store, clock, wake.clone(), config, resolver, None)
                        .shutdown()
                        .await
                }
            }
        })
        .await
        .expect("S1/S2/S3 local shutdown must not await network");
        assert_eq!(wake.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
    // S4 relay has durably completed, but its best-effort notification hangs.
    // E4 cancellation may abandon that hint (not the preceding store write).
    // The paired in-flight-observation test requires joining authoritative work.
    use awaken_dispatch::Outbox;
    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let input = harness::pending(
        "stop-relay",
        "stop-run",
        harness::TICKET,
        ResumeResult::allow(),
    );
    store.stage(input.clone()).await.unwrap();
    let wake = Arc::new(UnavailableWake(std::sync::atomic::AtomicUsize::new(0)));
    let reconciler = DispatchReconciler::spawn(
        store.clone(),
        clock,
        wake.clone(),
        DispatchServiceConfig::default(),
        Arc::new(MapResolver {
            workers: HashMap::new(),
        }),
        None,
    );
    tokio::task::yield_now().await;
    assert_eq!(wake.0.load(std::sync::atomic::Ordering::SeqCst), 1, "S4");
    tokio::time::timeout(Duration::from_millis(1), reconciler.shutdown())
        .await
        .expect("E4 notification is not a shutdown dependency");
    assert_eq!(
        store.list(&input.thread_id).await.unwrap().len(),
        1,
        "E4 durable input retained"
    );
}

#[tokio::test(start_paused = true)]
async fn service_terminal_cadence_remains_independent_and_optional() {
    // T1 empty service has entered its wake wait; T2 work arrives without a
    // hint; T3 terminal cadence enabled/disabled; T4 queue fallback is 60s.
    // R1 enabled => a 5s terminal tick also revisits ordinary durable work;
    // R2 disabled => no 5s tick, but an explicit hint still drains immediately.
    // Both reject early execution at 4s and use the public Service wiring.
    for interval in [Some(Duration::from_secs(5)), None] {
        let clock = Arc::new(ManualClock::new(0));
        let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let wake = Arc::new(ObservedWake::default());
        let service = awaken_dispatch::DispatchService::spawn_with_wake(
            worker_over(text_runtime(), store.clone(), commit.clone()),
            clock,
            DispatchServiceConfig {
                poll_interval: Duration::from_secs(60),
                terminal_reconciliation_interval: interval,
                ..Default::default()
            },
            wake.clone(),
        );
        tokio::time::timeout(Duration::from_millis(1), wake.waiting.notified())
            .await
            .unwrap();
        store
            .enqueue(RunDispatch::new(activation("cadence-run")))
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(commit.commit_count(), 0, "no early work");
        tokio::time::advance(Duration::from_secs(1)).await;
        if interval.is_none() {
            assert_eq!(commit.commit_count(), 0, "R2 disabled");
            wake.publish().await.unwrap();
        }
        tokio::time::timeout(Duration::from_millis(1), wake.waiting.notified())
            .await
            .expect("R1 timer / R2 explicit hint");
        assert!(commit.commit_count() > 0);
        assert_eq!(store.dispatch_count(), 0);
        service.shutdown().await;
    }
}

#[tokio::test(start_paused = true)]
async fn durable_delivery_returns_when_notification_never_returns() {
    // C1 Service/Pool writes pending input durably; C2 publisher never returns.
    // R1 C1+C2 => acknowledge durable write within the notification budget;
    // one exact input remains, no resend or rollback. The helper's unit matrix
    // covers success/error/cancellation; this crosses both public entry points.
    struct PendingWake;
    #[async_trait]
    impl WakeSignal for PendingWake {
        async fn publish(&self) -> Result<(), DispatchError> {
            std::future::pending().await
        }
        async fn wait(&self) {
            std::future::pending().await
        }
    }
    for pooled in [false, true] {
        let clock = Arc::new(ManualClock::new(0));
        let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
        let input = harness::pending(
            "bounded-delivery",
            "unstarted",
            harness::TICKET,
            ResumeResult::allow(),
        );
        let wake = Arc::new(PendingWake);
        let config = DispatchServiceConfig {
            poll_interval: Duration::from_secs(60),
            terminal_reconciliation_interval: None,
            ..Default::default()
        };
        tokio::time::timeout(Duration::from_millis(101), async {
            if pooled {
                let pool = DispatchPool::spawn_with_wake(
                    store.clone(),
                    clock,
                    "pool",
                    DEFAULT_LEASE_MS,
                    config,
                    Arc::new(MapResolver {
                        workers: HashMap::new(),
                    }),
                    1,
                    wake,
                );
                pool.deliver(input.clone()).await.unwrap();
                pool.shutdown().await;
            } else {
                let service = awaken_dispatch::DispatchService::spawn_with_wake(
                    worker_over(
                        text_runtime(),
                        store.clone(),
                        Arc::new(MemoryCommitCoordinator::new()),
                    ),
                    clock,
                    config,
                    wake,
                );
                service.deliver(input.clone()).await.unwrap();
                service.shutdown().await;
            }
        })
        .await
        .expect("R1 admission is not conditional on notification");
        assert_eq!(store.list(&input.thread_id).await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn service_shutdown_joins_current_attempt_but_does_not_drain_the_backlog() {
    // C1 first attempt blocked inside a real tool; C2 backlog remains runnable;
    // C3 shutdown arrives. R1 C1+C3 => join waits (no false stop evidence).
    // R2 C1 completes+C2+C3 => exit before another tick, retain backlog.
    // This fails the old run_until_idle loop even with a finite backlog.
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, _ran, entered) = blocking_tool_runtime_with_entry_signal(release.clone());
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
    for index in 0..70 {
        store
            .enqueue(RunDispatch::new(activation(&format!("backlog-{index:03}"))))
            .await
            .unwrap();
    }
    let service = awaken_dispatch::DispatchService::spawn(
        worker_over(
            runtime,
            store.clone(),
            Arc::new(MemoryCommitCoordinator::new()),
        ),
        Arc::new(SystemClock),
        DispatchServiceConfig::default(),
    );
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    let stopped = tokio::spawn(service.shutdown());
    tokio::task::yield_now().await;
    assert!(!stopped.is_finished(), "R1 actual attempt still executing");
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(10), stopped)
        .await
        .expect("R2 backlog is not a shutdown dependency")
        .unwrap();
    assert_eq!(
        store.dispatch_count(),
        69,
        "R2 no successor attempt claimed"
    );
}

#[tokio::test(start_paused = true)]
async fn service_full_batch_continues_without_waiting_for_poll_interval() {
    // C1 more ready work than the internal 64-tick batch; C2 60s fallback;
    // C3 no cancellation. R1 C1+C2+C3 => yield then continue draining, not a
    // 60s wait per batch. The paired shutdown test covers cancellation instead.
    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    for index in 0..70 {
        store
            .enqueue(RunDispatch::new(activation(&format!("batch-{index:03}"))))
            .await
            .unwrap();
    }
    let wake = Arc::new(ObservedWake::default());
    let service = awaken_dispatch::DispatchService::spawn_with_wake(
        worker_over(
            text_runtime(),
            store.clone(),
            Arc::new(MemoryCommitCoordinator::new()),
        ),
        clock,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(60),
            terminal_reconciliation_interval: None,
            ..Default::default()
        },
        wake.clone(),
    );
    tokio::time::timeout(Duration::from_secs(1), wake.waiting.notified())
        .await
        .expect("R1 first idle wait is after all batches");
    assert_eq!(
        store.dispatch_count(),
        0,
        "R1 no batch-boundary queue delay"
    );
    service.shutdown().await;
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

struct RecoveringBackpressureResolver {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    worker: Arc<MemWorker>,
}

#[async_trait]
impl WorkerResolver<MemoryDispatchStore> for RecoveringBackpressureResolver {
    async fn worker_for(
        &self,
        _thread_id: &ThreadId,
        _agent_id: Option<&str>,
    ) -> Result<Arc<MemWorker>, Error> {
        match self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
            0 => Err(Error::Execution(
                awaken_runtime_contract::execution::Error::Execution(
                    "synthetic transient resolution fault".into(),
                ),
            )),
            1 => Err(Error::ResolutionNotReady(
                "synthetic Environment pressure".into(),
            )),
            _ => Ok(self.worker.clone()),
        }
    }
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
        claimed: &awaken_dispatch::Claimed,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        self.terminal_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.terminal_worker
            .terminalize_retry_exhausted(claimed)
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
        claimed: &awaken_dispatch::Claimed,
        error: Error,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        self.worker
            .fail_claimed_before_execution(claimed, "dispatch_resolution_failed", error.to_string())
            .await
    }
}

struct FlakyResolver {
    worker: Arc<FlakyWorker>,
}

#[tokio::test(start_paused = true)]
async fn only_the_queue_owner_runs_maintenance_without_disabling_worker_drains() {
    maintenance_observations::check_all().await;
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

struct OwnershipCancellationAttemptExecutor {
    entered: Arc<tokio::sync::Notify>,
    cancellation_observed: Arc<tokio::sync::Notify>,
    cleanup_release: Option<Arc<tokio::sync::Semaphore>>,
}

struct NonQuiescingAttemptExecutor {
    entered: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl awaken_runtime_contract::execution::RunExecutor for NonQuiescingAttemptExecutor {
    async fn execute(
        &self,
        _activation: awaken_runtime_contract::activation::RunActivation,
        _context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        self.entered.notify_one();
        std::future::pending().await
    }
}

#[async_trait]
impl awaken_runtime_contract::execution::RunAttemptExecutor for NonQuiescingAttemptExecutor {
    async fn resume(
        &self,
        _activation: awaken_runtime_contract::activation::RunActivation,
        _command: awaken_runtime_contract::resume::ResumeCommand,
        _context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        Err(awaken_runtime_contract::execution::Error::Execution(
            "non-quiescing fresh-attempt fixture cannot resume".into(),
        ))
    }
}

#[async_trait]
impl awaken_runtime_contract::execution::RunExecutor for OwnershipCancellationAttemptExecutor {
    async fn execute(
        &self,
        _activation: awaken_runtime_contract::activation::RunActivation,
        context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        let cancellation = context
            .cancellation
            .expect("a claimed attempt carries cooperative cancellation");
        self.entered.notify_one();
        cancellation.cancelled().await;
        self.cancellation_observed.notify_one();
        if let Some(release) = &self.cleanup_release {
            release.acquire().await.unwrap().forget();
        }
        Err(awaken_runtime_contract::execution::Error::Execution(
            "exact claim ownership was lost".into(),
        ))
    }
}

#[async_trait]
impl awaken_runtime_contract::execution::RunAttemptExecutor
    for OwnershipCancellationAttemptExecutor
{
    async fn resume(
        &self,
        _activation: awaken_runtime_contract::activation::RunActivation,
        _command: awaken_runtime_contract::resume::ResumeCommand,
        _context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        Err(awaken_runtime_contract::execution::Error::Execution(
            "fresh-attempt cancellation test was asked to resume".into(),
        ))
    }

    async fn cancel(
        &self,
        _activation: awaken_runtime_contract::activation::RunActivation,
        _context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<()> {
        Err(awaken_runtime_contract::execution::Error::Execution(
            "fresh-attempt cancellation test was asked to cancel".into(),
        ))
    }
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
    let store = Arc::new(FlakyDispatchStore::new(0, Arc::new(SystemClock)));
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
async fn settled_predecessor_rearms_a_same_thread_successor_after_a_peer_consumes_the_edge() {
    use std::sync::atomic::Ordering;

    // Cause/effect graph: C1 two durable Runs share one Thread; C2 the first Run
    // is still executing; C3 an idle peer consumes the success handoff and finds
    // C1's successor ineligible; C4 the predecessor settles; C5 no external wake
    // and a one-hour fallback. E1 C2+C3 never runs the successor concurrently;
    // E2 C4 re-arms one local permit and drains it without waiting for C5.
    //
    // | Rule | predecessor | peer edge | predecessor settled | external/poll | Effect |
    // |---|---|---|---|---|---|
    // | S1 | live | consumed | no | none | E1 successor remains queued |
    // | S2 | ended | consumed | yes | none | E2 successor starts promptly |
    // Constraint: the durable store remains the claim authority; the permit is
    // only a liveness hint and cannot bypass same-Thread exclusion.
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, ran) = blocking_tool_runtime(release.clone());
    let store = Arc::new(FlakyDispatchStore::new(0, Arc::new(SystemClock)));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = Arc::new(DispatchWorker::new(
        runtime,
        store.clone(),
        commit.clone(),
        "settlement-rearm",
    ));
    let mut first = activation("settlement-rearm-first");
    first.input[0].id = awaken_agent_contract::agent::message::Id("rearm-m1".into());
    let mut second = activation("settlement-rearm-second");
    second.input[0].id = awaken_agent_contract::agent::message::Id("rearm-m2".into());
    store.enqueue(RunDispatch::new(first)).await.unwrap();
    store.enqueue(RunDispatch::new(second)).await.unwrap();
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        Arc::new(SystemClock),
        "settlement-rearm",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(3_600),
            ..Default::default()
        },
        Arc::new(FlakyResolver { worker }),
        2,
        Arc::new(BlackholeWake),
    );

    assert!(
        wait_for(|| ran.load(Ordering::SeqCst) == 1).await,
        "S1 first entered"
    );
    assert!(
        wait_for(|| store.claim_attempts() >= 2).await,
        "S1 idle peer consumed the handoff and observed no eligible successor"
    );
    assert_eq!(ran.load(Ordering::SeqCst), 1, "S1/E1");

    release.add_permits(1);
    assert!(
        wait_for(|| ran.load(Ordering::SeqCst) == 2).await,
        "S2/E2 settlement rearmed the successor before the fallback poll"
    );
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if store.list_dispatches().await.unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("S2 queue drained to idle");
    for run_id in ["settlement-rearm-first", "settlement-rearm-second"] {
        assert!(
            matches!(
                CommittedThreadView::run(&*commit, &RunId(run_id.into())).map(|run| run.state),
                Some(RunState::Ended(_))
            ),
            "S2 {run_id} reached terminal committed truth"
        );
    }
    pool.shutdown().await;
}

#[tokio::test]
async fn special_claim_error_blocks_the_same_tick_ordinary_claim() {
    // Cause/effect table: B1 special claim errors -> that drain tick returns an
    // error and ordinary claim count stays zero; B2 the shared backoff elapses
    // with no external wake and a one-hour poll -> the retained self-retry first
    // rechecks special admission, then ordinary execution may proceed. This
    // prevents a transient policy/read failure from bypassing retry exhaustion
    // admission or being stranded behind the coarse fallback.
    let clock = Arc::new(ManualClock::new(0));
    let store =
        Arc::new(FlakyDispatchStore::new(0, clock.clone()).with_retry_exhaustion_failures(1));
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
        clock,
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(3_600),
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

#[tokio::test]
async fn recovered_claim_does_not_serialize_distinct_threads_until_tool_completion() {
    assert_recovered_claim_thread_concurrency(false).await;
}

#[tokio::test]
async fn recovered_claim_still_serializes_runs_of_the_same_thread() {
    assert_recovered_claim_thread_concurrency(true).await;
}

async fn assert_recovered_claim_thread_concurrency(same_thread: bool) {
    use std::sync::atomic::Ordering;

    // Causes: C1 one queue claim fails; C2 the next claim and resolution succeed;
    // C3 that Run blocks inside a tool; C4 another Thread is runnable and pool
    // capacity is two. Effects: E1 the outage permits only one recovery probe;
    // E2 recovered admission releases the peer before C3 finishes; E3 each Run
    // executes once and settles after explicit release. Decision rule P1:
    // C1+C2+C3+C4 => E2, not serialization until the first Run settles.
    // P2 negates distinct-Thread C4: the second Run must wait for settlement;
    // releasing a local recovery probe never grants a durable execution claim.
    // The special-claim and gate tests own E1 while this real Pool/Worker test
    // crosses the missing recovery-to-execution boundary. A one-hour fallback
    // and no external wake prevent timers from disguising a lost handoff.
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, ran) = blocking_tool_runtime(release.clone());
    let store = Arc::new(FlakyDispatchStore::new(1));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = Arc::new(DispatchWorker::new(
        runtime,
        store.clone(),
        commit.clone(),
        "pool",
    ));
    for (run, thread) in [
        ("probe-a", "thread-a"),
        ("probe-b", if same_thread { "thread-a" } else { "thread-b" }),
    ] {
        let mut activation = activation_on(run, thread);
        activation.input[0].id = awaken_agent_contract::agent::message::Id(format!("{run}-input"));
        store.enqueue(RunDispatch::new(activation)).await.unwrap();
    }
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        Arc::new(SystemClock),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(3_600),
            ..Default::default()
        },
        Arc::new(FlakyResolver { worker }),
        2,
        Arc::new(BlackholeWake),
    );
    let first_entered = wait_for(|| ran.load(Ordering::SeqCst) >= 1).await;
    let both_entered = wait_for(|| ran.load(Ordering::SeqCst) == 2).await;
    // Always release and join even on the regression path; a failed assertion
    // must not leave blocked tool futures or background renewals behind.
    release.add_permits(2);
    let settled = wait_for(|| store.dispatch_count() == 0).await;
    pool.shutdown().await;
    assert!(first_entered, "P1/P2 the recovery probe actually executed");
    assert_eq!(
        both_entered, !same_thread,
        "P1/P2 recovered admission preserves only the same-Thread execution fence"
    );
    assert!(settled, "P1/E3 both retained Runs settle");
    assert_eq!(
        ran.load(Ordering::SeqCst),
        2,
        "P1/E3 no duplicate execution"
    );
}

#[tokio::test]
async fn failed_post_commit_observation_relinquishes_and_self_retries() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FailOnceObserver(AtomicUsize);

    #[async_trait]
    impl DispatchSettlementObserver for FailOnceObserver {
        async fn before_settle(
            &self,
            _dispatch: &RunDispatch,
            _claim: &RunClaim,
            _committed_state: &RunState,
            _cancellation_requested: bool,
        ) -> Result<(), DispatchSettlementError> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(DispatchSettlementError("projection not ready".into()))
            } else {
                Ok(())
            }
        }
    }

    // Causes: C1 execution committed terminal truth; C2 its settlement observer
    // fails once; C3 no external wake and the fallback poll is one hour; C4 the
    // physical attempt has already quiesced. Effects: E1 the exact mutation
    // claim is relinquished without deleting the row; E2 the pool self-retries;
    // E3 committed truth is not executed twice; E4 the second observation
    // settles the row. If a physical slot were still active, its independent
    // fence would block executor re-entry (covered by the physical-attempt suite).
    //
    // | Rule | commit | observer | physical slot | Effect |
    // | R1 | terminal | fails | quiesced | E1+E2+E3 |
    // | R2 | terminal | succeeds | quiesced | E4 |
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let observer = Arc::new(FailOnceObserver(AtomicUsize::new(0)));
    let (runtime, infers) = counting_text_runtime();
    let worker = Arc::new(
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "observer-worker")
            .with_settlement_observer(observer.clone()),
    );
    store
        .enqueue_session_child(
            RunDispatch::new(activation("observer-retry"))
                .for_session(ThreadId("parent-session".into()))
                .with_session_activity_epoch(1),
            SessionChildAdmission::new(25, Vec::new()),
        )
        .await
        .expect("admit coordinated run");
    let wake = Arc::new(LocalWakeSignal::new());
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        Arc::new(SystemClock),
        "observer-worker",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(3_600),
            ..DispatchServiceConfig::default()
        },
        Arc::new(MapResolver {
            workers: HashMap::from([("parent-session".to_string(), worker)]),
        }),
        1,
        wake,
    );
    pool.notify().await;

    let drained = wait_for(|| store.dispatch_count() == 0).await;
    assert!(
        drained,
        "R2/E4 rows={:?} observations={} commits={}",
        store.list_dispatches().await.unwrap(),
        observer.0.load(Ordering::SeqCst),
        commit.commit_count(),
    );
    assert_eq!(observer.0.load(Ordering::SeqCst), 2, "R1/R2");
    assert_eq!(infers.load(Ordering::SeqCst), 1, "R1/E3");
    pool.shutdown().await;
}

#[tokio::test]
async fn unavailable_settlement_observer_does_not_starve_an_independent_thread() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct UnavailableObserver(AtomicUsize);

    #[async_trait]
    impl DispatchSettlementObserver for UnavailableObserver {
        async fn before_settle(
            &self,
            _dispatch: &RunDispatch,
            _claim: &RunClaim,
            _committed_state: &RunState,
            _cancellation_requested: bool,
        ) -> Result<(), DispatchSettlementError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(DispatchSettlementError(
                "predecessor physical quiescence remains unknown".into(),
            ))
        }
    }

    // Causes: C1 an earlier coordinated Run has committed terminal truth; C2
    // its exact Session observer remains unavailable; C3 an unrelated Thread is
    // runnable behind it; C4 pool width is one and the coarse poll is one hour.
    // Effects: E1 only C1 is durably deferred; E2 C3 executes without inheriting
    // a pool-wide fault backoff; E3 C1 stays retained and is never re-executed.
    //
    // | Rule | observer | unrelated row | Effect |
    // | R1 | unavailable | absent | retain and retry after local deadline |
    // | R2 | unavailable | present | E1+E2+E3 |
    // | R3 | infrastructure/store fault | any | shared fault gate (separate tests) |
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let observer = Arc::new(UnavailableObserver(AtomicUsize::new(0)));
    let (runtime, infers) = counting_text_runtime();
    let worker = Arc::new(
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "observer-isolation")
            .with_settlement_observer(observer.clone()),
    );
    store
        .enqueue_session_child(
            RunDispatch::new(activation_on("blocked-observer", "blocked-session"))
                .for_session(ThreadId("parent-session".into()))
                .with_session_activity_epoch(1),
            SessionChildAdmission::new(25, Vec::new()),
        )
        .await
        .expect("admit coordinated run");
    store
        .enqueue(RunDispatch::new(activation_on(
            "independent-after-observer",
            "healthy-thread",
        )))
        .await
        .expect("enqueue unrelated run");
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        Arc::new(SystemClock),
        "observer-isolation",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(3_600),
            ..DispatchServiceConfig::default()
        },
        Arc::new(MapResolver {
            workers: HashMap::from([
                ("parent-session".to_string(), worker.clone()),
                ("healthy-thread".to_string(), worker),
            ]),
        }),
        1,
        Arc::new(BlackholeWake),
    );

    let progressed = tokio::time::timeout(
        Duration::from_millis(500),
        wait_for(|| {
            commit
                .run_state(&RunId("independent-after-observer".into()))
                .is_some()
        }),
    )
    .await
    .is_ok_and(|progressed| progressed);
    pool.shutdown().await;

    assert!(progressed, "R2/E2 unrelated Thread was starved");
    assert_eq!(
        infers.load(Ordering::SeqCst),
        2,
        "R2/E3 one inference per Run"
    );
    assert!(
        store
            .list_dispatches()
            .await
            .expect("list retained dispatches")
            .iter()
            .any(|row| row.run_id == RunId("blocked-observer".into())),
        "R1/E1 unavailable observer must retain its exact dispatch",
    );
    assert!(
        observer.0.load(Ordering::SeqCst) >= 1,
        "R2 exercised observer"
    );
}

/// Exact-claim renewal ownership cause/effect decision table. C1 is the Pool
/// claim, C2 is resolver completion, C3 is the exact Worker drive, C4 is
/// cancellation, C5 is resolver failure, C6 is expiry/replacement, and C7 is an
/// already-entered attempt. E1 is one
/// renewal write per interval, E2 is guard transfer without a second Tokio task,
/// E3 is guard shutdown, E4 is immediate relinquish, and E5 is fail-closed stale
/// execution. E6 is cooperative cancellation of the in-flight attempt.
///
/// | Rule | Resolver | Exact operation | Authority | Expected effect |
/// |---|---|---|---|---|
/// | RG1 | succeeds | normal execute | current | E1+E2, then E3 |
/// | RG2 | succeeds | cancel | current | E1+E2, then E3 |
/// | RG3 | fails | none | current | E1+E4, then E3 |
/// | RG4 | blocked | none | expired/replaced | one failed E1, E3+E5 |
/// | RG5 | succeeds | C7 execute | expired/replaced | one failed E1, E5+E6 |
/// | RG6 | succeeds | C7 execute | transient renewal errors | bounded retries, retain claim |
/// | RG7 | succeeds | C7 execute | renewal call stalls | deadline, then E5+E6 |
/// | RG8 | succeeds | C7 execute | renewal applies, response lost | exact retry converges |
///
/// The count is the observable task cardinality: the pre-fix Pool and Worker
/// tasks both woke on the same interval, producing two renewal writes for RG1
/// and RG2. One transferred guard produces exactly one.
/// Constraint/Invariant: exactly one renewal guard follows the exact claim from
/// Pool through Worker and stops at every terminal/failure exit. Decision rule:
/// this test owns RG1; the adjacent tests own RG2-RG8.
#[tokio::test(start_paused = true)]
async fn pool_transfers_one_renewal_guard_into_a_normal_drive() {
    use std::sync::atomic::Ordering;

    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, ran) = blocking_tool_runtime(release.clone());
    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(FlakyDispatchStore::new(0, clock.clone()));
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
        clock,
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
    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(FlakyDispatchStore::new(0, clock.clone()));
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
        clock,
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
    // the claim and drops that guard; C3 graceful drain closes new admission;
    // E3 later virtual time adds no renewal from the failed claim.
    // Decision rule RG3=C1+C2+C3=>E1+E2+E3.
    // Constraint/Invariant: resolution failure relinquishes the same exact claim
    // and terminates its sole guard before another Worker may claim it.
    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(FlakyDispatchStore::new(0, clock.clone()));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    store
        .enqueue(RunDispatch::new(activation("one-renewal-failure")))
        .await
        .unwrap();
    let entered_wait = entered.notified();
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        clock,
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
        wait_for_async(|| {
            let store = store.clone();
            async move {
                store.list_dispatches().await.unwrap()[0].state
                    == awaken_dispatch::DispatchState::Pending
            }
        })
        .await,
        "RG3 resolver failure relinquished the exact claim"
    );
    let _local_fence = pool.begin_drain().await;
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
    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(FlakyDispatchStore::new(0, clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = Arc::new(
        DispatchWorker::new(text_runtime(), store.clone(), commit.clone(), "pool")
            .with_lease_ms(90),
    );
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
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
            .claim("replacement", 90, &Default::default())
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

#[tokio::test(start_paused = true)]
async fn physical_return_ack_retries_transport_without_reexecuting_the_attempt() {
    // Cause/effect table after actual executor return: A1 transient failure
    // before write; A2 write committed but response lost; A3 request times out;
    // A4 permanent rejection. A1-A3 -> retry the SAME claim, one inference,
    // one input and one terminal commit, no occupied slot; A4 -> error, retain
    // the slot. Empty-slot ACK is the existing idempotent store transition.
    // This is a Worker integration test, not just a direct store retry oracle.
    for fault in [
        harness::FinishFault::BeforeWrite,
        harness::FinishFault::LostResponse,
        harness::FinishFault::Timeout,
        harness::FinishFault::Rejected,
    ] {
        let rejected = matches!(fault, harness::FinishFault::Rejected);
        let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
        let store = Arc::new(FlakyDispatchStore::new(0, clock.clock()).with_finish_fault(fault));
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let (runtime, infers) = counting_text_runtime();
        let worker = DispatchWorker::new(runtime, store.clone(), commit.clone(), "ack-owner")
            .with_lease_ms(90);
        store
            .enqueue(RunDispatch::new(activation("ack-retry")))
            .await
            .unwrap();
        clock.set(0);
        let claimed = store
            .claim("ack-owner", 90, &Default::default())
            .await
            .unwrap()
            .unwrap();
        let exact = RunClaim::from(&claimed.lease);
        let result = tokio::time::timeout(
            Duration::from_millis(250),
            worker.drive_claimed(claimed, Arc::new(ManualClock::new(0))),
        )
        .await
        .expect("A3 ACK request timeout must not abandon the observed return");
        assert_eq!(
            result.is_err(),
            rejected,
            "{fault:?}: retry transport, not execution"
        );
        assert_eq!(infers.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            commit.commit_count(),
            4,
            "input, model entry/return evidence, terminal; no replay"
        );
        assert_eq!(
            store.finish_claims(),
            vec![exact; if rejected { 1 } else { 2 }]
        );
        assert_eq!(
            store
                .list_dispatches()
                .await
                .unwrap()
                .iter()
                .any(|row| row.physical_attempt_active),
            rejected
        );
    }
}

#[tokio::test(start_paused = true)]
async fn lost_renewal_cancels_the_in_flight_attempt() {
    // Test design — Rule RG5 from the adjacent decision table. Causes: C1 the
    // exact claim has entered its attempt; C2 its lease expires; C3 a peer
    // replaces the claim; C4 the old renewal observes authoritative loss.
    // Effects: E1 the existing Runtime cancellation token fires immediately;
    // E2 the stale attempt exits without a Thread commit; E3 no later renewal
    // is emitted. Constraint: renewal/fencing remain the only ownership truth;
    // cancellation is a signal projection, not another claim state machine.
    // RG5b: delayed cleanup acknowledges cancellation, then stays in the same
    // executor future beyond the lease request budget. Only its later return
    // may clear the slot; a timed-out observer must remain attached.
    let cleanup_release = Arc::new(tokio::sync::Semaphore::new(0));
    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = Arc::new(
        DispatchWorker::new(text_runtime(), store.clone(), commit.clone(), "owner-a")
            .with_lease_ms(90),
    );
    let entered = Arc::new(tokio::sync::Notify::new());
    let cancellation_observed = Arc::new(tokio::sync::Notify::new());
    worker.install_attempt_executor(Arc::new(OwnershipCancellationAttemptExecutor {
        entered: entered.clone(),
        cancellation_observed: cancellation_observed.clone(),
        cleanup_release: Some(cleanup_release.clone()),
    }));
    store
        .enqueue(RunDispatch::new(activation("renewal-loss-cancels-attempt")))
        .await
        .unwrap();
    clock.set(0);
    let claimed = store
        .claim("owner-a", 90, &Default::default())
        .await
        .unwrap()
        .expect("owner A claims the Run");
    let entered_wait = entered.notified();
    let observed_wait = cancellation_observed.notified();
    let driven = tokio::spawn({
        let worker = worker.clone();
        let clock = clock.clone();
        async move { worker.drive_claimed(claimed, clock).await }
    });
    entered_wait.await;

    clock.set(91);
    assert!(
        store
            .claim("owner-b", 90, &Default::default())
            .await
            .unwrap()
            .is_some(),
        "RG5/C3 a peer replaces the expired exact claim"
    );
    tokio::time::advance(Duration::from_millis(30)).await;
    observed_wait.await;
    tokio::time::advance(Duration::from_millis(180)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(
        !driven.is_finished(),
        "RG5b cleanup observer survives request budget"
    );
    assert!(store.list_dispatches().await.unwrap()[0].physical_attempt_active);
    cleanup_release.add_permits(1);
    assert!(driven.await.expect("drive task joins").is_err(), "RG5/E1");
    assert_eq!(commit.commit_count(), 0, "RG5/E2");
    assert!(
        !store.list_dispatches().await.unwrap()[0].physical_attempt_active,
        "RG5 cooperative executor return acknowledges physical quiescence"
    );
}

#[tokio::test(start_paused = true)]
async fn durable_cancel_signals_the_exact_live_attempt_before_lease_expiry() {
    // Cause/effect decision table: C1=an exact claim owns the active physical
    // attempt; C2=durable cancel is committed before lease expiry; C3=the next
    // renewal observes the same row and exact physical owner/epoch. E1=the
    // existing Runtime token is cancelled; E2=the executor returns and clears
    // only its own slot; E3=no stale Thread commit is accepted.
    // R1=C1+C2+C3 -> E1+E2+E3. Wrong owner/epoch and the current cancellation
    // worker are covered by the shared three-backend LR6/LR7 table.
    // Constraint: this uses the sole renewal loop; no direct live-control signal,
    // extra poller, or cancellation registry participates.
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = Arc::new(MemoryDispatchStore::new(clock.clock()));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = Arc::new(
        DispatchWorker::new(text_runtime(), store.clone(), commit.clone(), "owner-a")
            .with_lease_ms(90),
    );
    let entered = Arc::new(tokio::sync::Notify::new());
    let cancellation_observed = Arc::new(tokio::sync::Notify::new());
    worker.install_attempt_executor(Arc::new(OwnershipCancellationAttemptExecutor {
        entered: entered.clone(),
        cancellation_observed: cancellation_observed.clone(),
        cleanup_release: None,
    }));
    let run_id = RunId("live-durable-cancellation".into());
    store
        .enqueue(RunDispatch::new(activation(&run_id.0)))
        .await
        .unwrap();
    clock.set(0);
    let claimed = store
        .claim("owner-a", 90, &Default::default())
        .await
        .unwrap()
        .expect("C1 current claim");
    let driven = tokio::spawn({
        let worker = worker.clone();
        async move {
            worker
                .drive_claimed(claimed, Arc::new(ManualClock::new(0)))
                .await
        }
    });
    entered.notified().await;
    assert_eq!(
        store.cancel(&run_id).await.unwrap(),
        Some(ThreadId(harness::THREAD.into())),
        "C2 durable cancellation",
    );
    let observed = cancellation_observed.notified();
    tokio::time::advance(Duration::from_millis(30)).await;
    observed.await;
    assert!(driven.await.expect("attempt joins").is_err(), "R1/E1");
    assert_eq!(commit.commit_count(), 0, "R1/E3");
    let row = store.list_dispatches().await.unwrap();
    assert_eq!(row.len(), 1, "cancellation intent remains claimable");
    assert!(!row[0].physical_attempt_active, "R1/E2");
}

#[tokio::test(start_paused = true)]
async fn non_quiescing_attempt_blocks_replacement_after_lease_takeover() {
    // Rule RG9 extends the renewal decision table. Causes: C1 A has entered an
    // opaque executor; C2 B replaces A's expired mutation claim; C3 cancellation
    // fires; C4 A's executor does not return within the bounded request timeout.
    // Effects: E1 A cannot commit; E2 its physical slot remains visible; E3 B's
    // exact begin is Blocked. Constraint: timeout/lease expiry is not fabricated
    // quiescence. A trusted provider/process termination receipt is required to
    // recover availability; absent that proof, safety intentionally wins.
    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = Arc::new(
        DispatchWorker::new(text_runtime(), store.clone(), commit.clone(), "owner-a")
            .with_lease_ms(30),
    );
    let entered = Arc::new(tokio::sync::Notify::new());
    worker.install_attempt_executor(Arc::new(NonQuiescingAttemptExecutor {
        entered: entered.clone(),
    }));
    store
        .enqueue(RunDispatch::new(activation("non-quiescing")))
        .await
        .unwrap();
    clock.set(0);
    let claimed = store
        .claim("owner-a", 30, &Default::default())
        .await
        .unwrap()
        .expect("RG9 A claim");
    let entered_wait = entered.notified();
    let driven = tokio::spawn({
        let worker = worker.clone();
        let clock = clock.clone();
        async move { worker.drive_claimed(claimed, clock).await }
    });
    entered_wait.await;

    clock.set(31);
    let replacement = store
        .claim("owner-b", 30, &Default::default())
        .await
        .unwrap()
        .expect("RG9 B mutation claim");
    tokio::time::advance(Duration::from_millis(15)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(
        !driven.is_finished(),
        "E1 unknown effect retains its observer"
    );
    driven.abort();
    assert!(driven.await.unwrap_err().is_cancelled());
    assert_eq!(commit.commit_count(), 0, "E1");
    assert!(
        store.list_dispatches().await.unwrap()[0].physical_attempt_active,
        "E2"
    );
    assert_eq!(
        store
            .begin_attempt(
                &awaken_dispatch::RunClaim::from(&replacement.lease),
                None
            )
            .await
            .unwrap(),
        awaken_dispatch::AttemptAdmission::Blocked,
        "E3"
    );
}

#[tokio::test]
async fn aborting_a_drive_never_fabricates_remote_quiescence() {
    // Rule RG10. Causes: C1 A entered an opaque external request; C2 its Tokio
    // drive task is aborted without an executor return; C3 the Lease expires
    // and B takes the mutation claim. Effects: E1 A's physical slot remains;
    // E2 B's begin is Blocked; E3 no second executor enters. Constraint: Future
    // drop stops local polling but is not a provider cancellation/terminal ACK.
    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = Arc::new(
        DispatchWorker::new(text_runtime(), store.clone(), commit, "owner-a").with_lease_ms(30),
    );
    let entered = Arc::new(tokio::sync::Notify::new());
    worker.install_attempt_executor(Arc::new(NonQuiescingAttemptExecutor {
        entered: entered.clone(),
    }));
    store
        .enqueue(RunDispatch::new(activation("aborted-opaque-attempt")))
        .await
        .expect("RG10 fixture enqueued");
    clock.set(0);
    let claimed = store
        .claim("owner-a", 30, &Default::default())
        .await
        .expect("RG10 A claim query")
        .expect("RG10 A claim");
    let entered_wait = entered.notified();
    let driven = tokio::spawn({
        let worker = worker.clone();
        let clock = clock.clone();
        async move { worker.drive_claimed(claimed, clock).await }
    });
    entered_wait.await;

    driven.abort();
    assert!(
        driven
            .await
            .expect_err("RG10/C2 abort cancels the local task")
            .is_cancelled(),
        "RG10/C2"
    );
    assert!(
        store.list_dispatches().await.unwrap()[0].physical_attempt_active,
        "RG10/E1 Future drop is not remote quiescence"
    );

    clock.set(31);
    let replacement = store
        .claim("owner-b", 30, &Default::default())
        .await
        .expect("RG10 B claim query")
        .expect("RG10 B mutation claim");
    assert_eq!(
        store
            .begin_attempt(
                &awaken_dispatch::RunClaim::from(&replacement.lease),
                None
            )
            .await
            .expect("RG10 B physical admission"),
        awaken_dispatch::AttemptAdmission::Blocked,
        "RG10/E2+E3"
    );
}

#[tokio::test(start_paused = true)]
async fn transient_renewal_errors_retry_inside_the_same_guard() {
    // Test design — Rule RG6. Causes: C1 an exact claim is actively executing;
    // C2 two renewal attempts fail transiently; C3 the third succeeds before the
    // local proof deadline. Effects: E1 retries use the same guard faster than
    // the regular interval; E2 Runtime is not cancelled; E3 the original claim
    // completes normally. No retry attempt authors lease state outside Dispatch.
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, ran) = blocking_tool_runtime(release.clone());
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = Arc::new(FlakyDispatchStore::new(0, clock.clock()).with_renewal_failures(2));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = Arc::new(
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "owner-a").with_lease_ms(90),
    );
    store
        .enqueue(RunDispatch::new(activation("transient-renewal-retry")))
        .await
        .unwrap();
    clock.set(0);
    let claimed = store
        .claim("owner-a", 90, &Default::default())
        .await
        .unwrap()
        .expect("RG6 exact claim");
    let driven = tokio::spawn({
        let worker = worker.clone();
        async move {
            worker
                .drive_claimed(claimed, Arc::new(ManualClock::new(0)))
                .await
        }
    });
    assert!(
        yield_until(|| ran.load(std::sync::atomic::Ordering::SeqCst) == 1).await,
        "RG6 execution entered"
    );

    tokio::time::advance(Duration::from_millis(30)).await;
    assert!(yield_until(|| store.renewal_attempts() >= 1).await);
    tokio::time::advance(Duration::from_millis(3)).await;
    assert!(yield_until(|| store.renewal_attempts() >= 2).await);
    tokio::time::advance(Duration::from_millis(3)).await;
    assert!(
        yield_until(|| store.renewal_attempts() >= 3).await,
        "RG6/E1"
    );
    assert!(!driven.is_finished(), "RG6/E2");

    release.add_permits(1);
    assert!(driven.await.expect("RG6 drive joins").is_ok(), "RG6/E3");
    assert!(
        commit.commit_count() > 0,
        "RG6/E3 authoritative commit path"
    );
}

#[tokio::test(start_paused = true)]
async fn stalled_renewal_call_cancels_before_the_lease_can_be_recovered() {
    // Test design — Rule RG7. Causes: C1 an exact claim is actively executing;
    // C2 every renewal call blocks; C3 each request exceeds its short deadline;
    // C4 no attempt proves ownership by the two-thirds-lease safety deadline.
    // Effects: E1 calls are bounded and retried by the same guard; E2 Runtime is
    // cancelled before the durable lease expires; E3 no Thread commit occurs.
    let renewal_gate = Arc::new(tokio::sync::Semaphore::new(0));
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = Arc::new(FlakyDispatchStore::new(0, clock.clock()).with_renewal_gate(renewal_gate));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let entered = Arc::new(tokio::sync::Notify::new());
    let cancellation_observed = Arc::new(tokio::sync::Notify::new());
    let worker = Arc::new(
        DispatchWorker::new(text_runtime(), store.clone(), commit.clone(), "owner-a")
            .with_lease_ms(90),
    );
    worker.install_attempt_executor(Arc::new(OwnershipCancellationAttemptExecutor {
        entered: entered.clone(),
        cancellation_observed: cancellation_observed.clone(),
        cleanup_release: None,
    }));
    store
        .enqueue(RunDispatch::new(activation("stalled-renewal-call")))
        .await
        .unwrap();
    clock.set(0);
    let claimed = store
        .claim("owner-a", 90, &Default::default())
        .await
        .unwrap()
        .expect("RG7 exact claim");
    let entered_wait = entered.notified();
    let cancellation_wait = cancellation_observed.notified();
    let driven = tokio::spawn({
        let worker = worker.clone();
        async move {
            worker
                .drive_claimed(claimed, Arc::new(ManualClock::new(0)))
                .await
        }
    });
    entered_wait.await;

    tokio::time::advance(Duration::from_millis(30)).await;
    assert!(yield_until(|| store.renewal_attempts() >= 1).await);
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_millis(15)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_millis(3)).await;
    assert!(
        yield_until(|| store.renewal_attempts() >= 2).await,
        "RG7/E1 timed-out call is retried"
    );
    tokio::time::advance(Duration::from_millis(12)).await;
    cancellation_wait.await;
    assert!(driven.await.expect("RG7 drive joins").is_err(), "RG7/E2");
    assert_eq!(commit.commit_count(), 0, "RG7/E3");
    assert!(
        !store.list_dispatches().await.unwrap()[0].physical_attempt_active,
        "RG7 cooperative cancellation ACK releases the physical slot"
    );
}

#[tokio::test(start_paused = true)]
async fn ambiguous_renewal_response_retries_the_same_exact_claim() {
    // Test design — Rule RG8. Causes: C1 the first exact renewal commits in the
    // store; C2 its response remains unavailable until the request deadline;
    // C3 a retry of the same RunClaim receives an acknowledgement inside the
    // proof window. Effects: E1 retry converges idempotently; E2 Runtime remains
    // active; E3 there is one Run/epoch and ordinary terminal commit. The guard
    // does not invent an "unknown renewal" state or a second receipt authority.
    let response_gate = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, ran) = blocking_tool_runtime(release.clone());
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = Arc::new(
        FlakyDispatchStore::new(0, clock.clock()).with_renewal_response_gate(response_gate.clone()),
    );
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = Arc::new(
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "owner-a").with_lease_ms(90),
    );
    store
        .enqueue(RunDispatch::new(activation("ambiguous-renewal-response")))
        .await
        .unwrap();
    clock.set(0);
    let claimed = store
        .claim("owner-a", 90, &Default::default())
        .await
        .unwrap()
        .expect("RG8 exact claim");
    let driven = tokio::spawn({
        let worker = worker.clone();
        async move {
            worker
                .drive_claimed(claimed, Arc::new(ManualClock::new(0)))
                .await
        }
    });
    assert!(
        yield_until(|| ran.load(std::sync::atomic::Ordering::SeqCst) == 1).await,
        "RG8 execution entered"
    );

    tokio::time::advance(Duration::from_millis(30)).await;
    assert!(yield_until(|| store.renewal_attempts() >= 1).await);
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_millis(15)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    response_gate.add_permits(1);
    tokio::time::advance(Duration::from_millis(3)).await;
    assert!(
        yield_until(|| store.renewal_attempts() >= 2).await,
        "RG8/E1 exact retry"
    );
    assert!(!driven.is_finished(), "RG8/E2");

    release.add_permits(1);
    assert!(driven.await.expect("RG8 drive joins").is_ok(), "RG8/E3");
    assert!(commit.commit_count() > 0, "RG8/E3 authoritative commit");
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
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
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
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
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
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
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
            tokio::time::timeout(std::time::Duration::from_secs(30), self.notify.notified())
                .await
                .expect("timed out waiting for self.notify.notified()");
        }
    }

    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
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
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
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

    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
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
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
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
    // One hand-driven clock: the store judges lease expiry against the same
    // reading the pool polls with, so only the advance recovers the run.
    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
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
    clock.set(0);
    assert!(
        store
            .claim("dead-worker", 1_000, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(commit.commit_count(), 0);

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
    // Test design. Causes: C1 a cross-Thread message is staged; C2 `send` publishes
    // the Pool's one external wake. Effects: E1 C2 fans out to maintenance and
    // relays before the deliberately distant fallback poll; E2 repeated relay
    // attempts do not duplicate the stable message id.
    // Constraint/Invariant: the Outbox row is the sole relay authority. Decision rule:
    // make the timer unusable as an explanation, then require prompt delivery.
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
    let resolver = Arc::new(MapResolver {
        workers: HashMap::new(),
    });
    let pool = DispatchPool::spawn(
        store.clone(),
        Arc::new(SystemClock),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(30),
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

    // The published wake, not the 30-second fallback, relays the outbox.
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let records = store.list(&thread).await.unwrap();
            if records.iter().any(|r| r.input.message_id == "xthread-1") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the external wake promptly relays staged input");

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
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
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

/// Concurrency > 1 drives two Threads in parallel while each Run retains one
/// authoritative multi-call ToolBatch.
#[tokio::test]
async fn concurrency_drives_distinct_runs_in_parallel() {
    use std::sync::atomic::Ordering;

    // Cause/effect graph: C1 pool capacity is two; C2 Runs own distinct Threads;
    // C3 each model response contains two calls; C4 one call per Run remains
    // blocked. Effects: E1 both Runs enter concurrently; E2 all four calls enter
    // exactly once; E3 each Thread commits one finalized two-call batch; E4 no
    // Run/call state crosses the Thread boundary. Decision rule MT1=C1+C2+C3+C4
    // =>E1+E2; releasing C4=>E3+E4. The sibling same-Thread test negates C2 and
    // proves the physical-attempt fence serializes Runs without disabling
    // within-batch tool parallelism.
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (rt_a, ran_a) = multi_tool_blocking_runtime(release.clone(), 2);
    let (rt_b, ran_b) = multi_tool_blocking_runtime(release.clone(), 2);
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
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
        wait_for(|| ran_a.load(Ordering::SeqCst) == 2 && ran_b.load(Ordering::SeqCst) == 2).await,
        "MT1/E1+E2 both two-call batches entered in parallel"
    );

    // Release both blocked tools; both drives complete on their own boundaries.
    release.add_permits(2);
    assert!(
        wait_for(|| {
            CommittedThreadView::run(commit_a.as_ref(), &RunId("run-a".into()))
                .is_some_and(|run| matches!(run.state, RunState::Ended(_)))
                && CommittedThreadView::run(commit_b.as_ref(), &RunId("run-b".into()))
                    .is_some_and(|run| matches!(run.state, RunState::Ended(_)))
        })
        .await,
        "MT1/E3 both parallel Runs settled"
    );
    for (rule, commit, thread) in [
        ("thread-a", &commit_a, ThreadId("thread-a".into())),
        ("thread-b", &commit_b, ThreadId("thread-b".into())),
    ] {
        let state =
            awaken_agent_contract::agent::state::Store::rebuild(&commit.committed_state(&thread));
        let batch = <awaken_runtime_contract::ActiveToolBatch as awaken_agent_contract::agent::state::StateKey>::load(&state)
            .expect("MT1 readable batch")
            .expect("MT1 retained batch");
        assert_eq!(batch.calls().len(), 2, "MT1/E3 {rule}");
        assert_eq!(
            batch.phase(),
            awaken_runtime_contract::ToolBatchPhase::Finalized,
            "MT1/E3 {rule}"
        );
        assert!(
            batch.calls().iter().all(|call| call.phase.is_terminal()),
            "MT1/E3+E4 {rule}"
        );
    }

    pool.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrency_two_never_enters_two_attempts_for_one_thread() {
    use std::sync::atomic::Ordering;

    // Cause/effect graph and decision rules:
    // C1=pool capacity 2, C2=two Runs share one Thread, C3=Run-1 is blocked
    // inside a real tool, C4=Run-1 tool returns. E1=Run-2 tool has never
    // entered, E2=Run-2 later enters, E3=max active tool attempts is one.
    // P1 C1+C2+C3+!C4 -> E1+E3; P2 C1+C2+C4 -> E2+E3. The adjacent
    // distinct-Thread case is the non-over-serialization control and proves
    // capacity 2 still permits genuine parallelism where it is safe.
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, calls, active, maximum) = concurrency_tracking_tool_runtime(release.clone());
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = worker_over(runtime, store.clone(), commit);
    let resolver = Arc::new(MapResolver {
        workers: HashMap::from([("thread-shared".to_string(), worker)]),
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

    let mut first_activation = activation_on("same-thread-1", "thread-shared");
    first_activation.input[0].id = awaken_agent_contract::agent::message::Id("same-input-1".into());
    pool.submit(first_activation)
        .await
        .expect("P1 submit first");
    assert!(
        wait_for(|| calls.load(Ordering::SeqCst) == 1).await,
        "P1 Run-1 entered the real tool"
    );
    let mut second_activation = activation_on("same-thread-2", "thread-shared");
    second_activation.input[0].id =
        awaken_agent_contract::agent::message::Id("same-input-2".into());
    pool.submit(second_activation)
        .await
        .expect("P1 submit second");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "P1/E1");
    assert_eq!(active.load(Ordering::SeqCst), 1, "P1 one live attempt");
    assert_eq!(maximum.load(Ordering::SeqCst), 1, "P1/E3");

    release.add_permits(1);
    assert!(
        wait_for(|| calls.load(Ordering::SeqCst) == 2).await,
        "P2/E2 Run-2 enters after Run-1 returns and settles"
    );
    assert!(wait_for(|| active.load(Ordering::SeqCst) == 0).await);
    assert_eq!(maximum.load(Ordering::SeqCst), 1, "P2/E3");
    pool.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicated_exact_claim_never_enters_the_executor_twice() {
    use std::sync::atomic::Ordering;

    // Physical-delivery decision table:
    // D1: same exact Claim + first future live -> duplicate waits at the one
    // Runtime Thread gate; calls=1 and max-active=1.
    // D2: first future returns and settles -> duplicate reaches durable
    // admission, is fenced by the removed/settled row, and never executes.
    // Constraint: `AlreadyApplied` supports a lost begin-response retry, but is
    // not permission for two local drive tasks to execute the same Claim.
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, calls, active, maximum) = concurrency_tracking_tool_runtime(release.clone());
    // The store and both drive tasks read one hand-driven t=0 reading.
    let manual = Arc::new(ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(manual.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = worker_over(runtime, store.clone(), commit);
    store
        .enqueue(RunDispatch::new(activation("duplicated-exact-claim")))
        .await
        .expect("D1 fixture enqueued");
    manual.set(0);
    let claimed = store
        .claim("pool", DEFAULT_LEASE_MS, &Default::default())
        .await
        .expect("D1 claim query")
        .expect("D1 exact claim");
    let clock: Arc<dyn Clock> = manual;
    let first = tokio::spawn({
        let worker = worker.clone();
        let claimed = claimed.clone();
        let clock = clock.clone();
        async move { worker.drive_claimed(claimed, clock).await }
    });
    assert!(
        wait_for(|| calls.load(Ordering::SeqCst) == 1).await,
        "D1 first delivery entered the tool"
    );
    let duplicate = tokio::spawn({
        let worker = worker.clone();
        let clock = clock.clone();
        async move { worker.drive_claimed(claimed, clock).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "D1 duplicate waits");
    assert_eq!(active.load(Ordering::SeqCst), 1, "D1 one live future");
    assert_eq!(maximum.load(Ordering::SeqCst), 1, "D1 max one");

    release.add_permits(1);
    first
        .await
        .expect("D2 first drive joins")
        .expect("D2 first drive commits");
    assert!(
        duplicate.await.expect("D2 duplicate drive joins").is_err(),
        "D2 settled exact Claim is fenced before executor re-entry"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "D2 no second call");
    assert_eq!(maximum.load(Ordering::SeqCst), 1, "D2 max remains one");
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
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
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
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
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
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, _ran, entered) = blocking_tool_runtime_with_entry_signal(release.clone());
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
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
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .expect("the drive entered the blocked tool while holding the pool's lease");

    // Across a window far beyond the base lease, a thief owner can never reclaim it —
    // the renewal loop keeps the lease from expiring.
    for _ in 0..30 {
        assert!(
            store
                .claim("thief", lease_ms, &Default::default())
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
        worker: Arc<FlakyWorker>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Semaphore>,
    }

    #[async_trait]
    impl WorkerResolver<FlakyDispatchStore> for SlowResolver {
        async fn worker_for(
            &self,
            _thread_id: &ThreadId,
            _agent_id: Option<&str>,
        ) -> Result<Arc<FlakyWorker>, Error> {
            self.entered.notify_one();
            self.release
                .acquire()
                .await
                .expect("resolution release")
                .forget();
            Ok(self.worker.clone())
        }
    }

    // The lease clock is manual so the base lease expires by decree, not by
    // waiting. The renewal guard still schedules itself on wall time
    // (renew every lease/3, give up after lease - lease/3 without a proof),
    // so the lease is long enough that a loaded machine cannot starve the
    // guard past its proof window; the test itself waits on the renewal
    // write, never on a sleep that assumes the guard has already run.
    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(FlakyDispatchStore::new(0, clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let lease_ms = 1_500;
    let worker = Arc::new(
        DispatchWorker::new(text_runtime(), store.clone(), commit.clone(), "pool")
            .with_lease_ms(lease_ms),
    );
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
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
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .expect("timed out waiting for the resolver to enter");
    // Expire the base lease outright, then wait for the first renewal written
    // against the advanced clock. The single-threaded runtime cannot run the
    // guard between reading the attempt count and moving the clock, and the
    // store counts an attempt in the same poll that applies it, so one more
    // attempt is one renewal stamped at or after the advanced instant.
    let renewals_before_expiry = store.renewal_attempts();
    clock.set(lease_ms * 3);
    let renewal_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while store.renewal_attempts() == renewals_before_expiry {
        assert!(
            tokio::time::Instant::now() < renewal_deadline,
            "timed out waiting for the blocked resolution to renew its claim"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        store
            .claim("thief", lease_ms, &Default::default())
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
    // | R2 | live | fails | removed | exact claim is request-locally deferred |
    // | R3 | absent | n/a | absent | no lease write (idle-poller test) |
    // R2 drops the exact guard instead of indefinitely preserving an unsettled
    // claim after provisioning or Runtime construction has failed. Deferral
    // avoids waiting a full lease or charging crash recovery without allowing
    // one unresolved request to hot-loop.
    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    store
        .enqueue(RunDispatch::new(activation("resolver-failure")))
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let lease_ms = 100;
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        clock.clone(),
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
    let _drain_fence = pool.begin_drain().await;
    clock.set(0);
    let premature = store
        .claim("recovery-owner", lease_ms, &Default::default())
        .await
        .unwrap();
    assert!(
        premature.is_none(),
        "R2 failed claim is not eligible before its request-local deadline"
    );
    clock.set(250);
    let reclaimed = store
        .claim("recovery-owner", lease_ms, &Default::default())
        .await
        .unwrap();
    let reclaimed = reclaimed.expect("R2 failed claim is available after its deadline");
    assert!(
        !reclaimed.recovered,
        "R2 is admission rollback, not a crash"
    );

    pool.shutdown().await;
}

#[tokio::test]
async fn retryable_resolution_failure_does_not_starve_an_independent_thread() {
    assert_resolution_failure_isolation(1).await;
}

#[tokio::test]
async fn equal_priority_resolution_failure_allows_an_independent_thread() {
    assert_resolution_failure_isolation(0).await;
}

async fn assert_resolution_failure_isolation(failing_priority: i64) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct SelectiveResolver {
        worker: Arc<MemWorker>,
        rejected: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl WorkerResolver<MemoryDispatchStore> for SelectiveResolver {
        async fn worker_for(
            &self,
            thread: &ThreadId,
            _: Option<&str>,
        ) -> Result<Arc<MemWorker>, Error> {
            if thread.0 == "unresolved-thread" {
                self.rejected.fetch_add(1, Ordering::SeqCst);
                return Err(Error::Execution(
                    awaken_runtime_contract::execution::Error::Execution(
                        "retained projection cannot yet be resolved".into(),
                    ),
                ));
            }
            Ok(self.worker.clone())
        }
    }

    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
    store
        .enqueue_with(
            RunDispatch::new(activation_on("unresolved", "unresolved-thread")),
            awaken_dispatch::SubmitOptions {
                priority: failing_priority,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    store
        .enqueue(RunDispatch::new(activation("independent")))
        .await
        .unwrap();
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let rejected = Arc::new(AtomicUsize::new(0));
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        Arc::new(SystemClock),
        "resolution-isolation",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_millis(10),
            ..Default::default()
        },
        Arc::new(SelectiveResolver {
            worker: worker_over(text_runtime(), store, commit.clone()),
            rejected: rejected.clone(),
        }),
        1,
        Arc::new(BlackholeWake),
    );
    let progressed = wait_for(|| commit.commit_count() > 0).await;
    pool.shutdown().await;
    assert!(
        rejected.load(Ordering::SeqCst) > 0,
        "the unresolved request was actually attempted"
    );
    assert!(
        progressed,
        "an independent runnable Thread must progress despite a returned resolution error"
    );
}

#[tokio::test]
async fn many_equal_priority_resolution_failures_do_not_accumulate_global_backoff() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct PrefixRejectingResolver {
        worker: Arc<MemWorker>,
        rejected: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl WorkerResolver<MemoryDispatchStore> for PrefixRejectingResolver {
        async fn worker_for(
            &self,
            thread: &ThreadId,
            _: Option<&str>,
        ) -> Result<Arc<MemWorker>, Error> {
            if thread.0.starts_with("unresolved-") {
                self.rejected.fetch_add(1, Ordering::SeqCst);
                return Err(Error::Execution(
                    awaken_runtime_contract::execution::Error::Execution(
                        "retained projection cannot yet be resolved".into(),
                    ),
                ));
            }
            Ok(self.worker.clone())
        }
    }

    const UNRESOLVED: usize = 16;
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
    for index in 0..UNRESOLVED {
        store
            .enqueue(RunDispatch::new(activation_on(
                &format!("unresolved-run-{index}"),
                &format!("unresolved-thread-{index}"),
            )))
            .await
            .unwrap();
    }
    store
        .enqueue(RunDispatch::new(activation_on(
            "healthy-after-unresolved",
            "healthy-thread",
        )))
        .await
        .unwrap();

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let rejected = Arc::new(AtomicUsize::new(0));
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        Arc::new(SystemClock),
        "bulk-resolution-isolation",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(60),
            ..Default::default()
        },
        Arc::new(PrefixRejectingResolver {
            worker: worker_over(text_runtime(), store, commit.clone()),
            rejected: rejected.clone(),
        }),
        1,
        Arc::new(BlackholeWake),
    );

    let progressed_without_global_backoff = tokio::time::timeout(
        Duration::from_millis(500),
        wait_for(|| commit.commit_count() > 0),
    )
    .await
    .is_ok_and(|progressed| progressed);
    pool.shutdown().await;

    assert_eq!(
        rejected.load(Ordering::SeqCst),
        UNRESOLVED,
        "all earlier unresolved rows were retained and individually deferred"
    );
    assert!(
        progressed_without_global_backoff,
        "later healthy work must not inherit unrelated resolution backoff"
    );
}

#[tokio::test]
async fn session_work_backpressure_defers_only_that_request_then_retries() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Cause/effect graph: C1 a valid Run claim reaches a Session whose one
    // Environment Work slot is occupied; C2 the resolver returns typed NotReady;
    // C3 queue polling is configured below the admission floor. C1+C2 -> E1
    // request-local deferral without crash accounting; C2+C3 -> E2 no
    // warning-speed hot reacquisition. Once the deadline passes, the same
    // durable row becomes eligible again.
    //
    // | Rule | resolver | poll | first 100 ms effect | durable row effect |
    // | B1 | NotReady | 10 ms | one resolution attempt | deferred for 250 ms |
    // | B2 | fault | any | warning/backoff path | covered by R2 above |
    //
    // FMECA: treating expected Environment serialization as a fault produced
    // ~20 claim/relinquish cycles per second, inflated lease epochs, and hid real
    // Worker failures. Holding the lease instead would starve the rightful next
    // owner, so the canonical pool releases the claim and durably defers only
    // that request.
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
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
            .claim("ready-owner", DEFAULT_LEASE_MS, &Default::default(),)
            .await
            .unwrap()
            .is_none(),
        "B1 remains durable but cannot be reclaimed before its deadline"
    );
    assert!(
        wait_for(|| calls.load(Ordering::SeqCst) >= 2).await,
        "B1 is retried after its request-local deadline"
    );
    assert_eq!(store.dispatch_count(), 1, "B1 is retained across retries");
    pool.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn successive_resolution_failures_retry_without_stranding_a_single_drain() {
    assert_successive_resolution_failures_retry(1).await;
}

#[tokio::test(start_paused = true)]
async fn successive_resolution_failures_preserve_multi_drain_progress() {
    assert_successive_resolution_failures_retry(4).await;
}

async fn assert_successive_resolution_failures_retry(concurrency: usize) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // C1 generic resolution fails before execution; C2 its request-local retry
    // reaches ordinary Environment pressure; C3 the next retry resolves.
    // E1 neither C1 nor C2 opens the pool-wide infrastructure-error gate; E2 C3
    // executes the retained request exactly once without another enqueue,
    // external wake, queue fallback, or lease-expiry recovery.
    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    store
        .enqueue(RunDispatch::new(activation("recovered-pressure")))
        .await
        .unwrap();
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let (runtime, inference_calls) = counting_text_runtime();
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        clock.clone(),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(3600),
            terminal_reconciliation_interval: None,
            ..Default::default()
        },
        Arc::new(RecoveringBackpressureResolver {
            calls: calls.clone(),
            worker: worker_over(runtime, store.clone(), commit.clone()),
        }),
        concurrency,
        Arc::new(BlackholeWake),
    );
    // Explicit virtual-clock steps allow runnable drain tasks to settle before
    // each deadline. Test timing cannot be decided by host CPU load.
    assert!(yield_until(|| calls.load(Ordering::SeqCst) == 1).await);
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    clock.set(250);
    tokio::time::advance(Duration::from_millis(250)).await;
    assert!(yield_until(|| calls.load(Ordering::SeqCst) == 2).await);
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    clock.set(500);
    tokio::time::advance(Duration::from_millis(250)).await;
    let completed = yield_until(|| store.dispatch_count() == 0).await;
    pool.shutdown().await;
    assert!(
        completed,
        "successive request-local resolution failures must not strand the pool"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "only the third resolution executes"
    );
    assert_eq!(
        inference_calls.load(Ordering::SeqCst),
        1,
        "one actual inference"
    );
    assert!(matches!(
        commit.committed().latest_run.unwrap().state,
        RunState::Ended(_)
    ));
}

#[tokio::test]
async fn session_work_backpressure_rearms_one_retry_after_the_floor() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Cause/effect graph: C1 the only initial permit reaches NotReady; C2 no
    // external/local queue wake follows; C3 the 250 ms admission floor elapses.
    // E1 no reacquisition occurs before C3; E2 the same durable row is retried
    // after C3. This is the restart row where Session projection completion is
    // local state and therefore emits no queue edge.
    //
    // | Rule | initial permit | later wake | floor elapsed | Effect |
    // |---|---|---|---|---|
    // | N1 | yes | no | no | exactly one attempt |
    // | N2 | yes | no | yes | E2 one rate-limited retry |
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
    store
        .enqueue(RunDispatch::new(activation("session-work-rearm")))
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let pool = DispatchPool::spawn_with_wake(
        store,
        Arc::new(SystemClock),
        "waiting-owner",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(30),
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
        "N1 initial attempt"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "N1 floor");
    tokio::time::timeout(Duration::from_secs(1), async {
        while calls.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("N2 rate-limited self-wake");

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
    // F5 extends F3: two queued Runs share a Thread with pool concurrency=2.
    // The first terminal settlement must release the follower; both resolve
    // exactly once, commit errors, notify completion, and leave no queued work.
    // This proves terminal queue progress, not physical execution exclusion.
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

    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = worker_over(text_runtime(), store.clone(), commit.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let sink = Arc::new(RecordingSink::default());
    let runs = ["resolution-failure", "resolution-failure-follower"];
    for run in runs {
        store
            .enqueue(RunDispatch::new(activation(run)))
            .await
            .unwrap();
    }
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
        2,
        sink.clone(),
    );

    assert!(
        wait_for(|| sink.0.lock().unwrap().len() == runs.len()).await,
        "F3/F5 both foreground completions are notified"
    );
    pool.shutdown().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        runs.len(),
        "F3/F5 each resolves once"
    );
    assert_eq!(store.dispatch_count(), 0, "F3/F5 dispatches settled Done");
    for run in runs {
        let state = commit
            .run_state(&RunId(run.into()))
            .expect("F3/F5 terminal Run commit");
        assert!(
            matches!(
                state,
                RunState::Ended(awaken_agent_contract::agent::run::EndCause::Error(_))
            ),
            "F3/F5 deterministic resolution error is committed as a Run failure"
        );
        assert_eq!(
            sink.0
                .lock()
                .unwrap()
                .iter()
                .filter(|(id, _)| id == run)
                .count(),
            1
        );
    }
}

/// Pool drain cause/effect table: P1 expired+exhausted -> special claim; P2 the
/// special claim -> canonical boundary Worker terminal commit and Done settle;
/// P3 this path -> ordinary execution resolution is never invoked; P4 automatic
/// terminalization -> no manual dead-letter quarantine.
#[tokio::test]
async fn drain_terminalizes_retry_exhaustion_without_execution_resolution() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    store
        .enqueue(RunDispatch::new(activation_on("poison", "thread-poison")))
        .await
        .unwrap();
    clock.set(0);
    assert!(
        store
            .claim("failed-worker", 1, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    clock.set(2);
    assert!(
        store
            .claim("failed-worker", 1, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    clock.set(4);
    assert!(
        store
            .claim("failed-worker", 1, &Default::default())
            .await
            .unwrap()
            .is_some()
    );

    let ordinary_calls = Arc::new(AtomicUsize::new(0));
    let terminal_calls = Arc::new(AtomicUsize::new(0));
    let terminal_worker = worker_over(text_runtime(), store.clone(), commit.clone());
    clock.set(6);
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        clock.clone(),
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

    let clock = Arc::new(ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    store
        .enqueue(RunDispatch::new(activation_on("poison", "thread-poison")))
        .await
        .unwrap();
    clock.set(0);
    assert!(
        store
            .claim("dead", 1, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    clock.set(2);
    assert!(
        store
            .claim("dead", 1, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    let ordinary_calls = Arc::new(AtomicUsize::new(0));
    let terminal_calls = Arc::new(AtomicUsize::new(0));
    clock.set(4);
    let maintenance = DispatchReconciler::spawn(
        store.clone(),
        clock.clone(),
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
        clock.clone(),
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
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
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
    let _local_fence = pool.begin_drain().await;
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
            && matches!(s.state, awaken_dispatch::DispatchState::Pending)),
        "the post-drain run stays PENDING (never claimed); got {summaries:?}"
    );

    pool.shutdown().await;
}

#[tokio::test]
async fn authority_suspension_is_reversible_but_drain_is_absorbing() {
    // Cause/effect decision table for the single Pool admission state machine:
    //
    // | Rule | current | command | effect |
    // |---|---|---|---|
    // | A1 | Open | suspend | no new claim; queued Run remains pending |
    // | A2 | Suspended | resume | claims reopen; queued Run completes |
    // | A3 | Open/Suspended | drain | admission closes permanently |
    // | A4 | Draining | resume | false; no claim is resurrected |
    //
    // In-flight execution is intentionally not cancelled by A1; the claim gate
    // only linearizes admission. Existing cancellation/renewal tests own that
    // orthogonal behavior.
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = worker_over(text_runtime(), store.clone(), commit.clone());
    let resolver = Arc::new(MapResolver {
        workers: HashMap::from([("thread-authority".to_string(), worker)]),
    });
    let pool = DispatchPool::spawn(
        store.clone(),
        Arc::new(SystemClock),
        "pool-authority",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig::default(),
        resolver,
        1,
    );

    assert!(pool.is_accepting().await, "A1 precondition");
    pool.suspend_admission().await;
    assert!(!pool.is_accepting().await, "A1");
    assert!(pool.can_resume().await, "A2 precondition");
    pool.submit(activation_on("run-paused", "thread-authority"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        commit.run_state(&RunId("run-paused".into())).is_none(),
        "A1"
    );
    assert!(
        store
            .list_dispatches()
            .await
            .unwrap()
            .iter()
            .any(|row| row.run_id.0 == "run-paused"
                && row.state == awaken_dispatch::DispatchState::Pending),
        "A1"
    );

    assert!(pool.resume_admission().await, "A2");
    assert!(
        wait_for(|| commit.run_state(&RunId("run-paused".into())).is_some()).await,
        "A2"
    );
    let _local_fence = pool.begin_drain().await;
    assert!(!pool.can_resume().await, "A3");
    assert!(!pool.resume_admission().await, "A4");
    assert!(!pool.is_accepting().await, "A4");
    pool.shutdown().await;
}

#[tokio::test]
async fn authority_suspension_does_not_cancel_an_in_flight_run() {
    // Cause/effect rule I1: a Run has crossed the claim boundary and is blocked
    // in execution when registry uncertainty suspends admission. The effect is
    // that in-flight stays one, its Runtime token is not cancelled, and release
    // still commits normally while new claims remain closed. Permanent drain
    // cancellation is intentionally a different command/state transition.
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, ran) = blocking_tool_runtime(release.clone());
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(SystemClock)));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = worker_over(runtime, store.clone(), commit.clone());
    let pool = DispatchPool::spawn(
        store,
        Arc::new(SystemClock),
        "pool-in-flight-authority",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig::default(),
        Arc::new(MapResolver {
            workers: HashMap::from([("thread-in-flight".to_string(), worker)]),
        }),
        1,
    );
    pool.submit(activation_on("run-in-flight", "thread-in-flight"))
        .await
        .unwrap();
    assert!(
        wait_for(|| ran.load(std::sync::atomic::Ordering::SeqCst) == 1).await,
        "I1 execution entered"
    );
    assert_eq!(pool.in_flight(), 1, "I1 precondition");
    pool.suspend_admission().await;
    assert!(!pool.is_accepting().await, "I1 claim gate closes");
    assert_eq!(pool.in_flight(), 1, "I1 in-flight ownership remains");
    release.add_permits(1);
    assert!(
        wait_for(|| commit.run_state(&RunId("run-in-flight".into())).is_some()).await,
        "I1 existing Run commits while admission is suspended"
    );
    pool.shutdown().await;
}

#[path = "pool_memory/maintenance_observations.rs"]
mod maintenance_observations;
