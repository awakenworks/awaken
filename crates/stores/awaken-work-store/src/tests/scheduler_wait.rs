//! T38-C01: SQLite authority waits on the one-connection pool are asynchronous
//! (they never occupy the only runtime thread), end at a deadline or pool
//! closure, and do not stop a peer authority from making progress.
use super::*;
use awaken_executable_environment_contract::WorkQueueError;

const TIMER: Duration = Duration::from_millis(10);
/// Far above a scheduled 10 ms timer, far below the 300 ms blocking mutant.
const PROMPT: Duration = Duration::from_millis(200);

/// Oracle check: how long a 10 ms timer takes to fire on this runtime.
async fn timer_latency() -> Duration {
    let started = Instant::now();
    // awaken-allow(wait): the timer itself is the measured subject; its prompt firing proves the runtime thread is free.
    tokio::time::timeout(Duration::from_secs(2), tokio::time::sleep(TIMER))
        .await
        .expect("the timer eventually fires");
    started.elapsed()
}

fn session() -> awaken_session_types::SessionId {
    awaken_session_types::SessionId::from("session")
}

/// Q1 on a current-thread runtime (one thread for every task), a Session
/// acquire queued behind the held connection leaves the timer prompt; after
/// release the acquire commits exactly one lease. NC: a mutant waiter that
/// blocks the thread (the shape of a synchronous database wait) delays the
/// same timer past the oracle threshold, so the check detects it.
#[tokio::test(flavor = "current_thread")]
async fn sqlite_authority_waiter_keeps_a_current_thread_runtime_schedulable() {
    let queue = Arc::new(q().await);
    let held = queue.pool.acquire().await.expect("Q1 connection held");
    let waiter = tokio::spawn({
        let queue = Arc::clone(&queue);
        async move { queue.acquire_session("env", &session(), "owner", 0).await }
    });
    tokio::task::yield_now().await;

    let latency = timer_latency().await;
    assert!(
        latency < PROMPT,
        "Q1 timer delayed {latency:?} by a queued waiter"
    );
    assert!(!waiter.is_finished(), "Q1 the waiter is still queued");

    drop(held);
    let lease = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("Q1 the waiter completes after release")
        .expect("Q1 waiter task")
        .expect("Q1 acquire")
        .expect("Q1 Session lease");
    assert_eq!(lease.session_id(), "session", "Q1");

    // Negative control: a blocking wait on the one runtime thread.
    let blocking = tokio::spawn(async {
        // awaken-allow(wait): NC mutant that simulates a synchronous database wait occupying the runtime thread.
        std::thread::sleep(Duration::from_millis(300));
    });
    let mutant_latency = timer_latency().await;
    tokio::time::timeout(Duration::from_secs(5), blocking)
        .await
        .expect("NC mutant finishes")
        .unwrap();
    assert!(
        mutant_latency >= PROMPT,
        "NC the latency oracle detects a blocking waiter: {mutant_latency:?}"
    );
}

/// C1 the store's one connection is held; C2 a Session acquire is queued; C3
/// the pool closes. E1 the queued waiter returns a typed storage error within
/// a bounded deadline instead of hanging; E2 the close completes once the held
/// connection returns; E3 an independent peer authority still grants work.
#[tokio::test]
async fn closing_the_pool_ends_a_queued_authority_waiter_and_peers_progress() {
    let queue = Arc::new(q().await);
    let held = queue.pool.acquire().await.expect("C1");
    let waiter = tokio::spawn({
        let queue = Arc::clone(&queue);
        async move { queue.acquire_session("env", &session(), "owner", 0).await }
    });
    tokio::task::yield_now().await;
    let closing = tokio::spawn({
        let queue = Arc::clone(&queue);
        async move { queue.pool.close().await }
    });

    let refused = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("E1 the waiter ends at pool closure")
        .expect("E1 waiter task");
    assert!(
        matches!(refused, Err(WorkQueueError::Storage(_))),
        "E1 typed storage failure: {refused:?}"
    );

    drop(held);
    tokio::time::timeout(Duration::from_secs(5), closing)
        .await
        .expect("E2 close completes after release")
        .expect("E2 close task");

    let peer = q().await;
    let peer_lease = peer
        .acquire_session("env", &session(), "peer-owner", 0)
        .await
        .expect("E3 peer acquire")
        .expect("E3 peer lease");
    assert_eq!(peer_lease.session_id(), "session", "E3");
}

/// An authority waiter queued behind a wedged connection must fail within one
/// Session lease TTL; the pool's admission wait is 24 hours.
#[tokio::test]
#[ignore = "product gap: SQLite admission wait is 24h (awaken-sqlite-pool pool.rs:29), far beyond the Session lease TTL"]
async fn a_wedged_connection_fails_authority_waiters_within_one_lease_ttl() {
    let queue = Arc::new(q().await);
    let _wedged = queue.pool.acquire().await.expect("wedged connection");
    let waiter = tokio::spawn({
        let queue = Arc::clone(&queue);
        async move { queue.acquire_session("env", &session(), "owner", 0).await }
    });
    let bounded = tokio::time::timeout(Duration::from_millis(LEASE_TTL_MS + 5_000), waiter).await;
    let outcome = bounded.expect("the waiter itself reaches its deadline");
    assert!(
        matches!(outcome, Ok(Err(WorkQueueError::Storage(_)))),
        "the waiter fails with a typed storage error"
    );
}
