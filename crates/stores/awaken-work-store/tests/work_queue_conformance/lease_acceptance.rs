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
use awaken_time::{UtcTime, WallClock};
use awaken_work_store::{InMemoryWorkQueue, LEASE_TTL_MS, PostgresWorkQueue, SqliteWorkQueue};

use super::work_postgres_fixture::WorkPostgresFixture;

type Handle = Arc<dyn WorkBackend>;

fn same_item(
    left: &awaken_executable_environment_contract::WorkItem,
    right: &awaken_executable_environment_contract::WorkItem,
) -> bool {
    left.id == right.id
        && left.environment_id == right.environment_id
        && left.data == right.data
        && left.metadata == right.metadata
        && left.state == right.state
        && left.created_at == right.created_at
        && left.acknowledged_at == right.acknowledged_at
        && left.latest_heartbeat_at == right.latest_heartbeat_at
        && left.started_at == right.started_at
        && left.stop_requested_at == right.stop_requested_at
        && left.stopped_at == right.stopped_at
}

struct ObservedClock {
    inner: ManualClock,
    reads: AtomicUsize,
}

impl ObservedClock {
    fn new() -> Self {
        Self {
            inner: ManualClock::new(0),
            reads: AtomicUsize::new(0),
        }
    }

    fn set(&self, now: u64) {
        self.inner.set(now);
    }

