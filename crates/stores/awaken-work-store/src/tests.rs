//! Durable Work queue tests and owned SQLite file lifetimes.

use super::LEASE_TTL_MS;
use super::*;
use awaken_executable_environment_contract::{WorkPayload, WorkState};
use awaken_sqlite_pool::EmbeddedSqlite;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

async fn q() -> SqliteWorkQueue {
    // Claims in these decision tables use the logical epoch-zero timeline.
    SqliteWorkQueue::open_in_memory(Arc::new(awaken_lease_testkit::ManualClock::new(0)))
        .await
        .unwrap()
}

struct SqliteFileFixture {
    directory: tempfile::TempDir,
    path: PathBuf,
}

impl SqliteFileFixture {
    fn new(label: &str) -> Self {
        let directory = tempfile::Builder::new()
            .prefix(label)
            .tempdir()
            .expect("create a uniquely owned Work SQLite directory");
        let path = directory.path().join("queue.db");
        Self { directory, path }
    }

    fn path(&self) -> &str {
        self.path.to_str().expect("UTF-8 Work SQLite fixture path")
    }

    async fn settled(&self) {
        EmbeddedSqlite::file(&self.path).settled().await;
    }

    async fn close(self) {
        self.settled().await;
        let directory = self.directory.path().to_path_buf();
        self.directory
            .close()
            .expect("remove Work SQLite directory after every connection closes");
        assert!(!directory.exists(), "the owned directory must be removed");
    }
}

#[tokio::test]
async fn sqlite_file_fixtures_isolate_live_work_and_remove_only_their_owned_directory() {
    // F1 equal fixture labels, Environment and Session identities with two live
    // databases -> each can claim its own Work. A reused PID/thread directory
    // joins the databases instead, so the first active lease rejects claim two.
    // F2 retire database two while database one's lease remains active -> only
    // two disappears physically, and one's original lease still blocks reclaim.
    // F3 drop one's final store and await its close witness -> remove one too.
    let first = SqliteFileFixture::new("awaken-work-token-");
    let second = SqliteFileFixture::new("awaken-work-token-");
    let first_queue = SqliteWorkQueue::open(first.path(), system_clock())
        .await
        .expect("open first database");
    let second_queue = SqliteWorkQueue::open(second.path(), system_clock())
        .await
        .expect("open second database");
    let session = awaken_session_types::SessionId::from("same-session");
    let first_id = first_queue.enqueue_session("env", &session).await.unwrap();
    let second_id = second_queue.enqueue_session("env", &session).await.unwrap();
    assert_eq!(
        first_queue
            .claim("env", "owner", 0)
            .await
            .unwrap()
            .expect("F1 first database has its own claim")
            .id,
        first_id
    );
    assert_eq!(
        second_queue
            .claim("env", "owner", 0)
            .await
            .unwrap()
            .expect("F1 the first database's live lease cannot block the second")
            .id,
        second_id
    );
    drop(second_queue);
    second.close().await;
    assert!(first.path.exists(), "F2 the live neighbor must remain");
    assert_eq!(
        first_queue
            .get("env", &first_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        WorkState::Active,
        "F2 the original claim survives the neighbor's physical removal"
    );
    assert!(
        first_queue
            .claim("env", "other-owner", 1)
            .await
            .unwrap()
            .is_none()
    );
    drop(first_queue);
    first.close().await;
}

/// Work-store scheduler cause/effect graph: C1 an inspector owns the
/// queue's one connection; C2 the authoritative Session-acquire path and a
/// background stats read arrive concurrently on a two-worker Tokio runtime.
/// Effects: E1 neither waiter blocks a Tokio worker, E2 a 10 ms authority
/// timer fires while the inspector still holds the connection, and E3 both
/// queue operations complete against the one durable authority after release.
///
/// | Rule | Connection owner | Async waiters | Effect |
/// |---|---|---|---|
/// | Q1 | inspector | acquire + stats | E1 + E2 + E3 |
///
/// This adapter-level rule fails if any WorkQueue operation bypasses the
/// store's one-connection pool and blocks a runtime thread while it waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_work_authority_waiters_never_block_runtime_timers() {
    let queue = Arc::new(q().await);
    let held = queue.pool.acquire().await.expect("Q1 connection held");

    let started = Instant::now();
    let timer = tokio::spawn(async move {
        // awaken-allow(wait): Q1 measures the timer itself; its prompt firing proves runtime workers are not blocked.
        tokio::time::sleep(Duration::from_millis(10)).await;
        started.elapsed()
    });
    let acquire = tokio::spawn({
        let queue = Arc::clone(&queue);
        async move {
            queue
                .acquire_session(
                    "env",
                    &awaken_session_types::SessionId::from("session"),
                    "owner",
                    0,
                )
                .await
        }
    });
    let stats = tokio::spawn({
        let queue = Arc::clone(&queue);
        async move { queue.stats("env", 0).await }
    });
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let elapsed = tokio::time::timeout(Duration::from_millis(100), timer)
        .await
        .expect("Q1/E1-E2 authority timer remains schedulable")
        .expect("Q1 timer task");
    assert!(
        elapsed < Duration::from_millis(100),
        "Q1/E2 timer fired after {elapsed:?}; WorkQueue blocked runtime workers"
    );

    drop(held);
    let lease = acquire
        .await
        .expect("Q1 acquire task")
        .expect("Q1/E3 acquire")
        .expect("Q1/E3 Session lease");
    assert_eq!(lease.session_id(), "session", "Q1/E3");
    stats.await.expect("Q1 stats task").expect("Q1/E3 stats");
    let final_stats = queue.stats("env", 0).await.expect("Q1/E3 final stats");
    assert_eq!(
        final_stats.depth + final_stats.pending,
        1,
        "Q1/E3 one durable row after both unordered waiters complete"
    );
}

