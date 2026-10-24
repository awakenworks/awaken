//! Work-side lease acceptance across every Work backend (T08-C02 Work half,
//! T08-C03 Work half, T09-C02).
//!
//! Each scenario runs against two *independent* handles over one backend:
//! the Volatile reference (one shared logical model, the only meaning of a
//! second handle there), a real file SQLite database opened twice, and a live
//! PostgreSQL schema reached through two separate pools. Races are driven on
//! separate Tokio tasks behind a barrier; a concurrency probe proves the test
//! harness itself never serialises the two clients (negative control).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_executable_environment_contract::{
    HeartbeatResult, LeaseHeartbeat, SessionAccessDigest, WorkMutationResult, WorkSessionAccess,
    WorkState,
};
use awaken_lease_testkit::ManualClock;
use awaken_session_types::SessionId;
use awaken_store_conformance::work_queue::{WorkBackend, session_access_digest};
use awaken_work_store::{InMemoryWorkQueue, LEASE_TTL_MS, PostgresWorkQueue, SqliteWorkQueue};

use super::work_postgres_fixture::WorkPostgresFixture;

type Handle = Arc<dyn WorkBackend>;

/// Two handles over one backend plus whatever must outlive them.
struct Pair {
    label: &'static str,
    a: Handle,
    b: Handle,
    clock: Arc<ManualClock>,
    _dir: Option<tempfile::TempDir>,
    postgres: Option<WorkPostgresFixture>,
}

impl Pair {
    fn volatile() -> Self {
        let clock = Arc::new(ManualClock::new(0));
        let shared: Handle = Arc::new(InMemoryWorkQueue::new(clock.clone()));
        Self {
            label: "volatile",
            a: shared.clone(),
            b: shared,
            clock,
            _dir: None,
            postgres: None,
        }
    }

    async fn sqlite_file() -> Self {
        let clock = Arc::new(ManualClock::new(0));
        let dir = tempfile::tempdir().expect("SQLite tempdir");
        let path = dir.path().join("work.db");
        let path = path.to_str().expect("utf-8 path");
        let a: Handle = Arc::new(
            SqliteWorkQueue::open(path, clock.clone())
                .await
                .expect("first SQLite handle"),
        );
        let b: Handle = Arc::new(
            SqliteWorkQueue::open(path, clock.clone())
                .await
                .expect("second, independent SQLite handle"),
        );
        Self {
            label: "sqlite-file",
            a,
            b,
            clock,
            _dir: Some(dir),
            postgres: None,
        }
    }

    async fn postgres(url: &str) -> Self {
        let clock = Arc::new(ManualClock::new(0));
        let fixture = WorkPostgresFixture::new(url, 8).await;
        let a: Handle = Arc::new(
            PostgresWorkQueue::with_pool(fixture.pool.clone(), clock.clone())
                .await
                .expect("first PostgreSQL pool"),
        );
        let b: Handle = Arc::new(
            PostgresWorkQueue::connect_existing(&fixture.scoped_url, clock.clone())
                .await
                .expect("second, independent PostgreSQL pool"),
        );
        Self {
            label: "postgres",
            a,
            b,
            clock,
            _dir: None,
            postgres: Some(fixture),
        }
    }

    async fn cleanup(self) {
        drop(self.a);
        drop(self.b);
        if let Some(fixture) = self.postgres {
            fixture.cleanup().await;
        }
    }
}

/// Concurrency probe: counts clients simultaneously inside a store call.
#[derive(Default)]
struct Probe {
    inflight: AtomicUsize,
    max: AtomicUsize,
}

