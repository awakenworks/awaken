// @formal evidence worker_drain.bounded_cleanup_eventually_exits_without_fabricating_quiescence
// @formal evidence worker_drain.grace_expiry_retains_cancellation_observer
//! Decision tables for the process dispatch: pool start, coordinator-side
//! terminal reconciliation start and lifetime, and the drain fence deadline.

#[path = "tests/startup_drain.rs"]
mod startup_drain;

#[path = "tests/publication.rs"]
mod publication;

#[path = "tests/construction.rs"]
mod construction;

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{
    Mutex,
    mpsc::{Receiver, SyncSender, sync_channel},
};

#[test]
fn pool_start_follows_the_selected_topology_and_pool_state() {
    // Cause/effect graph: C1 the topology executes Runs locally (pool sizing);
    // C2 a pool already runs; C3 the dispatch authority is bound. E1 admission
    // only; E2 keep the running pool; E3 wait for the binding; E4 spawn with the
    // selected sizing. Constraint: a coordinator-only process never spawns, and
    // a second start never replaces the first pool.
    //
    // | Rule | C1 sizing | C2 running | C3 bound | Effect |
    // |------|-----------|------------|----------|--------|
    // | S1   | none      | any        | any      | E1     |
    // | S2   | some      | yes        | any      | E2     |
    // | S3   | some      | no         | no       | E3     |
    // | S4   | some      | no         | yes      | E4     |
    let sizing = DispatchPoolSizing::LocalParallelism(std::num::NonZeroUsize::new(3).unwrap());
    for (running, bound) in [(false, false), (false, true), (true, false), (true, true)] {
        assert_eq!(
            pool_start(None, running, bound),
            PoolStart::RemoteWorkersOnly,
            "S1 running={running} bound={bound}"
        );
    }
    for bound in [false, true] {
        assert_eq!(
            pool_start(Some(sizing), true, bound),
            PoolStart::AlreadyRunning,
            "S2 bound={bound}"
        );
    }
    assert_eq!(
        pool_start(Some(sizing), false, false),
        PoolStart::AwaitingBinding,
        "S3"
    );
    assert_eq!(
        pool_start(Some(sizing), false, true),
        PoolStart::Spawn(sizing),
        "S4"
    );
}

#[test]
fn terminal_reconciliation_starts_only_for_a_poolless_durable_truth_owner() {
    // Cause/effect graph: C1 durable ingress; C2 a local pool runs (it already
    // reconciles); C3 this process scans committed terminals; C4 the loop
    // already started. E1 start the loop; E2 do not.
    //
    // | Rule | C1  | C2  | C3  | C4  | Effect |
    // |------|-----|-----|-----|-----|--------|
    // | R1   | yes | no  | yes | no  | E1     |
    // | R2   | no  | -   | -   | -   | E2     |
    // | R3   | -   | yes | -   | -   | E2     |
    // | R4   | -   | -   | no  | -   | E2     |
    // | R5   | -   | -   | -   | yes | E2     |
    for durable in [false, true] {
        for local_pool in [false, true] {
            for scans in [false, true] {
                for started in [false, true] {
                    let expected = durable && !local_pool && scans && !started;
                    assert_eq!(
                        starts_terminal_reconciliation(durable, local_pool, scans, started),
                        expected,
                        "R durable={durable} pool={local_pool} scans={scans} started={started}"
                    );
                }
            }
        }
    }
    assert!(
        starts_terminal_reconciliation(true, false, true, false),
        "R1"
    );
}

