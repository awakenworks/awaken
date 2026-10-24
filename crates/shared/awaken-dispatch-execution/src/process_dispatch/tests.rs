// @formal evidence worker_drain.bounded_cleanup_eventually_exits_without_fabricating_quiescence
// @formal evidence worker_drain.grace_expiry_retains_cancellation_observer
//! Decision tables for the process dispatch: pool start, coordinator-side
//! terminal reconciliation start and lifetime, and the drain fence deadline.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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

fn coordinator_fixture() -> CoordinatorFixture {
    let clock = awaken_time::system_clock();
    let store = Arc::new(awaken_dispatch::MemoryDispatchStore::new(clock.clone()));
    let owner = Arc::new(AnyCoordinatorDispatch::from_durable_dispatch(store.clone()));
    let dispatch = ProcessDispatch::new(
        ExecutionTopology {
            ingress: crate::RunIngress::Durable,
            local: crate::LocalExecution::RemoteWorkersOnly,
            lease_owner: crate::LeaseOwnerId::new("coordinator").unwrap(),
            lost_wake_recovery_bound: std::time::Duration::from_millis(5),
            queue_depth: crate::QueueDepthSource::External,
        },
        clock,
        Arc::new(NoCompletion),
        DispatchHint::new(Arc::new(crate::LocalWakeSignal::new())),
    );
    let scans = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let resolver = Arc::new(ScanStateResolver {
        queue: Arc::new(AnyDispatchStore::from_dispatch(store)),
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
    let fixture = coordinator_fixture();
    let scan_state = Arc::downgrade(fixture.dispatch.terminal_reconciliation());
    let resolver = fixture.resolver.clone() as Arc<dyn WorkerResolver<AnyDispatchStore>>;
    assert!(
        fixture.dispatch.ensure_terminal_reconciliation(
            true,
            Some(fixture.owner.clone()),
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

    let fixture = coordinator_fixture();
    let resolver = fixture.resolver.clone() as Arc<dyn WorkerResolver<AnyDispatchStore>>;
    assert!(
        fixture.dispatch.ensure_terminal_reconciliation(
            true,
            Some(fixture.owner.clone()),
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
        !fixture
            .dispatch
            .ensure_terminal_reconciliation(true, Some(fixture.owner), || {
                panic!("T4/E4 a stopped loop builds no second resolver")
            }),
        "T4/E4"
    );
}
