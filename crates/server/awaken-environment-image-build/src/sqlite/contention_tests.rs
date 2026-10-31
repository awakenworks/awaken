//! T38-C01: image-build SQLite waits on a single-thread runtime stay
//! asynchronous and end at pool closure instead of hanging.
use std::sync::Arc;
use std::time::{Duration, Instant};

use awaken_executable_environment_contract::EnvironmentImageBuildError;
use awaken_sqlite_pool::EmbeddedSqlite;

use super::{RecordBackend, open_backend};

const PROMPT: Duration = Duration::from_millis(200);

async fn timer_latency() -> Duration {
    let started = Instant::now();
    // awaken-allow(wait): the timer itself is the measured subject; its prompt firing proves the runtime thread is free.
    tokio::time::timeout(
        Duration::from_secs(2),
        tokio::time::sleep(Duration::from_millis(10)),
    )
    .await
    .expect("the timer eventually fires");
    started.elapsed()
}

/// B1 current-thread runtime: a repository read queued behind the held
/// connection leaves the timer prompt and completes after release. B2 pool
/// closure ends a queued read with a typed storage error. NC a blocking
/// mutant waiter on the same thread delays the timer past the threshold.
#[tokio::test(flavor = "current_thread")]
async fn image_build_waiters_stay_async_and_end_at_pool_closure() {
    let backend = Arc::new(open_backend(EmbeddedSqlite::memory()).await.unwrap());
    let held = backend.pool.acquire().await.expect("B1 connection held");
    let read = tokio::spawn({
        let backend = Arc::clone(&backend);
        async move { backend.get("missing").await }
    });
    tokio::task::yield_now().await;
    let latency = timer_latency().await;
    assert!(latency < PROMPT, "B1 timer delayed {latency:?}");
    assert!(!read.is_finished(), "B1 the read is still queued");
    drop(held);
    let found = tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .expect("B1 read completes after release")
        .expect("B1 read task")
        .expect("B1 read");
    assert!(found.is_none(), "B1");

    let held = backend.pool.acquire().await.expect("B2 connection held");
    let queued = tokio::spawn({
        let backend = Arc::clone(&backend);
        async move { backend.get("missing").await }
    });
    tokio::task::yield_now().await;
    let closing = tokio::spawn({
        let backend = Arc::clone(&backend);
        async move { backend.pool.close().await }
    });
    let refused = tokio::time::timeout(Duration::from_secs(5), queued)
        .await
        .expect("B2 the queued read ends at closure")
        .expect("B2 read task");
    assert!(
        matches!(refused, Err(EnvironmentImageBuildError::Storage(_))),
        "B2 typed storage failure: {:?}",
        refused.as_ref().err()
    );
    drop(held);
    tokio::time::timeout(Duration::from_secs(5), closing)
        .await
        .expect("B2 close completes after release")
        .expect("B2 close task");

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