#[tokio::test(start_paused = true)]
async fn drain_fence_obeys_the_callers_absolute_deadline() {
    // Cause/effect graph: C1 pool-fence acquisition completes or remains
    // pending; C2 the absolute drain deadline is before or after completion.
    // E1 return the exact fence value on completion; E2 retain and poll the
    // pending production future until C2; E3 report timeout without a fence.
    // Constraint: `Some(None)` remains the valid no-local-pool result and must
    // not be confused with E3's outer `None`.
    //
    // | Rule | C1 | C2 | Effect |
    // |---|---|---|---|
    // | F1 | complete | after completion | E1 (`Some(None)`) |
    // | F2 | pending | before deadline | E2 |
    // | F3 | pending | at deadline | E3 |
    let budget = std::time::Duration::from_secs(5);
    assert!(
        matches!(
            drain_fence_before(tokio::time::Instant::now() + budget, async {
                None::<PoolDrainFence>
            })
            .await,
            Some(None)
        ),
        "F1/E1"
    );

    let dropped = Arc::new(AtomicBool::new(false));
    let started = Arc::new(tokio::sync::Notify::new());
    let deadline = tokio::time::Instant::now() + budget;
    let task = tokio::spawn(drain_fence_before(deadline, {
        let dropped = dropped.clone();
        let started = started.clone();
        async move {
            struct FenceDrop(Arc<AtomicBool>);
            impl Drop for FenceDrop {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let _fence = FenceDrop(dropped);
            started.notify_one();
            std::future::pending::<Option<PoolDrainFence>>().await
        }
    }));
    started.notified().await;
    tokio::time::advance(budget - std::time::Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert!(!task.is_finished(), "F2/E2");
    assert!(!dropped.load(Ordering::SeqCst), "F2/E2");
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    assert!(task.await.unwrap().is_none(), "F3/E3");
    assert!(dropped.load(Ordering::SeqCst), "F3/E3");
}

/// A maintenance-only resolver that retains the shared scan state, as the
/// executor's coordinator resolver does: the edge that closed the former cycle
/// (task -> resolver -> state -> loop).
struct ScanStateResolver {
    queue: Arc<AnyDispatchStore>,
    _scan_state: Arc<TerminalReconciliationState>,
    scans: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
}

impl Drop for ScanStateResolver {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl WorkerResolver<AnyDispatchStore> for ScanStateResolver {
    fn dispatch_store(&self) -> Result<Arc<AnyDispatchStore>, Error> {
        Ok(self.queue.clone())
    }

    async fn worker_for(
        &self,
        _thread_id: &awaken_agent_contract::agent::thread::Id,
        _agent_id: Option<&str>,
    ) -> Result<Arc<crate::DispatchWorker<AnyDispatchStore>>, Error> {
        unreachable!("terminal maintenance resolves no execution Worker")
    }

    async fn reconcile_committed_terminals(
        &self,
        _limit: usize,
    ) -> Result<
        Vec<(
            awaken_agent_contract::agent::run::Id,
            awaken_agent_contract::agent::run::RunState,
        )>,
        Error,
    > {
        self.scans.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
}

struct NoCompletion;

impl CompletionSink for NoCompletion {
    fn settled(
        &self,
        _run_id: &awaken_agent_contract::agent::run::Id,
        _state: &awaken_agent_contract::agent::run::RunState,
    ) {
    }
}

/// A poolless durable Coordinator dispatch, its owner and a resolver over the
/// dispatch's own scan state.
struct CoordinatorFixture {
    dispatch: ProcessDispatch,
    owner: Arc<AnyCoordinatorDispatch>,
    resolver: Arc<ScanStateResolver>,
    scans: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
}

fn coordinator_fixture(local: awaken_deployment_config::ClaimPool) -> CoordinatorFixture {
    coordinator_fixture_with_wake(local, Arc::new(crate::LocalWakeSignal::new()))
}

fn coordinator_fixture_with_wake(
    local: awaken_deployment_config::ClaimPool,
    wake: Arc<dyn awaken_dispatch_contract::WakeSignal>,
) -> CoordinatorFixture {
    let clock = awaken_time::system_clock();
    let store = Arc::new(awaken_dispatch::MemoryDispatchStore::new(clock.clone()));
    let owner = Arc::new(AnyCoordinatorDispatch::from_durable_dispatch(store));
    let dispatch = ProcessDispatch::new(
        ExecutionTopology {
            foreground_delivery: awaken_deployment_config::ForegroundRunDelivery::Durable,
            claim_pool: local,
            lease_owner: awaken_deployment_config::LeaseOwnerId::new("coordinator").unwrap(),
            lost_wake_recovery_bound: std::time::Duration::from_millis(5),
            queue_depth: awaken_deployment_config::QueueDepthSource::External,
        },
        clock,
        DispatchHint::new(wake),
    );
    let scans = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let resolver = Arc::new(ScanStateResolver {
        queue: owner.worker_view(),
        _scan_state: dispatch.terminal_reconciliation().clone(),
        scans: scans.clone(),
        dropped: dropped.clone(),
    });
    CoordinatorFixture {
        dispatch,
        owner,
        resolver,
        scans,
        dropped,
    }
}

async fn eventually(what: &str, condition: impl Fn() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        // awaken-allow(wait): a task's end has no committed fact; it is observed
        // through the values it releases, bounded by the enclosing timeout.
        while !condition() {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"));
}

#[tokio::test]
async fn terminal_reconciliation_ends_with_its_process_dispatch_or_its_stop() {
    // Cause/effect graph: C1 the loop started over a resolver that retains
    // this dispatch's scan state; C2 the process dispatch is dropped; C3 the
    // process begins draining while the dispatch lives; C4 a start
    // after the stop. E1 the loop runs and holds its resolver; E2 the loop
    // ends and frees its resolver and the scan state; E3 the loop ends and
    // frees its resolver, the dispatch stays usable; E4 refused, no resolver
    // is built.
    //
    // | Rule | C1  | C2  | C3  | C4  | Effect |
    // |------|-----|-----|-----|-----|--------|
    // | T1   | yes | no  | no  | -   | E1     |
    // | T2   | yes | yes | -   | -   | E2     |
    // | T3   | yes | no  | yes | -   | E3     |
    // | T4   | yes | no  | yes | yes | E4     |
    let fixture = coordinator_fixture(awaken_deployment_config::ClaimPool::RemoteWorkersOnly);
    let scan_state = Arc::downgrade(fixture.dispatch.terminal_reconciliation());
    let resolver = fixture.resolver.clone() as Arc<dyn WorkerResolver<AnyDispatchStore>>;
    assert!(
        fixture.dispatch.ensure_terminal_reconciliation(
            true,
            Some(fixture.owner.clone()),
            Arc::new(NoCompletion),
            || Some(resolver)
        ),
        "T1 start"
    );
    drop(fixture.resolver);
    let scans = fixture.scans.clone();
    eventually("T1/E1 the loop scans", || scans.load(Ordering::SeqCst) > 0).await;
    assert!(
        !fixture.dropped.load(Ordering::SeqCst),
        "T1/E1 the loop holds its resolver"
    );
    drop(fixture.dispatch);
    let dropped = fixture.dropped.clone();
    eventually("T2/E2 the loop frees its resolver", || {
        dropped.load(Ordering::SeqCst)
    })
    .await;
    assert!(
        scan_state.upgrade().is_none(),
        "T2/E2 the scan state is freed"
    );

    let fixture = coordinator_fixture(awaken_deployment_config::ClaimPool::RemoteWorkersOnly);
    let resolver = fixture.resolver.clone() as Arc<dyn WorkerResolver<AnyDispatchStore>>;
    assert!(
        fixture.dispatch.ensure_terminal_reconciliation(
            true,
            Some(fixture.owner.clone()),
            Arc::new(NoCompletion),
            || Some(resolver)
        ),
        "T3 start"
    );
    drop(fixture.resolver);
    let scans = fixture.scans.clone();
    eventually("T3 the loop scans", || scans.load(Ordering::SeqCst) > 0).await;
    assert!(
        fixture.dispatch.begin_drain().await.is_none(),
        "T3 no pool fence"
    );
    assert!(
        fixture.dispatch.begin_drain().await.is_none(),
        "T3 idempotent"
    );
    let dropped = fixture.dropped.clone();
    eventually("T3/E3 the stopped loop frees its resolver", || {
        dropped.load(Ordering::SeqCst)
    })
    .await;
    assert_eq!(
        fixture.dispatch.in_flight(),
        0,
        "T3/E3 the dispatch stays usable"
    );
    assert!(
        !fixture.dispatch.ensure_terminal_reconciliation(
            true,
            Some(fixture.owner),
            Arc::new(NoCompletion),
            || { panic!("T4/E4 a stopped loop builds no second resolver") }
        ),
        "T4/E4"
    );
}

/// Pause the first actual initializer while a competing call can enter. The
/// bounded receive only releases fixture setup; exact factory counts after
/// both calls finish determine success, never a scheduling or timing SLO.
struct StartRace {
    calls: AtomicUsize,
    first: (SyncSender<()>, Mutex<Receiver<()>>),
    competing: (SyncSender<()>, Mutex<Receiver<()>>),
    release: (SyncSender<()>, Mutex<Receiver<()>>),
}

impl StartRace {
    fn new() -> Self {
        let signal = || {
            let (send, receive) = sync_channel(1);
            (send, Mutex::new(receive))
        };
        Self {
            calls: AtomicUsize::new(0),
            first: signal(),
            competing: signal(),
            release: signal(),
        }
    }

    fn enter_factory(&self) {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first.0.send(()).unwrap();
            self.release
                .1
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
        } else {
            self.competing.0.send(()).unwrap();
        }
    }

    fn wait_for_first(&self) {
        self.first
            .1
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    }

    fn release_when_competitor_can_enter(self: &Arc<Self>) -> std::thread::JoinHandle<()> {
        let race = self.clone();
        std::thread::spawn(move || {
            // awaken-allow(wait): release a controlled factory stall even when
            // atomic initialization correctly blocks the competing factory.
            let _ = race
                .competing
                .1
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(2));
            race.release.0.send(()).unwrap();
        })
    }
}

#[tokio::test]
async fn concurrent_pool_start_constructs_one_pool_and_one_resolver() {
    // C1 local topology + bound queue + absent pool; C2 first factory is
    // paused; C3 another start arrives. Rule I1 C1+C2+C3 => both calls succeed,
    // one factory/pool is constructed. I2 an already-started call never builds
    // another resolver or replaces the registered pool. The real constructor
    // and empty authoritative queue run; no fake pool or second queue is used.
    let fixture = Arc::new(coordinator_fixture(
        awaken_deployment_config::ClaimPool::InProcess(DispatchPoolSizing::LocalParallelism(
            std::num::NonZeroUsize::new(1).unwrap(),
        )),
    ));
    let race = Arc::new(StartRace::new());
    let first = {
        let fixture = fixture.clone();
        let race = race.clone();
        let runtime = tokio::runtime::Handle::current();
        std::thread::spawn(move || {
            let _runtime = runtime.enter();
            fixture.dispatch.ensure_pool(
                Some(DispatchBinding::local(fixture.owner.clone())),
                Arc::new(NoCompletion),
                || {
                    race.enter_factory();
                    Ok(fixture.resolver.clone() as Arc<dyn WorkerResolver<AnyDispatchStore>>)
                },
            )
        })
    };
    race.wait_for_first();
    let release = race.release_when_competitor_can_enter();
    let second = fixture.dispatch.ensure_pool(
        Some(DispatchBinding::local(fixture.owner.clone())),
        Arc::new(NoCompletion),
        || {
            race.enter_factory();
            Ok(fixture.resolver.clone() as Arc<dyn WorkerResolver<AnyDispatchStore>>)
        },
    );
    first.join().unwrap().unwrap();
    second.unwrap();
    release.join().unwrap();
    let calls = race.calls.load(Ordering::SeqCst);
    let original = fixture.dispatch.pool().unwrap().clone();
    fixture
        .dispatch
        .ensure_pool(
            Some(DispatchBinding::local(fixture.owner.clone())),
            Arc::new(NoCompletion),
            || panic!("I2 an existing pool cannot construct another resolver"),
        )
        .unwrap();
    assert!(
        Arc::ptr_eq(&original, fixture.dispatch.pool().unwrap()),
        "I2"
    );
    let _drain = fixture.dispatch.begin_drain().await;
    assert_eq!(calls, 1, "I1 one physical pool constructor");
}

#[tokio::test]
async fn concurrent_terminal_start_constructs_one_loop_and_one_resolver() {
    // C1 poolless durable committed-truth owner + absent loop; C2 first
    // factory paused; C3 competing start. I3 C1+C2+C3 => one physical loop and
    // resolver, exactly one caller reports a new start. I4 an existing loop
    // ignores later factories. Original T1-T4 below still own drop/drain.
    let fixture = Arc::new(coordinator_fixture(
        awaken_deployment_config::ClaimPool::RemoteWorkersOnly,
    ));
    let race = Arc::new(StartRace::new());
    let first = {
        let fixture = fixture.clone();
        let race = race.clone();
        let runtime = tokio::runtime::Handle::current();
        std::thread::spawn(move || {
            let _runtime = runtime.enter();
            fixture.dispatch.ensure_terminal_reconciliation(
                true,
                Some(fixture.owner.clone()),
                Arc::new(NoCompletion),
                || {
                    race.enter_factory();
                    Some(fixture.resolver.clone() as Arc<dyn WorkerResolver<AnyDispatchStore>>)
                },
            )
        })
    };
    race.wait_for_first();
    let release = race.release_when_competitor_can_enter();
    let second = fixture.dispatch.ensure_terminal_reconciliation(
        true,
        Some(fixture.owner.clone()),
        Arc::new(NoCompletion),
        || {
            race.enter_factory();
            Some(fixture.resolver.clone() as Arc<dyn WorkerResolver<AnyDispatchStore>>)
        },
    );
    let first = first.join().unwrap();
    release.join().unwrap();
    let calls = race.calls.load(Ordering::SeqCst);
    assert_eq!(usize::from(first) + usize::from(second), 1, "I3 one start");
    assert!(!fixture.dispatch.ensure_terminal_reconciliation(
        true,
        Some(fixture.owner.clone()),
        Arc::new(NoCompletion),
        || panic!("I4 an existing loop cannot construct another resolver"),
    ));
    assert!(fixture.dispatch.begin_drain().await.is_none());
    assert_eq!(calls, 1, "I3 one physical reconciliation-loop constructor");
}

#[tokio::test]
async fn failed_pool_initialization_preserves_refusal_and_remains_retryable() {
    // F1 unbound queue => no factory; F2 factory error or pool-binding
    // refusal => unchanged refusal and no pool; F3 factory panic => it
    // propagates without caching a pool;
    // F4 a later valid factory => one pool. Neither a transient error nor an
    // unwound initializer may poison/cache failure as the permanent owner.
    let fixture = coordinator_fixture(awaken_deployment_config::ClaimPool::InProcess(
        DispatchPoolSizing::LocalParallelism(std::num::NonZeroUsize::new(1).unwrap()),
    ));
    fixture
        .dispatch
        .ensure_pool(None, Arc::new(NoCompletion), || {
            panic!("F1 an unbound queue cannot construct a resolver")
        })
        .unwrap();
    assert!(fixture.dispatch.pool().is_none(), "F1");
    let error = crate::session_execution::execution_error("controlled pool initializer refusal");
    let expected = error.to_string();
    let refused = fixture
        .dispatch
        .ensure_pool(
            Some(DispatchBinding::local(fixture.owner.clone())),
            Arc::new(NoCompletion),
            || Err(error),
        )
        .unwrap_err();
    assert_eq!(refused.to_string(), expected, "F2 unchanged error");
    assert!(fixture.dispatch.pool().is_none(), "F2 empty");
    let foreign = coordinator_fixture(awaken_deployment_config::ClaimPool::RemoteWorkersOnly);
    let mismatch = fixture.dispatch.ensure_pool(
        Some(DispatchBinding::local(fixture.owner.clone())),
        Arc::new(NoCompletion),
        || Ok(foreign.resolver.clone()),
    );
    assert!(mismatch.is_err(), "F2 underlying pool-binding refusal");
    assert!(
        fixture.dispatch.pool().is_none(),
        "F2 rejected pool is not cached"
    );
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fixture.dispatch.ensure_pool(
            Some(DispatchBinding::local(fixture.owner.clone())),
            Arc::new(NoCompletion),
            || panic!("controlled pool initializer panic"),
        )
    }));
    assert!(panic.is_err(), "F3 original unwind");
    assert!(fixture.dispatch.pool().is_none(), "F3 empty");
    fixture
        .dispatch
        .ensure_pool(
            Some(DispatchBinding::local(fixture.owner.clone())),
            Arc::new(NoCompletion),
            || Ok(fixture.resolver.clone()),
        )
        .unwrap();
    assert!(fixture.dispatch.pool().is_some(), "F4 successful retry");
    let _drain = fixture.dispatch.begin_drain().await;
}