    fn observations(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl WallClock for ObservedClock {
    fn now(&self) -> UtcTime {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.now()
    }
}

/// Two handles over one backend plus whatever must outlive them.
struct Pair {
    label: &'static str,
    a: Handle,
    b: Handle,
    clock: Arc<ObservedClock>,
    _dir: Option<tempfile::TempDir>,
    postgres: Option<WorkPostgresFixture>,
}

impl Pair {
    fn volatile() -> Self {
        let clock = Arc::new(ObservedClock::new());
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
        let clock = Arc::new(ObservedClock::new());
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
        Self::postgres_with_limit(url, 8).await
    }

    async fn postgres_with_limit(url: &str, limit: u32) -> Self {
        let clock = Arc::new(ObservedClock::new());
        let fixture = WorkPostgresFixture::new(url, limit).await;
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
    env: &str,
    owner: &'static str,
    now: u64,
    digest: SessionAccessDigest,
) -> impl std::future::Future<Output = bool> + Send + 'static {
    let q = q.clone();
    let env = env.to_owned();
    async move {
        // A storage-level refusal under contention is a failed claim: it must
        // never grant authority, which the caller checks independently.
        matches!(
            q.claim_with_session_access(&env, owner, owner, now, None, &digest)
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

// The historical same-owner/process reproducer expected two identical
// owner-only requests to be distinguishable. E17 retains that original
// failed source. The credential_generation module tests the actual credential
// boundary and the exact Session generation with current-holder controls.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ack_after_the_lease_lapsed_is_refused() {
    gap_backends("lapsed lease ack accepted", async |pair: &Pair| {
        let mut correct = true;
        for (index, (bound, offset)) in [false, true]
            .into_iter()
            .flat_map(|bound| {
                [LEASE_TTL_MS - 1, LEASE_TTL_MS, LEASE_TTL_MS + 1].map(|offset| (bound, offset))
            })
            .enumerate()
        {
            let base = index as u64 * LEASE_TTL_MS * 4;
            let env = format!("ack-expiry-{index}");
            let (work, lease) = claimed_work(pair, &env, bound, base).await;
            let before = pair.a.get(&env, &work).await.unwrap().unwrap();
            pair.clock.set(base + offset);
            let reads = pair.clock.observations();
            let result = acknowledge(&pair.a, &env, &work, lease.as_ref()).await;
            correct &= pair.clock.observations() == reads + 1;
            if offset < LEASE_TTL_MS {
                correct &= result.into_item().is_some_and(|item| {
                    item.acknowledged_at
                        == Some(awaken_time::utc::epoch_millis_to_rfc3339(base + offset))
                        && item.state == WorkState::Active
                });
            } else {
                correct &= matches!(result, WorkMutationResult::PreconditionFailed)
                    && pair
                        .a
                        .get(&env, &work)
                        .await
                        .unwrap()
                        .is_some_and(|after| same_item(&after, &before));
            }
        }
        correct
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retried_ack_replays_the_original_acknowledgement() {
    gap_backends(
        "the retry re-stamped the acknowledgement",
        async |pair: &Pair| {
            let mut correct = true;
            for (index, bound) in [false, true].into_iter().enumerate() {
                let base = index as u64 * LEASE_TTL_MS * 4;
                let env = format!("ack-replay-{index}");
                let (work, lease) = claimed_work(pair, &env, bound, base).await;
                pair.clock.set(base + 100);
                let first = acknowledge(&pair.a, &env, &work, lease.as_ref())
                    .await
                    .into_item()
                    .unwrap();
                // Replay is historical evidence, not another grant: it reads no
                // clock, even after expiry, and never bypasses subject fencing.
                for offset in [200, LEASE_TTL_MS + 1] {
                    pair.clock.set(base + offset);
                    let reads = pair.clock.observations();
                    let retry = acknowledge(&pair.b, &env, &work, lease.as_ref())
                        .await
                        .into_item()
                        .unwrap();
                    correct &= same_item(&retry, &first) && pair.clock.observations() == reads;
                }
                let reads = pair.clock.observations();
                correct &= matches!(
                    pair.b.ack(&env, &work, "foreign").await.unwrap(),
                    WorkMutationResult::PreconditionFailed
                );
                if let Some(mut wrong_epoch) = lease {
                    wrong_epoch.lease_epoch = wrong_epoch.lease_epoch.next().unwrap();
                    correct &= matches!(
                        pair.b.ack_with_session_access(&wrong_epoch).await.unwrap(),
                        WorkMutationResult::PreconditionFailed
                    );
                }
                correct &= pair.clock.observations() == reads;
            }
            correct
        },
    )
    .await;
}

async fn claimed_work(
    pair: &Pair,
    env: &str,
    bound: bool,
    base: u64,
) -> (String, Option<WorkSessionAccess>) {
    pair.clock.set(base);
    if bound {
        let work = pair
            .a
            .enqueue_session(env, &SessionId::from(env))
            .await
            .unwrap();
        let digest = session_access_digest(base + 901);
        assert!(claim_as(&pair.a, env, "owner", base, digest.clone()).await);
        (work, Some(access(&pair.a, &digest, base).await.unwrap()))
    } else {
        let work = pair.a.enqueue_healthcheck(env).await.unwrap();
        pair.a.claim(env, "owner", base).await.unwrap().unwrap();
        (work, None)
    }
}

async fn acknowledge(
    q: &Handle,
    env: &str,
    work: &str,
    access: Option<&WorkSessionAccess>,
) -> WorkMutationResult {
    if let Some(access) = access {
        q.ack_with_session_access(access).await.unwrap()
    } else {
        q.ack(env, work, "owner").await.unwrap()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acknowledgement_receipts_do_not_cross_lease_generations() {
    gap_backends(
        "prior ACK replay bypassed the replacement lease",
        async |pair: &Pair| {
            let mut correct = true;
            for (index, (bound, expired)) in [false, true]
                .into_iter()
                .flat_map(|bound| [false, true].map(|expired| (bound, expired)))
                .enumerate()
            {
                let base = index as u64 * LEASE_TTL_MS * 5;
                let env = format!("ack-generation-{index}");
                let (work, old_access) = claimed_work(pair, &env, bound, base).await;
                pair.clock.set(base + 100);
                let original = acknowledge(&pair.a, &env, &work, old_access.as_ref())
                    .await
                    .into_item()
                    .unwrap();
                let next_start = base + LEASE_TTL_MS;
                pair.clock.set(next_start);
                let next = if bound {
                    let digest = session_access_digest(base + 902);
                    assert!(claim_as(&pair.b, &env, "owner", next_start, digest.clone()).await);
                    Some(access(&pair.b, &digest, next_start).await.unwrap())
                } else {
                    pair.b
                        .claim(&env, "owner", next_start)
                        .await
                        .unwrap()
                        .unwrap();
                    None
                };
                let before = pair.a.get(&env, &work).await.unwrap().unwrap();
                if let Some(old) = old_access {
                    correct &= matches!(
                        pair.a.ack_with_session_access(&old).await.unwrap(),
                        WorkMutationResult::PreconditionFailed
                    );
                }
                let observed = next_start + if expired { LEASE_TTL_MS } else { 10 };
                pair.clock.set(observed);
                let reads = pair.clock.observations();
                let result = acknowledge(&pair.b, &env, &work, next.as_ref()).await;
                correct &= pair.clock.observations() == reads + 1;
                if expired {
                    correct &= matches!(result, WorkMutationResult::PreconditionFailed)
                        && pair
                            .a
                            .get(&env, &work)
                            .await
                            .unwrap()
                            .is_some_and(|after| same_item(&after, &before));
                } else {
                    let current = result.into_item().unwrap();
                    correct &= current.acknowledged_at
                        == Some(awaken_time::utc::epoch_millis_to_rfc3339(observed))
                        && current.acknowledged_at != original.acknowledged_at;
                    pair.clock.set(next_start + LEASE_TTL_MS);
                    let reads = pair.clock.observations();
                    let replay = acknowledge(&pair.a, &env, &work, next.as_ref())
                        .await
                        .into_item()
                        .unwrap();
                    correct &= same_item(&replay, &current) && pair.clock.observations() == reads;
                }
            }
            correct
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_acknowledgements_share_one_original_receipt() {
    gap_backends(
        "concurrent ACK changed the original receipt",
        async |pair: &Pair| {
            let env = "concurrent-ack";
            let (work, lease) = claimed_work(pair, env, true, 0).await;
            let lease = lease.unwrap();
            pair.clock.set(100);
            let reads = pair.clock.observations();
            let (left, right, concurrent) = race(
                false,
                {
                    let q = pair.a.clone();
                    let lease = lease.clone();
                    async move { q.ack_with_session_access(&lease).await.unwrap() }
                },
                {
                    let q = pair.b.clone();
                    async move { q.ack_with_session_access(&lease).await.unwrap() }
                },
            )
            .await;
            let left = left.into_item().unwrap();
            let right = right.into_item().unwrap();
            concurrent == 2
                && same_item(&left, &right)
                && pair.clock.observations() == reads + 1
                && pair
                    .a
                    .get(env, &work)
                    .await
                    .unwrap()
                    .is_some_and(|stored| same_item(&stored, &left))
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_ack_checks_expiry_after_waiting_for_its_write_transaction() {
    use sqlx::Connection as _;
    let pair = Pair::sqlite_file().await;
    let (work, lease) = claimed_work(&pair, "ack-write-wait", true, 0).await;
    let before = pair.a.get("ack-write-wait", &work).await.unwrap();
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(pair._dir.as_ref().unwrap().path().join("work.db"));
    let mut blocker = sqlx::SqliteConnection::connect_with(&options)
        .await
        .unwrap();
    let lock = blocker.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let reads = pair.clock.observations();
    let mut request = Box::pin(acknowledge(
        &pair.a,
        "ack-write-wait",
        &work,
        lease.as_ref(),
    ));
    let pending = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(request.as_mut(), cx).is_pending())
    })
    .await;
    let early_reads = pair.clock.observations() - reads;
    pair.clock.set(LEASE_TTL_MS);
    lock.commit().await.unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), request)
        .await
        .unwrap();
    let after = pair.a.get("ack-write-wait", &work).await.unwrap();
    blocker.close().await.unwrap();
    pair.cleanup().await;
    assert!(pending, "the owned writer lock blocks ACK admission");
    assert_eq!(
        early_reads, 0,
        "no authoritative clock sample before write admission"
    );
    assert!(matches!(result, WorkMutationResult::PreconditionFailed));
    assert!(
        same_item(after.as_ref().unwrap(), before.as_ref().unwrap()),
        "expired ACK produces no mutation"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_ack_reply_belongs_to_its_committed_lease_snapshot() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    // The real row lock admits ACK before a queued read takes its only pool
    // connection. An independent pool then reclaims the item. The response
    // must describe the acknowledged lease, including its original start.
    // A deliberately late reload is executed as the counterfactual: without
    // this control a harness that missed the interleaving could pass vacuously.
    assert!(postgres_mutation_snapshot_matches(&url, SnapshotMutation::Acknowledge, false).await);
    assert!(
        !postgres_mutation_snapshot_matches(&url, SnapshotMutation::Acknowledge, true).await,
        "late-reload mutant must be detected"
    );
}

#[derive(Clone, Copy)]
enum SnapshotMutation {
    Acknowledge,
    Stop,
}

async fn postgres_mutation_snapshot_matches(
    url: &str,
    mutation: SnapshotMutation,
    late_reload: bool,
) -> bool {
    let pair = Pair::postgres_with_limit(url, 1).await;
    let env = "mutation-response-snapshot";
    let (work, lease) = claimed_work(&pair, env, true, 0).await;
    let lease = lease.unwrap();
    let mut expected = pair.a.get(env, &work).await.unwrap().unwrap();
    assert_eq!(expected.state, WorkState::Active, "valid claimed fixture");
    let stamp = awaken_time::utc::epoch_millis_to_rfc3339(100);
    match mutation {
        SnapshotMutation::Acknowledge => expected.acknowledged_at = Some(stamp),
        SnapshotMutation::Stop => {
            expected.state = WorkState::Stopped;
            expected.stop_requested_at = Some(stamp.clone());
            expected.stopped_at = Some(stamp);
        }
    }
    pair.clock.set(100);
    let fixture = pair.postgres.as_ref().unwrap();
    let original_pool = fixture.pool.clone();
    let inspector = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&fixture.scoped_url)
        .await
        .unwrap();
    let backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&original_pool)
        .await
        .unwrap();
    let mut held_row = inspector.begin().await.unwrap();
    sqlx::query("SELECT work_id FROM work_queue_item WHERE work_id=$1 FOR UPDATE")
        .bind(&work)
        .fetch_one(&mut *held_row)
        .await
        .unwrap();
    let request = tokio::spawn({
        let queue = pair.a.clone();
        let work = work.clone();
        async move {
            let reply = match mutation {
                SnapshotMutation::Acknowledge => queue.ack_with_session_access(&lease).await,
                SnapshotMutation::Stop => queue.stop_with_session_access(&lease).await,
            }
            .unwrap()
            .into_item()
            .unwrap();
            if late_reload {
                queue.get(env, &work).await.unwrap().unwrap()
            } else {
                reply
            }
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event_type='Lock')")
                .bind(backend_pid).fetch_one(&inspector).await.unwrap();
            if waiting { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("ACK reaches the held row lock");
    let mut queued_read = Box::pin(original_pool.acquire());
    let queued = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(queued_read.as_mut(), cx).is_pending())
    })
    .await;
    assert!(
        queued,
        "the only connection belongs to ACK before its commit"
    );
    held_row.commit().await.unwrap();
    let read_blocker = tokio::time::timeout(std::time::Duration::from_secs(20), queued_read)
        .await
        .unwrap()
        .unwrap();
    pair.clock.set(LEASE_TTL_MS + 1);
    if matches!(mutation, SnapshotMutation::Stop) {
        assert_eq!(
            pair.b
                .wake_session(env, &SessionId::from(env))
                .await
                .unwrap(),
            work
        );
    }
    let replacement = pair
        .b
        .claim(env, "replacement", LEASE_TTL_MS + 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replacement.id, work, "actual reclaim of the original item");
    assert_ne!(
        replacement.started_at, expected.started_at,
        "the two generation snapshots differ"
    );
    drop(read_blocker);
    let reply = tokio::time::timeout(std::time::Duration::from_secs(20), request)
        .await
        .unwrap()
        .unwrap();
    inspector.close().await;
    pair.cleanup().await;
    same_item(&reply, &expected)
}

#[path = "lease_acceptance/stop_replay.rs"]
mod stop_replay;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retried_stop_after_lost_response_replays() {
    stop_replay::retry_matrix().await;
}

#[path = "lease_acceptance/credential_generation.rs"]
mod credential_generation;
