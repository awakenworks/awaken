//! Queue-depth sampling is one observer, independent of pool width and the fallback poll.
use super::*;

#[tokio::test(start_paused = true)]
async fn queue_depth_sampling_is_independent_of_pool_width_and_fallback_poll() {
    // Causes: C1 a width-16 idle pool starts; C2 fallback poll is 120s; C3 the
    // independent depth-sample deadline reaches 60s. Effects: E1 one startup
    // exact depth read for the whole pool; E2 no extra claim or depth read from
    // its width; E3 a second depth read at 60s without an ordinary claim tick.
    // Decision table: R1=C1+C2+!C3 -> E1+E2; R2=C1+C2+C3 -> E3. A remote
    // adapter's maintenance-disabled partition is covered by the existing
    // maintenance ownership test; the sampled gauge owns no queue state.
    let store = Arc::new(FlakyDispatchStore::new(0, system_clock()));
    let worker = Arc::new(DispatchWorker::new(
        text_runtime(),
        store.clone(),
        Arc::new(MemoryCommitCoordinator::new(system_clock())),
        "depth-pool",
    ));
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        system_clock(),
        "depth-pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(120),
            terminal_reconciliation_interval: None,
            ..Default::default()
        },
        Arc::new(FlakyResolver { worker }),
        16,
        Arc::new(BlackholeWake),
    );
    for _ in 0..200 {
        if store.depth_reads() == 1 && store.claim_attempts() >= 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(store.depth_reads(), 1, "R1/E1");
    assert_eq!(store.claim_attempts(), 1, "R1/E2");
    tokio::time::advance(Duration::from_secs(60)).await;
    for _ in 0..100 {
        if store.depth_reads() == 2 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(store.depth_reads(), 2, "R2/E3");
    assert_eq!(store.claim_attempts(), 1, "R2/E3 no fallback claim poll");
    pool.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn hosted_pool_does_not_duplicate_its_depth_observer() {
    // Causes: C1 a width-16 pool is locally claimable; C2 a hosting adapter
    // owns the depth observation; C3 the 60-second sample deadline passes.
    // Effects: E1 the pool still probes Dispatch; E2 it never invokes the
    // optional exact depth query. Rule P1=C1+C2+C3 -> E1+E2. The store remains
    // the sole queue authority and hosted scaling reads that same store.
    let store = Arc::new(FlakyDispatchStore::new(0, system_clock()));
    let worker = Arc::new(DispatchWorker::new(
        text_runtime(),
        store.clone(),
        Arc::new(MemoryCommitCoordinator::new(system_clock())),
        "hosted-depth-pool",
    ));
    let pool = DispatchPool::spawn_with_wake(
        store.clone(),
        system_clock(),
        "hosted-depth-pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(120),
            terminal_reconciliation_interval: None,
            sample_dispatch_queue_depth: false,
            ..Default::default()
        },
        Arc::new(FlakyResolver { worker }),
        16,
        Arc::new(BlackholeWake),
    );
    for _ in 0..200 {
        if store.claim_attempts() >= 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(store.claim_attempts(), 1, "P1/E1");
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert_eq!(store.depth_reads(), 0, "P1/E2");
    pool.shutdown().await;
}