#[cfg(test)]
#[tokio::test]
async fn sqlite_storage_outage_is_typed_for_every_queue_operation() {
    // Cause/effect graph: C1 the queue schema is available; C2 the backing
    // schema becomes unavailable; C3 an operation would otherwise return
    // None/empty. Effects: E1 C1 preserves ordinary success/not-found
    // semantics; E2 C2 returns WorkQueueError::Storage for every read and
    // mutation; E3 no C2 path panics or degrades into C3.
    //
    // | Rule | storage | logical item | operation result |
    // | S1 | healthy | present/absent | success/None |
    // | S2 | unavailable | any | Storage error |
    // The ordinary backend and conformance tests own S1. This test owns S2
    // across the complete port after invalidating the SQLite schema.
    let q = q().await;
    sqlx::raw_sql("DROP TABLE work_queue_item")
        .execute(&q.pool)
        .await
        .unwrap();
    let is_storage = |result: Result<(), WorkQueueError>| {
        assert!(matches!(result, Err(WorkQueueError::Storage(_))));
    };

    is_storage(
        q.enqueue_session("env", &awaken_session_types::SessionId::from("session"))
            .await
            .map(|_| ()),
    );
    is_storage(q.enqueue_healthcheck("env").await.map(|_| ()));
    is_storage(q.ensure_healthcheck("env").await.map(|_| ()));
    is_storage(q.list("env").await.map(|_| ()));
    is_storage(q.get("env", "work").await.map(|_| ()));
    is_storage(q.claim("env", "worker", 0).await.map(|_| ()));
    is_storage(
        q.claim_with_reclaim("env", "worker", "worker", 1, Some(1))
            .await
            .map(|_| ()),
    );
    is_storage(q.ack("env", "work", "worker").await.map(|_| ()));
    is_storage(
        q.heartbeat("env", "work", "worker", 0, LeaseHeartbeat::unconditional())
            .await
            .map(|_| ()),
    );
    is_storage(q.stop("env", "work", "worker").await.map(|_| ()));
    is_storage(
        q.update_metadata("env", "work", BTreeMap::new())
            .await
            .map(|_| ()),
    );
    is_storage(q.stats("env", 0).await.map(|_| ()));
    is_storage(q.remove_env("env").await);
}