#[tokio::test]
async fn unavailable_terminal_initialization_leaves_the_slot_retryable() {
    // F5 missing owner => no resolver factory; F6 resolver declines => no loop;
    // F7 factory panic propagates without registering a loop; F8 a later valid
    // factory starts exactly once. The existing lifetime tests still own
    // cancellation/drain, and I3/I4 above own concurrent construction.
    let fixture = coordinator_fixture(awaken_deployment_config::ClaimPool::RemoteWorkersOnly);
    assert!(!fixture.dispatch.ensure_terminal_reconciliation(
        true,
        None,
        Arc::new(NoCompletion),
        || { panic!("F5 no owner means no resolver") }
    ));
    assert!(
        !fixture.dispatch.ensure_terminal_reconciliation(
            true,
            Some(fixture.owner.clone()),
            Arc::new(NoCompletion),
            || None,
        ),
        "F6"
    );
    assert!(
        fixture.dispatch.terminal_reconciler.get().is_none(),
        "F5/F6 empty"
    );
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fixture.dispatch.ensure_terminal_reconciliation(
            true,
            Some(fixture.owner.clone()),
            Arc::new(NoCompletion),
            || panic!("controlled terminal initializer panic"),
        )
    }));
    assert!(panic.is_err(), "F7 original unwind");
    assert!(
        fixture.dispatch.terminal_reconciler.get().is_none(),
        "F7 empty"
    );
    assert!(
        fixture.dispatch.ensure_terminal_reconciliation(
            true,
            Some(fixture.owner.clone()),
            Arc::new(NoCompletion),
            || Some(fixture.resolver.clone()),
        ),
        "F8 successful retry"
    );
    assert!(!fixture.dispatch.ensure_terminal_reconciliation(
        true,
        Some(fixture.owner.clone()),
        Arc::new(NoCompletion),
        || panic!("F8 a successful initializer is immutable"),
    ));
    assert!(fixture.dispatch.begin_drain().await.is_none());
}

