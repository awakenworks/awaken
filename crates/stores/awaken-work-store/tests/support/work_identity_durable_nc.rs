//! T37-C02: the identity allocator is a durable, transactional authority shared
//! by genuinely concurrent clients -- not a process counter and not one
//! serialised connection.
//!
//! Cause/effect graph: C1 two independent handles (two SQLite connections to
//! one file; two PostgreSQL pools) are *both inside* `enqueue_session` at once;
//! C2 the Environment is removed (no row can serve as the counter); C3 every
//! handle closes and a fresh one reopens the store. Effects: E1 all identities
//! are distinct; E2 after reopen the next identity is new (no reset); E3 every
//! retired identity stays fenced (ack/stop/heartbeat NotFound).
//!
//! Negative controls (executed): N1 the concurrency probe reports a harness
//! that serialises the two clients behind one lock as non-concurrent; N2 an
//! in-process counter that restarts with the process re-mints a retired id, and
//! [`reopen_verdict`] rejects it.

use super::*;
use awaken_executable_environment_contract::{HeartbeatResult, LeaseHeartbeat, WorkMutationResult};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[derive(Default)]
struct Probe {
    inflight: AtomicUsize,
    max: AtomicUsize,
}

const ENV: &str = "raced";
const ROUNDS: usize = 8;

/// C1: each round releases one allocation per handle on its own task.
///
/// `enqueue_session` is idempotent per `(env, session)`, so every phase names
/// its own Sessions (`phase`); otherwise a replay would return an earlier id and
/// E1 could not distinguish a duplicate mint from an idempotent replay.
async fn racing_allocations<Q: WorkBackend + 'static>(
    a: &Arc<Q>,
    b: &Arc<Q>,
    phase: &str,
    serialize: bool,
) -> (Vec<String>, usize) {
    let probe = Arc::new(Probe::default());
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let mut ids = Vec::new();
    for round in 0..ROUNDS {
        let inside = Arc::new(tokio::sync::Barrier::new(2));
        let spawn = |queue: Arc<Q>, session: String| {
            let (probe, lock, inside) = (probe.clone(), lock.clone(), inside.clone());
            tokio::spawn(async move {
                let _serialized = if serialize {
                    Some(lock.lock().await)
                } else {
                    None
                };
                let now = probe.inflight.fetch_add(1, Ordering::SeqCst) + 1;
                probe.max.fetch_max(now, Ordering::SeqCst);
                if !serialize {
                    inside.wait().await;
                }
                let id = queue
                    .enqueue_session(ENV, &awaken_session_types::SessionId::new(session))
                    .await
                    .expect("allocation");
                probe.inflight.fetch_sub(1, Ordering::SeqCst);
                id
            })
        };
        let left = spawn(a.clone(), format!("{phase}-left-{round}"));
        let right = spawn(b.clone(), format!("{phase}-right-{round}"));
        ids.push(left.await.expect("left task"));
        ids.push(right.await.expect("right task"));
    }
    (ids, probe.max.load(Ordering::SeqCst))
}

/// E2: an identity minted after reopen is none of the retired ones.
fn reopen_verdict(retired: &[String], next: &str) -> Result<(), String> {
    if retired.iter().any(|id| id == next) {
        return Err(format!("E2 reopen re-minted retired identity {next}"));
    }
    Ok(())
}

/// N2: a counter held only in process memory, recreated with the process.
struct InProcessCounter {
    generation: String,
    next: AtomicU64,
}

impl InProcessCounter {
    fn started_for(observed: &str) -> Self {
        Self {
            generation: observed[6..38].to_owned(),
            next: AtomicU64::new(1),
        }
    }

    fn allocate(&self) -> String {
        let sequence = self.next.fetch_add(1, Ordering::SeqCst);
        format!("work_g{}{sequence:016x}", self.generation)
    }
}

async fn concurrent_then_reopen<Q, R, F>(a: Arc<Q>, b: Arc<Q>, reopen: R)
where
    Q: WorkBackend + 'static,
    R: FnOnce() -> F,
    F: std::future::Future<Output = Q>,
{
    let (serialized, serialized_max) = racing_allocations(&a, &b, "serialized", true).await;
    assert_eq!(
        serialized_max, 1,
        "N1: the probe reports a lock-serialised harness as non-concurrent"
    );
    let (concurrent, concurrent_max) = racing_allocations(&a, &b, "concurrent", false).await;
    assert_eq!(concurrent_max, 2, "C1 both handles were inside the store");
    let mut retired: Vec<String> = serialized;
    retired.extend(concurrent);
    let unique: std::collections::BTreeSet<_> = retired.iter().collect();
    assert_eq!(unique.len(), ROUNDS * 4, "E1 distinct identities");
    a.remove_env(ENV).await.unwrap();
    drop((a, b));

    let reopened = reopen().await;
    let next = reopened
        .enqueue_session(ENV, &"after-reopen".into())
        .await
        .unwrap();
    reopen_verdict(&retired, &next).unwrap_or_else(|failure| panic!("{failure}"));
    for id in &retired {
        assert!(
            matches!(
                reopened.ack(ENV, id, "worker").await.unwrap(),
                WorkMutationResult::NotFound
            ),
            "E3 retired {id} ack"
        );
        assert!(
            matches!(
                reopened.stop(ENV, id, "worker").await.unwrap(),
                WorkMutationResult::NotFound
            ),
            "E3 retired {id} stop"
        );
        assert!(
            matches!(
                reopened
                    .heartbeat(ENV, id, "worker", LeaseHeartbeat::unconditional())
                    .await
                    .unwrap(),
                HeartbeatResult::NotFound
            ),
            "E3 retired {id} heartbeat"
        );
    }
    assert_eq!(
        reopened.list(ENV).await.unwrap().len(),
        1,
        "E3 only the new row"
    );

    // N2: the same verdict rejects a process-local counter after "restart".
    let mutant = InProcessCounter::started_for(&retired[0]);
    let before_restart = mutant.allocate();
    drop(mutant);
    let restarted = InProcessCounter::started_for(&before_restart);
    assert!(
        reopen_verdict(&retired, &restarted.allocate()).is_err(),
        "N2: an in-process counter resets and the verdict detects the reuse"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_allocation_is_concurrent_durable_and_fences_retired_ids_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("concurrent-allocation.db");
    let path = path.to_str().unwrap().to_owned();
    let open = |path: String| async move {
        SqliteWorkQueue::open(&path, awaken_time::system_clock())
            .await
            .unwrap()
    };
    let a = Arc::new(open(path.clone()).await);
    let b = Arc::new(open(path.clone()).await);
    concurrent_then_reopen(a, b, || open(path)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_allocation_is_concurrent_durable_and_fences_retired_ids_after_reopen() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let fixture = WorkPostgresFixture::new(&url, 4).await;
    let a = Arc::new(
        PostgresWorkQueue::with_pool(fixture.pool.clone(), awaken_time::system_clock())
            .await
            .unwrap(),
    );
    let b = Arc::new(
        PostgresWorkQueue::connect_existing(&fixture.scoped_url, awaken_time::system_clock())
            .await
            .unwrap(),
    );
    let scoped = fixture.scoped_url.clone();
    concurrent_then_reopen(a, b, || async move {
        PostgresWorkQueue::connect_existing(&scoped, awaken_time::system_clock())
            .await
            .unwrap()
    })
    .await;
    fixture.cleanup().await;
}

#[test]
fn postgres_allocation_is_concurrent_durable_and_fences_retired_ids_after_reopen_fixture_admission()
{
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "durable::nc::postgres_allocation_is_concurrent_durable_and_fences_retired_ids_after_reopen",
    );
}