#[tokio::test]
async fn healthcheck_seed_self_references_and_survives_reload() {
    let q = q().await;
    let id = q.enqueue_healthcheck("env_a").await.expect("enqueue");
    let w = q.get("env_a", &id).await.expect("get").expect("seeded");
    assert_eq!(w.state, WorkState::Queued);
    assert!(matches!(w.data, WorkPayload::HealthCheck { id: ref d } if *d == id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_session_dispatch_replay_has_one_sqlite_row() {
    // Cause/effect graph: C1 one durable queue; C2 identical Environment and
    // Session coordinates; C3 concurrent replay. Effects: E1 every caller
    // observes the same canonical id and E2 exactly one row exists. The
    // differing-coordinate rules are covered by the backend conformance table.
    //
    // | Rule | queue | coordinates | concurrency | ids | rows |
    // | C1 | shared | identical | 32 callers | one | one |
    let queue = Arc::new(q().await);
    let mut tasks = Vec::new();
    for _ in 0..32 {
        let queue = queue.clone();
        tasks.push(tokio::spawn(async move {
            queue
                .enqueue_session("env", &awaken_session_types::SessionId::from("session"))
                .await
                .expect("C1 enqueue")
        }));
    }
    let mut ids = Vec::new();
    for task in tasks {
        ids.push(task.await.expect("C1 join"));
    }
    assert!(ids.iter().all(|id| id == &ids[0]), "C1/E1");
    assert_eq!(queue.list("env").await.unwrap().len(), 1, "C1/E2");
}

#[tokio::test]
async fn claim_leases_oldest_and_caps_at_one_active() {
    let q = q().await;
    let w1 = q
        .enqueue_session("env_a", &awaken_session_types::SessionId::from("s1"))
        .await
        .expect("enqueue");
    let _w2 = q
        .enqueue_session("env_a", &awaken_session_types::SessionId::from("s2"))
        .await
        .expect("enqueue");
    let leased = q
        .claim("env_a", "w", 0)
        .await
        .expect("claim query")
        .expect("leases oldest");
    assert_eq!(leased.id, w1);
    assert_eq!(leased.state, WorkState::Active);
    assert!(
        q.claim("env_a", "w", 0).await.expect("claim").is_none(),
        "single active lease"
    );
    q.stop("env_a", &w1, "w")
        .await
        .expect("stop query")
        .into_item()
        .expect("stop");
    assert!(
        q.claim("env_a", "w", 0).await.expect("claim").is_some(),
        "next lease after stop"
    );
}

#[tokio::test]
async fn durable_reclaims_an_expired_lease_on_the_next_poll() {
    let q = q().await;
    let w1 = q
        .enqueue_session("env_a", &awaken_session_types::SessionId::from("s1"))
        .await
        .expect("enqueue");
    assert_eq!(
        q.claim("env_a", "a", 0)
            .await
            .expect("claim query")
            .expect("lease")
            .id,
        w1
    );
    // Live lease caps; a lapsed lease is reclaimed and re-leased.
    assert!(
        q.claim("env_a", "b", 1_000).await.expect("claim").is_none(),
        "live lease caps"
    );
    let reclaimed = q
        .claim("env_a", "b", LEASE_TTL_MS + 1)
        .await
        .expect("claim query")
        .expect("expired lease reclaimed");
    assert_eq!(reclaimed.id, w1);
    assert_eq!(reclaimed.state, WorkState::Active);
}

#[tokio::test]
async fn sqlite_lease_survives_process_store_reopen() {
    let file = SqliteFileFixture::new("awaken-work-lease-");
    let path = file.path();
    {
        let q = SqliteWorkQueue::open(path, system_clock())
            .await
            .expect("open first store");
        q.enqueue_session("env", &awaken_session_types::SessionId::from("session"))
            .await
            .expect("enqueue");
        assert!(
            q.claim("env", "worker-a", 0)
                .await
                .expect("claim")
                .is_some()
        );
    }
    file.settled().await;
    {
        let reopened = SqliteWorkQueue::open(path, system_clock())
            .await
            .expect("reopen store");
        assert!(
            reopened
                .claim("env", "worker-b", LEASE_TTL_MS - 1)
                .await
                .expect("claim")
                .is_none(),
            "a restart must not erase a live lease"
        );
        assert!(
            reopened
                .claim("env", "worker-b", LEASE_TTL_MS)
                .await
                .expect("claim")
                .is_some(),
            "the durable lease is reclaimable at its exact expiry"
        );
    }
    file.close().await;
}

#[tokio::test]
async fn sqlite_session_token_is_single_delivery_epoch_bound_and_reopen_safe() {
    // Cause/effect graph: C1 HealthCheck versus Session payload; C2 token is
    // current/corrupt; C3 store remains open/reopens; C4 lease is live/expires
    // and is reclaimed. Effects: E1 only a Session claim returns a token;
    // E2 only its digest is durable and the clear token authenticates after
    // reopen; E3 wrong and expired tokens fail; E4 reclaim rotates the token
    // and invalidates the prior epoch; C5 a request authenticated immediately
    // before reclaim arrives after the epoch rotated. Constraints: K1 the Work row is the
    // sole lease/token authority; K2 list/get cannot re-deliver cleartext.
    // Decision rows T1=C1(health)->no token, T2=C1(session)+C2(current)->E1/E2,
    // T3=C2(corrupt)->E3, T4=C3(reopen)+C4(live)->E2,
    // T5=C4(expired/reclaimed)->E3/E4, T6=C5->PreconditionFailed without
    // mutating the replacement lease.
    let file = SqliteFileFixture::new("awaken-work-token-");
    let path = file.path();
    let (work_id, token, first_access) = {
        let q = SqliteWorkQueue::open(path, system_clock())
            .await
            .expect("open first store");
        let health = q.enqueue_healthcheck("env").await.unwrap();
        q.enqueue_session("env", &awaken_session_types::SessionId::from("sesn_1"))
            .await
            .unwrap();
        let claimed = q
            .claim_with_session_access(
                "env",
                "owner-a",
                "poller",
                0,
                None,
                &test_session_access_digest(1),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.item.id, health, "T1");
        assert!(claimed.session_access.is_none(), "T1");
        q.stop("env", &health, "owner-a").await.unwrap();
        let claimed = q
            .claim_with_session_access(
                "env",
                "owner-a",
                "poller",
                1,
                None,
                &test_session_access_digest(2),
            )
            .await
            .unwrap()
            .unwrap();
        let token = claimed.session_access.expect("T2 session token");
        let first_access = q
            .authenticate_session_access(&token, 2)
            .await
            .unwrap()
            .expect("T2 current access");
        assert!(
            q.authenticate_session_access(&test_session_access_digest(u64::MAX), 2,)
                .await
                .unwrap()
                .is_none(),
            "T3"
        );
        (claimed.item.id, token, first_access)
    };
    file.settled().await;
    let reopened = SqliteWorkQueue::open(path, system_clock())
        .await
        .expect("reopen store");
    assert!(
        reopened
            .authenticate_session_access(&token, LEASE_TTL_MS)
            .await
            .unwrap()
            .is_some(),
        "T4"
    );
    let replacement = reopened
        .claim_with_session_access(
            "env",
            "owner-b",
            "poller-b",
            LEASE_TTL_MS + 1,
            None,
            &test_session_access_digest(3),
        )
        .await
        .unwrap()
        .unwrap();
    let replacement_token = replacement.session_access.expect("T5 replacement token");
    assert_eq!(replacement.item.id, work_id, "T5 same Work");
    assert!(
        reopened
            .authenticate_session_access(&token, LEASE_TTL_MS + 1)
            .await
            .unwrap()
            .is_none(),
        "T5 stale token"
    );
    let replacement_access = reopened
        .authenticate_session_access(&replacement_token, LEASE_TTL_MS + 1)
        .await
        .unwrap()
        .expect("T5 replacement token");
    assert!(
        matches!(
            reopened
                .stop_with_session_access(&first_access)
                .await
                .unwrap(),
            WorkMutationResult::PreconditionFailed
        ),
        "T6 stale epoch cannot stop replacement"
    );
    assert!(
        reopened
            .heartbeat_with_session_access(
                &replacement_access,
                LEASE_TTL_MS + 2,
                LeaseHeartbeat::unconditional(),
            )
            .await
            .unwrap()
            .into_receipt()
            .is_some(),
        "T6 replacement epoch remains live"
    );
    drop(reopened);
    file.close().await;
}

#[tokio::test]
async fn ack_heartbeat_and_membership_match_the_in_memory_contract() {
    let q = q().await;
    let id = q
        .enqueue_session("env_a", &awaken_session_types::SessionId::from("s1"))
        .await
        .expect("enqueue");
    q.claim("env_a", "worker", 0)
        .await
        .expect("claim query")
        .expect("claim");
    assert_eq!(
        q.ack("env_a", &id, "worker")
            .await
            .expect("ack query")
            .into_item()
            .expect("ack")
            .state,
        WorkState::Active
    );
    assert!(
        q.heartbeat("env_a", &id, "worker", 0, LeaseHeartbeat::unconditional())
            .await
            .expect("heartbeat")
            .into_receipt()
            .expect("hb")
            .lease_extended
    );
    // Wrong env → none across the board.
    assert!(q.get("env_b", &id).await.expect("get").is_none());
    assert!(
        q.ack("env_b", &id, "worker")
            .await
            .expect("ack")
            .is_not_found()
    );
    assert!(
        q.heartbeat("env_b", &id, "worker", 0, LeaseHeartbeat::unconditional())
            .await
            .expect("heartbeat")
            .is_not_found()
    );
    assert!(
        q.stop("env_b", &id, "worker")
            .await
            .expect("stop")
            .is_not_found()
    );
}

#[tokio::test]
async fn stats_and_metadata_and_remove_env() {
    let q = q().await;
    q.enqueue_healthcheck("env_a").await.expect("enqueue");
    let s = q
        .enqueue_session("env_a", &awaken_session_types::SessionId::from("s1"))
        .await
        .expect("enqueue");
    let st = q.stats("env_a", 0).await.expect("stats");
    assert_eq!((st.depth, st.pending, st.workers_polling), (2, 0, 0));
    q.claim("env_a", "w1", 0).await.expect("claim");
    let st = q.stats("env_a", 0).await.expect("stats");
    assert_eq!((st.depth, st.pending, st.workers_polling), (1, 1, 1));
    let patch = BTreeMap::from([("k".to_string(), Some("v".to_string()))]);
    let up = q
        .update_metadata("env_a", &s, patch)
        .await
        .expect("update query")
        .expect("patch");
    assert_eq!(up.metadata.get("k").map(String::as_str), Some("v"));
    let deleted = q
        .update_metadata("env_a", &s, BTreeMap::from([("k".to_string(), None)]))
        .await
        .expect("delete query")
        .expect("delete");
    assert!(!deleted.metadata.contains_key("k"));
    q.remove_env("env_a").await.unwrap();
    assert!(
        q.list("env_a").await.expect("list").is_empty(),
        "env delete purges work"
    );
}

#[tokio::test]
async fn list_is_enqueue_ordered() {
    let q = q().await;
    let a = q
        .enqueue_session("e", &awaken_session_types::SessionId::from("s1"))
        .await
        .expect("enqueue");
    let b = q
        .enqueue_session("e", &awaken_session_types::SessionId::from("s2"))
        .await
        .expect("enqueue");
    let ids: Vec<String> = q
        .list("e")
        .await
        .expect("list")
        .into_iter()
        .map(|w| w.id)
        .collect();
    assert_eq!(ids, vec![a, b]);
}

#[tokio::test]
async fn accepted_instant_survives_reopening_the_durable_file() {
    // Round trip. Causes: C1 a row accepted while the store reads clock A;
    // C2 the same database reopened behind a different clock B. Effects:
    // E1 the read-back acceptance instant is A's reading; E2 it is not B's,
    // so the value is persisted rather than recomputed on read. Rule
    // R1 = C1 + C2 => E1 + E2. Diffusion: a sibling accepted one second
    // later keeps its own distinct instant across the same reopen.
    use awaken_lease_testkit::ManualClock;

    const WRITE_MS: u64 = 1_600_000_000_000;
    const SIBLING_MS: u64 = WRITE_MS + 1_000;
    const REOPEN_MS: u64 = 1_900_000_000_000;

    let file = SqliteFileFixture::new("awaken-work-roundtrip-");

    let writing = Arc::new(ManualClock::new(WRITE_MS));
    let queue = SqliteWorkQueue::open(file.path(), writing.clone())
        .await
        .expect("open durable file");
    let first = queue
        .enqueue_session("rt-env", &awaken_session_types::SessionId::from("rt-a"))
        .await
        .expect("C1");
    writing.set(SIBLING_MS);
    let sibling = queue
        .enqueue_session("rt-env", &awaken_session_types::SessionId::from("rt-b"))
        .await
        .expect("C1 sibling");
    drop(queue);
    file.settled().await;

    let reopened = SqliteWorkQueue::open(file.path(), Arc::new(ManualClock::new(REOPEN_MS)))
        .await
        .expect("C2");
    let expected = awaken_time::utc::epoch_millis_to_rfc3339(WRITE_MS);
    let read = reopened
        .get("rt-env", &first)
        .await
        .expect("read back")
        .expect("row survives");
    assert_eq!(read.created_at, expected, "E1");
    assert_ne!(
        read.created_at,
        awaken_time::utc::epoch_millis_to_rfc3339(REOPEN_MS),
        "E2 the reopening clock must not supply the value"
    );
    let sibling = reopened
        .get("rt-env", &sibling)
        .await
        .expect("read back sibling")
        .expect("sibling survives");
    assert_eq!(
        sibling.created_at,
        awaken_time::utc::epoch_millis_to_rfc3339(SIBLING_MS),
        "E1 diffusion: the sibling keeps its own instant"
    );
    drop(reopened);
    file.close().await;
}

#[tokio::test]
async fn file_open_and_pg_connect_entry_points() {
    // F1 file path -> exact SQLite read under an owned tempdir.
    // F2 no PG URL -> canonical visible skip; configured setup failure
    // -> hard failure. F3 live PG -> connect() reads its own schema and
    // cleanup removes only that schema. No second DB URL is selected.
    let file = SqliteFileFixture::new("awaken-work-entrypoint-");
    let q = SqliteWorkQueue::open(file.path(), system_clock())
        .await
        .expect("open file db");
    let id = q
        .enqueue_session("e", &awaken_session_types::SessionId::from("s"))
        .await
        .expect("enqueue");
    assert!(q.get("e", &id).await.expect("get").is_some(), "F1");
    drop(q);
    file.close().await;
    if let Some(url) = awaken_reliability_testkit::postgres_fixture_url() {
        let fixture = crate::work_postgres_fixture::WorkPostgresFixture::new(&url, 16).await;
        let q = PostgresWorkQueue::connect(&fixture.scoped_url, system_clock())
            .await
            .expect("explicit Work connect entrypoint");
        let id = q.enqueue_healthcheck("cov_env").await.expect("enqueue");
        assert!(q.get("cov_env", &id).await.expect("get").is_some(), "F3");
        let owned_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM work_queue_item WHERE environment_id = $1 AND work_id = $2",
        )
        .bind("cov_env")
        .bind(&id)
        .fetch_one(&fixture.pool)
        .await
        .expect("inspect exact owned Work schema");
        assert_eq!(
            owned_rows, 1,
            "F3 connect(url) wrote inside this owned schema"
        );
        q.remove_env("cov_env")
            .await
            .expect("remove only owned env");
        drop(q);
        fixture.cleanup().await;
    }
}

/// The environment-row lock in pg `claim` serializes the count-and-lease decision:
/// under real concurrent contention, the single-active-lease cap holds —
/// with one queued item and N workers claiming at once, exactly ONE wins the lease
/// and the rest get `None` after they observe the winner's active row.
/// Distinct pool connections per task make the transactions genuinely concurrent.
/// No configured URL skips visibly; an explicit connection/setup failure
/// fails. Each invocation creates and removes only its own schema.
#[tokio::test]
async fn postgres_concurrent_claim_yields_a_single_active_lease() {
    use std::sync::Arc;

    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let fixture = crate::work_postgres_fixture::WorkPostgresFixture::new(&url, 16).await;
    let q = Arc::new(
        PostgresWorkQueue::with_pool(fixture.pool.clone(), system_clock())
            .await
            .expect("store"),
    );

    // One queued item, N workers race to claim it concurrently.
    q.enqueue_session("env_a", &awaken_session_types::SessionId::from("s1"))
        .await
        .expect("enqueue");
    let mut handles = Vec::new();
    for w in 0..8u32 {
        let q = q.clone();
        handles.push(tokio::spawn(async move {
            q.claim("env_a", &format!("worker_{w}"), 0).await
        }));
    }
    let mut winners = 0;
    for h in handles {
        if h.await.unwrap().expect("claim").is_some() {
            winners += 1;
        }
    }
    assert_eq!(
        winners, 1,
        "exactly one worker wins the lease under concurrent contention"
    );
    // And the store agrees: exactly one active row (pending == 1), depth drained.
    let st = q.stats("env_a", 0).await.expect("stats");
    assert_eq!(
        (st.depth, st.pending),
        (0, 1),
        "the single-active cap holds: one active lease, nothing left queued"
    );
    q.remove_env("env_a").await.unwrap();
    drop(q);
    fixture.cleanup().await;
}
