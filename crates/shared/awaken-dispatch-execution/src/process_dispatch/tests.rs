// @formal evidence worker_drain.bounded_cleanup_eventually_exits_without_fabricating_quiescence
// @formal evidence worker_drain.grace_expiry_retains_cancellation_observer
//! Decision tables for the process dispatch: pool start, coordinator-side
//! terminal reconciliation start, and the drain fence deadline.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

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