/// Run two store calls on separate tasks released together. With
/// `serialize` the harness takes one test-side lock around each call: the
/// mutant a negative control must be able to detect.
async fn race<A, B>(
    serialize: bool,
    left: impl std::future::Future<Output = A> + Send + 'static,
    right: impl std::future::Future<Output = B> + Send + 'static,
) -> (A, B, usize)
where
    A: Send + 'static,
    B: Send + 'static,
{
    let probe = Arc::new(Probe::default());
    let start = Arc::new(tokio::sync::Barrier::new(2));
    let inside = Arc::new(tokio::sync::Barrier::new(2));
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    fn spawn<T: Send + 'static>(
        serialize: bool,
        probe: Arc<Probe>,
        start: Arc<tokio::sync::Barrier>,
        inside: Arc<tokio::sync::Barrier>,
        lock: Arc<tokio::sync::Mutex<()>>,
        call: impl std::future::Future<Output = T> + Send + 'static,
    ) -> tokio::task::JoinHandle<T> {
        tokio::spawn(async move {
            start.wait().await;
            let enter = |probe: &Probe| {
                let now = probe.inflight.fetch_add(1, Ordering::SeqCst) + 1;
                probe.max.fetch_max(now, Ordering::SeqCst);
            };
            if serialize {
                let _guard = lock.lock().await;
                enter(&probe);
                let result = call.await;
                probe.inflight.fetch_sub(1, Ordering::SeqCst);
                result
            } else {
                enter(&probe);
                inside.wait().await;
                let result = call.await;
                probe.inflight.fetch_sub(1, Ordering::SeqCst);
                result
            }
        })
    }
    let l = spawn(
        serialize,
        probe.clone(),
        start.clone(),
        inside.clone(),
        lock.clone(),
        left,
    );
    let r = spawn(serialize, probe.clone(), start, inside, lock, right);
    let (l, r) = (l.await.unwrap(), r.await.unwrap());
    (l, r, probe.max.load(Ordering::SeqCst))
}

async fn access(q: &Handle, digest: &SessionAccessDigest, now: u64) -> Option<WorkSessionAccess> {
    q.authenticate_session_access(digest, now)
        .await
        .expect("authenticate")
}

fn claim_as(
    q: &Handle,
    env: &'static str,
    owner: &'static str,
    now: u64,
    digest: SessionAccessDigest,
) -> impl std::future::Future<Output = bool> + Send + 'static {
    let q = q.clone();
    async move {
        // A storage-level refusal under contention is a failed claim: it must
        // never grant authority, which the caller checks independently.
        matches!(
            q.claim_with_session_access(env, owner, owner, now, None, &digest)
                .await,
            Ok(Some(_))
        )
    }
}

