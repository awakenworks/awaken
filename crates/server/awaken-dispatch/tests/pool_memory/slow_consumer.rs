//! T38-C02 queue and slow consumer: a width-2 DispatchPool is saturated by
//! consumers blocked inside an external dependency, more Runs keep arriving,
//! then the pool drains and shuts down.
//!
//! Oracles: execution growth stays within the configured width; Runs beyond
//! it are classified as admitted-but-pending (backpressure), never dropped;
//! a drain admits nothing new; shutdown completes within its budget once the
//! dependency returns. Product-gap reproducers pin the unbounded LiveInbox and
//! the deadline-free `DispatchPool::shutdown`.

use super::*;
use awaken_dispatch::DispatchState;

const WIDTH: usize = 2;
const RUNS: usize = 5;

struct Saturated {
    pool: DispatchPool<MemoryDispatchStore>,
    store: Arc<MemoryDispatchStore>,
    commit: Arc<MemoryCommitCoordinator>,
    release: Arc<tokio::sync::Semaphore>,
    ran: Arc<std::sync::atomic::AtomicUsize>,
}

async fn saturated_pool() -> Saturated {
    use std::sync::atomic::Ordering;
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, ran) = harness::all_calls_blocking_tool_runtime_with_metrics(
        release.clone(),
        Arc::new(awaken_runtime_contract::metrics::NoopRecorder),
    );
    let store = Arc::new(MemoryDispatchStore::new(system_clock()));
    let commit = Arc::new(MemoryCommitCoordinator::new(system_clock()));
    let workers = (0..=RUNS)
        .map(|index| {
            (
                format!("slow-thread-{index}"),
                worker_over(runtime.clone(), store.clone(), commit.clone()),
            )
        })
        .collect();
    let pool = DispatchPool::spawn(
        store.clone(),
        system_clock(),
        "slow-pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig::default(),
        Arc::new(MapResolver {
            store: store.clone(),
            workers,
        }),
        WIDTH,
    )
    .expect("valid test Pool binding");
    for index in 0..RUNS {
        pool.submit(activation_on(
            &format!("slow-run-{index}"),
            &format!("slow-thread-{index}"),
        ))
        .await
        .unwrap();
    }
    assert!(
        wait_for(|| ran.load(Ordering::SeqCst) == WIDTH).await,
        "the pool filled its width"
    );
    Saturated {
        pool,
        store,
        commit,
        release,
        ran,
    }
}

async fn states(store: &MemoryDispatchStore) -> (usize, usize) {
    let rows = store.list_dispatches().await.unwrap();
    let pending = rows
        .iter()
        .filter(|row| row.state == DispatchState::Pending)
        .count();
    (rows.len(), pending)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saturated_pool_backpressures_without_dropping_and_drains_within_budget() {
    use std::sync::atomic::Ordering;
    // | Row | condition                         | effect                                  |
    // | Q1  | 5 Runs, width 2, consumers stuck  | 2 executing, 3 Pending, 0 dropped       |
    // | Q2  | sustained stall                   | no growth past the width                |
    // | Q3  | drain, then one more submission   | admitted durably, never started         |
    // | Q4  | dependency returns, shutdown      | within budget; executing Runs end       |
    let saturated = saturated_pool().await;
    let Saturated {
        pool,
        store,
        commit,
        release,
        ran,
    } = saturated;

    // Q1 + Q2
    // awaken-allow(wait): the fixture already waited for WIDTH starts; this
    // holds the stall open to prove no consumer starts beyond the width.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        ran.load(Ordering::SeqCst),
        WIDTH,
        "Q2 no consumer beyond the width"
    );
    assert_eq!(pool.concurrency(), WIDTH, "Q1 configured width");
    assert!(pool.in_flight() as usize <= WIDTH, "Q2 in-flight bound");
    assert_eq!(
        states(&store).await,
        (RUNS, RUNS - WIDTH),
        "Q1 every admitted Run is retained; the excess is Pending"
    );

    // Q3
    let _fence = pool.begin_drain().await;
    pool.submit(activation_on(
        "slow-run-late",
        &format!("slow-thread-{RUNS}"),
    ))
    .await
    .unwrap();
    // awaken-allow(wait): a non-occurrence window; the drained pool must not
    // start the late durable Run while the stalled consumers still hold it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        ran.load(Ordering::SeqCst),
        WIDTH,
        "Q3 the drain starts nothing new"
    );
    assert_eq!(
        states(&store).await,
        (RUNS + 1, RUNS + 1 - WIDTH),
        "Q3 late Run kept"
    );

    // Q4
    release.add_permits(RUNS + 1);
    tokio::time::timeout(Duration::from_secs(10), pool.shutdown())
        .await
        .expect("Q4 shutdown finishes within its budget once the dependency returns");
    let ended = (0..RUNS)
        .filter(|index| {
            CommittedThreadView::run(commit.as_ref(), &RunId(format!("slow-run-{index}")))
                .is_some_and(|run| matches!(run.state, RunState::Ended(_)))
        })
        .count();
    assert_eq!(ended, WIDTH, "Q4 the executing Runs completed");
    assert_eq!(
        states(&store).await,
        (RUNS + 1 - WIDTH, RUNS + 1 - WIDTH),
        "Q4 nothing admitted was silently dropped"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "product gap: DispatchPool::shutdown awaits every drain with no deadline, so a dependency that never returns blocks shutdown forever"]
async fn shutdown_is_bounded_when_a_dependency_never_returns() {
    let saturated = saturated_pool().await;
    // The dependency never returns: no permits are ever added.
    let shutdown = tokio::time::timeout(Duration::from_secs(2), saturated.pool.shutdown()).await;
    assert!(
        shutdown.is_ok(),
        "shutdown must finish (or abandon with a classified result) within its budget"
    );
    drop(saturated.release);
}

#[test]
#[ignore = "product gap: LiveInbox::offer is unbounded; a stalled consumer lets the queue grow without backpressure"]
fn live_inbox_applies_backpressure_to_a_stalled_consumer() {
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    let inbox = awaken_runtime_contract::LiveInbox::new();
    let refused = (0..100_000).any(|index| {
        !matches!(
            inbox.offer(Message::text(
                MessageId(format!("m{index}")),
                Role::User,
                "nobody drains this"
            )),
            awaken_runtime_contract::live_inbox::Offer::Accepted(_)
        )
    });
    assert!(refused, "100000 undrained messages were all accepted");
}