#[test]
fn preparation_role_retains_one_pure_compiler_without_process_services() {
    // C1 application and Worker transport select their preparation role from
    // the same process; C2 an unrelated root has equal stateless behavior;
    // C3 the process owner drops while its compiler remains retained.
    // R1=C1 -> exact Arc identity shared; R2=C2 -> different identity, so the
    // existing mount validator still refuses foreign instances; R3=C3 ->
    // process services released, original compiler retained without replacement.
    let process = Arc::new(
        coordinator_fixture(awaken_deployment_config::ClaimPool::RemoteWorkersOnly).dispatch,
    );
    let weak = Arc::downgrade(&process);
    let application: Arc<dyn awaken_session_execution_contract::SessionPreparationPlacement> =
        process.preparation_placement();
    let transport: Arc<dyn awaken_session_execution_contract::SessionPreparationPlacement> =
        process.preparation_placement();
    assert!(
        Arc::ptr_eq(&application, &transport),
        "R1 one original pure compiler"
    );
    let other = coordinator_fixture(awaken_deployment_config::ClaimPool::RemoteWorkersOnly);
    let foreign: Arc<dyn awaken_session_execution_contract::SessionPreparationPlacement> =
        other.dispatch.preparation_placement();
    assert!(
        !Arc::ptr_eq(&application, &foreign),
        "R2 independent root remains distinguishable"
    );
    drop(process);
    assert!(
        weak.upgrade().is_none(),
        "R3 preparation role retains no service owner"
    );
    assert!(
        Arc::ptr_eq(&application, &transport),
        "R3 original compiler identity survives"
    );
}