/// T09-C02: two clients race reclaim, then the stale owner keeps committing
/// while the winner renews, then renewal races a second takeover.
///
/// | Row | race                                   | effect                                  |
/// | C1  | reclaim(a) || reclaim(b) at expiry      | exactly one new epoch; loser no access  |
/// | C2  | stale ack/heartbeat/stop || renew       | stale rejected, winner renews           |
/// | C3  | winner renew || third reclaim at expiry | exactly one of them succeeds            |
async fn concurrent_takeover(pair: &Pair) {
    let label = pair.label;
    let env = "race-env";
    let work = pair
        .a
        .enqueue_session(env, &SessionId::from("race-session"))
        .await
        .unwrap();
    assert!(claim_as(&pair.a, env, "owner-old", 0, session_access_digest(1)).await);
    let old = access(&pair.a, &session_access_digest(1), 0)
        .await
        .expect("old access");

    // C1
    let (left, right, concurrent) = race(
        false,
        claim_as(
            &pair.a,
            env,
            "owner-a",
            LEASE_TTL_MS,
            session_access_digest(2),
        ),
        claim_as(
            &pair.b,
            env,
            "owner-b",
            LEASE_TTL_MS,
            session_access_digest(3),
        ),
    )
    .await;
    assert_eq!(
        concurrent, 2,
        "{label} NC: both clients were inside the store at once"
    );
    assert_eq!(
        usize::from(left) + usize::from(right),
        1,
        "{label} C1: exactly one reclaim wins"
    );
    let (winner_digest, loser_digest, winner_owner, loser_owner) = if left {
        (
            session_access_digest(2),
            session_access_digest(3),
            "owner-a",
            "owner-b",
        )
    } else {
        (
            session_access_digest(3),
            session_access_digest(2),
            "owner-b",
            "owner-a",
        )
    };
    let winner = access(&pair.b, &winner_digest, LEASE_TTL_MS)
        .await
        .expect("C1 winner access");
    assert_eq!(winner.lease_owner, winner_owner, "{label} C1");
    assert_eq!(winner.work_id, work, "{label} C1 same Work");
    assert!(winner.lease_epoch > old.lease_epoch, "{label} C1 new epoch");
    assert!(
        access(&pair.a, &loser_digest, LEASE_TTL_MS).await.is_none(),
        "{label} C1 a failed claim does not widen authority"
    );
    assert!(
        access(&pair.a, &session_access_digest(1), LEASE_TTL_MS)
            .await
            .is_none(),
        "{label} C1 stale bearer revoked"
    );

    // C2: the stale owner keeps committing while the winner renews.
    let (stale_ack, renewed, concurrent) = race(
        false,
        {
            let (q, old) = (pair.a.clone(), old.clone());
            async move { q.ack_with_session_access(&old).await.unwrap() }
        },
        {
            let (q, winner) = (pair.b.clone(), winner.clone());
            async move {
                q.heartbeat_with_session_access(
                    &winner,
                    LEASE_TTL_MS + 1,
                    LeaseHeartbeat::unconditional(),
                )
                .await
                .unwrap()
            }
        },
    )
    .await;
    assert_eq!(concurrent, 2, "{label} C2 NC");
    assert!(
        matches!(stale_ack, WorkMutationResult::PreconditionFailed),
        "{label} C2 stale ack rejected"
    );
    assert!(
        matches!(renewed, HeartbeatResult::Accepted(_)),
        "{label} C2 current owner renews"
    );
    assert!(matches!(
        pair.a
            .heartbeat_with_session_access(&old, LEASE_TTL_MS + 1, LeaseHeartbeat::unconditional())
            .await
            .unwrap(),
        HeartbeatResult::PreconditionFailed
    ));
    assert!(matches!(
        pair.a.stop_with_session_access(&old).await.unwrap(),
        WorkMutationResult::PreconditionFailed
    ));
    for owner in ["owner-old", loser_owner] {
        assert!(
            matches!(
                pair.a.ack(env, &work, owner).await.unwrap(),
                WorkMutationResult::PreconditionFailed
            ),
            "{label} C2 {owner} owner-only commit rejected"
        );
    }
    assert_eq!(
        pair.a.get(env, &work).await.unwrap().unwrap().state,
        WorkState::Active,
        "{label} C2 rejected commits left the Work active"
    );
    let current = access(&pair.a, &winner_digest, LEASE_TTL_MS + 1)
        .await
        .expect("C2 winner still current");
    assert_eq!(
        current.lease_epoch, winner.lease_epoch,
        "{label} C2 epoch kept"
    );

    // C3: renewal races a third client's takeover at the renewed expiry.
    let expiry = current.expires_at_unix_ms;
    let (renew, takeover, concurrent) = race(
        false,
        {
            let (q, current) = (pair.a.clone(), current.clone());
            async move {
                matches!(
                    q.heartbeat_with_session_access(
                        &current,
                        expiry,
                        LeaseHeartbeat::unconditional()
                    )
                    .await,
                    Ok(HeartbeatResult::Accepted(_))
                )
            }
        },
        claim_as(&pair.b, env, "owner-c", expiry, session_access_digest(4)),
    )
    .await;
    assert_eq!(concurrent, 2, "{label} C3 NC");
    assert_eq!(
        usize::from(renew) + usize::from(takeover),
        1,
        "{label} C3 renew and takeover never both win (renew={renew}, takeover={takeover})"
    );
    let live = [winner_digest, session_access_digest(4)];
    let mut live_count = 0;
    for digest in &live {
        live_count += usize::from(access(&pair.b, digest, expiry).await.is_some());
    }
    assert_eq!(live_count, 1, "{label} C3 one live authority");
}

/// The concurrency probe detects a harness that serialises the clients.
async fn serialized_harness_is_detected() {
    let (_, _, concurrent) = race(true, async { tokio::task::yield_now().await }, async {
        tokio::task::yield_now().await
    })
    .await;
    assert_eq!(
        concurrent, 1,
        "NC: the probe reports a test-side lock as non-concurrent"
    );
}

/// T08-C03 Work half: Session Work leases and the healthcheck item are taken
/// over independently; old claimers are refused; the healthcheck completes on
/// its own without touching Session Work authority.
async fn session_and_healthcheck_takeover(pair: &Pair) {
    let label = pair.label;
    let session = SessionId::from("lease-session");
    pair.a
        .enqueue_session("session-env", &session)
        .await
        .unwrap();
    let first = pair
        .a
        .acquire_session("session-env", &session, "owner-a", 0)
        .await
        .unwrap()
        .expect("first Session lease");
    let second = pair
        .b
        .acquire_session("session-env", &session, "owner-b", LEASE_TTL_MS)
        .await
        .unwrap()
        .expect("expired Session lease is taken over");
    assert_eq!(second.owner(), "owner-b", "{label}");
    assert!(second.epoch() > first.epoch(), "{label} new Work epoch");
    assert!(
        !pair.a.release_session(&first).await.unwrap(),
        "{label} old Session owner refused"
    );

    let hc = pair.a.enqueue_healthcheck("hc-env").await.unwrap();
    assert_eq!(
        pair.a
            .claim("hc-env", "hc-old", 0)
            .await
            .unwrap()
            .map(|w| w.id),
        Some(hc.clone())
    );
    assert_eq!(
        pair.b
            .claim("hc-env", "hc-new", LEASE_TTL_MS)
            .await
            .unwrap()
            .map(|w| w.id),
        Some(hc.clone()),
        "{label} expired healthcheck is reclaimed"
    );
    for mutation in ["ack", "stop"] {
        let result = if mutation == "ack" {
            pair.a.ack("hc-env", &hc, "hc-old").await.unwrap()
        } else {
            pair.a.stop("hc-env", &hc, "hc-old").await.unwrap()
        };
        assert!(
            matches!(result, WorkMutationResult::PreconditionFailed),
            "{label} old healthcheck claimer {mutation} refused"
        );
    }
    assert!(
        pair.b
            .ack("hc-env", &hc, "hc-new")
            .await
            .unwrap()
            .is_accepted()
    );
    assert!(
        pair.b
            .stop("hc-env", &hc, "hc-new")
            .await
            .unwrap()
            .is_accepted()
    );
    assert_eq!(
        pair.a.get("hc-env", &hc).await.unwrap().unwrap().state,
        WorkState::Stopped,
        "{label} healthcheck completed independently"
    );
    let observed = pair
        .a
        .acquire_session("session-env", &session, "observer", LEASE_TTL_MS + 1)
        .await
        .unwrap()
        .expect("Session lease remains");
    assert_eq!(
        observed, second,
        "{label} healthcheck left Session authority intact"
    );
}

/// T08-C02 Work half: after the Run's terminal release, only the original
/// release/ack identity is retried; nothing becomes claimable again.
async fn terminal_ack_retry(pair: &Pair) {
    let label = pair.label;
    let env = "ack-env";
    let work = pair
        .a
        .enqueue_session(env, &SessionId::from("ack-session"))
        .await
        .unwrap();
    assert!(claim_as(&pair.a, env, "owner", 0, session_access_digest(7)).await);
    let lease_access = access(&pair.a, &session_access_digest(7), 0).await.unwrap();
    pair.clock.set(100);
    let first = pair
        .a
        .ack_with_session_access(&lease_access)
        .await
        .unwrap()
        .into_item()
        .expect("ack");
    // Lost response: the same identity retries from the other handle.
    let retry = pair
        .b
        .ack_with_session_access(&lease_access)
        .await
        .unwrap()
        .into_item()
        .expect("retried original ack");
    assert_eq!(retry.state, first.state, "{label} retry changes no state");
    assert_eq!(retry.acknowledged_at, first.acknowledged_at, "{label}");
    let lease = pair
        .a
        .acquire_session(env, &SessionId::from("ack-session"), "owner", 1)
        .await
        .unwrap()
        .expect("current lease");
    assert!(
        pair.a.release_session(&lease).await.unwrap(),
        "{label} release"
    );
    assert!(
        pair.b.release_session(&lease).await.unwrap(),
        "{label} lost release response retries idempotently"
    );
    assert_eq!(
        pair.a.get(env, &work).await.unwrap().unwrap().state,
        WorkState::Stopped
    );
    assert!(
        pair.b.claim(env, "other", 2).await.unwrap().is_none()
            && pair
                .b
                .acquire_session(env, &SessionId::from("ack-session"), "owner", 2)
                .await
                .unwrap()
                .is_none(),
        "{label} terminal Work is never re-executed"
    );
    assert!(
        matches!(
            pair.b.ack_with_session_access(&lease_access).await.unwrap(),
            WorkMutationResult::PreconditionFailed
        ),
        "{label} a retried ack after release grants nothing"
    );
}

async fn all_backends(scenario: &impl AsyncFn(&Pair)) {
    let volatile = Pair::volatile();
    scenario(&volatile).await;
    volatile.cleanup().await;
    let sqlite = Pair::sqlite_file().await;
    scenario(&sqlite).await;
    sqlite.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_takeover_admits_only_the_current_owner_on_volatile_and_file_sqlite() {
    serialized_harness_is_detected().await;
    all_backends(&concurrent_takeover).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_and_healthcheck_leases_are_taken_over_independently() {
    all_backends(&session_and_healthcheck_takeover).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_ack_retries_only_the_original_identity() {
    all_backends(&terminal_ack_retry).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_work_lease_acceptance() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    for scenario in [0, 1, 2] {
        let pair = Pair::postgres(&url).await;
        match scenario {
            0 => concurrent_takeover(&pair).await,
            1 => session_and_healthcheck_takeover(&pair).await,
            _ => terminal_ack_retry(&pair).await,
        }
        pair.cleanup().await;
    }
}

#[test]
fn postgres_work_lease_acceptance_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "lease_acceptance::postgres_work_lease_acceptance",
    );
}

// ── Product-gap reproducers (asserting the intended behaviour) ───────────────
//
// Each reproducer runs the same row on every backend (Volatile, file SQLite
// over two independent handles, and PostgreSQL over two pools when the
// fixture is configured) and fails naming every backend that diverges, so a
// per-backend gap is visible instead of stopping at the first backend.

async fn gap_backends(row: &str, scenario: impl AsyncFn(&Pair) -> bool) {
    let mut divergent = Vec::new();
    let volatile = Pair::volatile();
    if !scenario(&volatile).await {
        divergent.push(volatile.label);
    }
    volatile.cleanup().await;
    let sqlite = Pair::sqlite_file().await;
    if !scenario(&sqlite).await {
        divergent.push(sqlite.label);
    }
    sqlite.cleanup().await;
    if let Some(url) = awaken_reliability_testkit::postgres_fixture_url() {
        let pair = Pair::postgres(&url).await;
        if !scenario(&pair).await {
            divergent.push(pair.label);
        }
        pair.cleanup().await;
    }
    assert!(
        divergent.is_empty(),
        "{row}: diverging backends {divergent:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "product gap: plain ack/heartbeat/stop are owner-only, so a stale claimer reusing the same worker id is not fenced after reclaim"]
async fn stale_same_owner_claimer_is_refused_after_reclaim() {
    gap_backends("stale incarnation ack accepted", async |pair: &Pair| {
        let hc = pair.a.enqueue_healthcheck("same-owner").await.unwrap();
        pair.a
            .claim("same-owner", "worker", 0)
            .await
            .unwrap()
            .unwrap();
        // The worker restarts under the same id and reclaims the expired lease.
        pair.b
            .claim("same-owner", "worker", LEASE_TTL_MS)
            .await
            .unwrap()
            .unwrap();
        // The previous incarnation's delayed ack must not act on the new lease.
        matches!(
            pair.a.ack("same-owner", &hc, "worker").await.unwrap(),
            WorkMutationResult::PreconditionFailed
        )
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "product gap: Work ack does not check lease expiry, so an ack after the lease lapsed is accepted"]
async fn ack_after_the_lease_lapsed_is_refused() {
    gap_backends("lapsed lease ack accepted", async |pair: &Pair| {
        let hc = pair.a.enqueue_healthcheck("lapsed").await.unwrap();
        pair.a.claim("lapsed", "worker", 0).await.unwrap().unwrap();
        pair.clock.set(LEASE_TTL_MS + 1);
        matches!(
            pair.a.ack("lapsed", &hc, "worker").await.unwrap(),
            WorkMutationResult::PreconditionFailed
        )
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "product gap: a retried Work ack overwrites acknowledged_at instead of replaying the original ack"]
async fn retried_ack_replays_the_original_acknowledgement() {
    gap_backends(
        "the retry re-stamped the acknowledgement",
        async |pair: &Pair| {
            let env = "ack-replay";
            pair.a
                .enqueue_session(env, &SessionId::from("replay"))
                .await
                .unwrap();
            assert!(claim_as(&pair.a, env, "owner", 0, session_access_digest(9)).await);
            let lease = access(&pair.a, &session_access_digest(9), 0).await.unwrap();
            pair.clock.set(100);
            let first = pair.a.ack_with_session_access(&lease).await.unwrap();
            pair.clock.set(200);
            let retry = pair.b.ack_with_session_access(&lease).await.unwrap();
            retry.into_item().unwrap().acknowledged_at == first.into_item().unwrap().acknowledged_at
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "product gap: a retried stop after a lost response returns PreconditionFailed (412) instead of replaying the stop"]
async fn retried_stop_after_lost_response_replays() {
    gap_backends("retried stop was refused", async |pair: &Pair| {
        let hc = pair.a.enqueue_healthcheck("stop-replay").await.unwrap();
        pair.a
            .claim("stop-replay", "worker", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            pair.a
                .stop("stop-replay", &hc, "worker")
                .await
                .unwrap()
                .is_accepted()
        );
        pair.b
            .stop("stop-replay", &hc, "worker")
            .await
            .unwrap()
            .is_accepted()
    })
    .await;
}
