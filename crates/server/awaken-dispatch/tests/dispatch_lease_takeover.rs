//! Dispatch-side lease acceptance across every Dispatch backend (T08-C03
//! dispatch half, T09-C02 dispatch half).
//!
//! Two independent handles per backend: the Volatile store shared by two
//! clients (its only meaning of "two handles"), one file SQLite database
//! opened twice, and one PostgreSQL schema reached through two separate pools
//! (PostgreSQL judges leases on its own clock, so expiry is waited for, not
//! set). Races run on separate tasks behind a barrier; a concurrency probe
//! proves the harness does not serialise the clients (negative control).

mod harness;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_dispatch::{
    AttemptAdmission, DispatchOutcome, MemoryDispatchStore, PostgresDispatchStore, RunClaim,
    RunDispatch, SqliteDispatchStore,
};
use awaken_dispatch_contract::{
    DispatchClaimOwnership, DispatchClaims, DispatchEnqueue, DispatchExecution, DispatchReads,
    DispatchSettlement, LeaseRenewalOutcome, SettleOutcome,
};
use awaken_lease_testkit::ManualClock;
use harness::activation;

/// The Dispatch roles these scenarios exercise, over one concrete backend.
trait Backend:
    DispatchClaims
    + DispatchExecution
    + DispatchSettlement
    + DispatchEnqueue
    + DispatchReads
    + DispatchClaimOwnership
    + Send
    + Sync
    + 'static
{
}
impl<
    T: DispatchClaims
        + DispatchExecution
        + DispatchSettlement
        + DispatchEnqueue
        + DispatchReads
        + DispatchClaimOwnership
        + Send
        + Sync
        + 'static,
> Backend for T
{
}

const LONG_LEASE_MS: u64 = 30_000;

enum Time {
    Manual(Arc<ManualClock>),
    Postgres(sqlx::PgPool),
}

impl Time {
    async fn now(&self) -> u64 {
        match self {
            Self::Manual(clock) => awaken_time::WallClockExt::now_ms(clock.as_ref()),
            Self::Postgres(pool) => harness::postgres_now_ms(pool).await,
        }
    }

    /// Move the backend's authoritative clock strictly past `deadline_ms`.
    async fn pass(&self, deadline_ms: u64) {
        match self {
            Self::Manual(clock) => clock.set(deadline_ms + 1),
            Self::Postgres(pool) => {
                while harness::postgres_now_ms(pool).await <= deadline_ms {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
    }
}

struct Pair<S> {
    label: &'static str,
    a: Arc<S>,
    b: Arc<S>,
    time: Time,
    first_lease_ms: u64,
    _dir: Option<tempfile::TempDir>,
}

impl Pair<MemoryDispatchStore> {
    fn volatile() -> Self {
        let clock = Arc::new(ManualClock::new(0));
        let shared = Arc::new(MemoryDispatchStore::new(clock.clone()));
        Self {
            label: "volatile",
            a: shared.clone(),
            b: shared,
            time: Time::Manual(clock),
            first_lease_ms: LONG_LEASE_MS,
            _dir: None,
        }
    }
}

impl Pair<SqliteDispatchStore> {
    async fn sqlite_file() -> Self {
        let clock = Arc::new(ManualClock::new(0));
        let dir = tempfile::tempdir().expect("SQLite tempdir");
        let path = dir.path().join("dispatch.db");
        let path = path.to_str().expect("utf-8 path");
        let a = Arc::new(
            SqliteDispatchStore::open(path, clock.clone())
                .await
                .expect("first SQLite handle"),
        );
        let b = Arc::new(
            SqliteDispatchStore::open(path, clock.clone())
                .await
                .expect("second, independent SQLite handle"),
        );
        Self {
            label: "sqlite-file",
            a,
            b,
            time: Time::Manual(clock),
            first_lease_ms: LONG_LEASE_MS,
            _dir: Some(dir),
        }
    }
}

impl Pair<PostgresDispatchStore> {
    async fn postgres(schema: &'static str) -> Option<Self> {
        let pool = harness::schema_pool(schema).await?;
        let a = Arc::new(
            PostgresDispatchStore::with_pool(pool.clone())
                .await
                .expect("first PostgreSQL pool"),
        );
        let b = Arc::new(
            PostgresDispatchStore::connect_existing(&harness::database_url_in_schema(schema), 4)
                .await
                .expect("second, independent PostgreSQL pool"),
        );
        Some(Self {
            label: "postgres",
            a,
            b,
            time: Time::Postgres(pool),
            // Short enough that the real database clock crosses it quickly.
            first_lease_ms: 300,
            _dir: None,
        })
    }
}

#[derive(Default)]
struct Probe {
    inflight: AtomicUsize,
    max: AtomicUsize,
}

/// Two store calls on separate tasks released together; `serialize` is the
/// harness mutant that takes one test-side lock around each call.
async fn race<A: Send + 'static, B: Send + 'static>(
    serialize: bool,
    left: impl std::future::Future<Output = A> + Send + 'static,
    right: impl std::future::Future<Output = B> + Send + 'static,
) -> (A, B, usize) {
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
            let guard = if serialize {
                Some(lock.lock().await)
            } else {
                None
            };
            enter(&probe);
            if !serialize {
                inside.wait().await;
            }
            let result = call.await;
            probe.inflight.fetch_sub(1, Ordering::SeqCst);
            drop(guard);
            result
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

fn reclaim<S: Backend>(
    store: &Arc<S>,
    run: &RunId,
    owner: &'static str,
) -> impl std::future::Future<Output = Option<awaken_dispatch::Claimed>> + Send + 'static {
    let (store, run) = (store.clone(), run.clone());
    async move {
        // A storage-level refusal under contention is a failed claim; it must
        // never grant authority, which is checked independently below.
        store
            .claim_run(&run, owner, LONG_LEASE_MS, &Default::default())
            .await
            .ok()
            .flatten()
    }
}

/// | Row | race                                        | effect                                 |
/// | D1  | reclaim(a) || reclaim(b) after expiry       | one winner, epoch + 1, loser no claim  |
/// | D2  | stale settle || winner renew                | stale Fenced, winner Current           |
/// | D3  | winner executes and settles                 | Applied once; the row is gone          |
async fn dispatch_takeover<S: Backend>(pair: &Pair<S>) {
    let label = pair.label;
    let run = RunId(format!("takeover-{label}"));
    pair.a
        .enqueue(RunDispatch::new(activation(&run.0)))
        .await
        .unwrap();
    let first = pair
        .a
        .claim_run(&run, "worker-old", pair.first_lease_ms, &Default::default())
        .await
        .unwrap()
        .expect("first claim");
    let old = RunClaim::from(&first.lease);
    pair.time.pass(first.lease.expires_ms()).await;

    // D1
    let (left, right, concurrent) = race(
        false,
        reclaim(&pair.a, &run, "worker-a"),
        reclaim(&pair.b, &run, "worker-b"),
    )
    .await;
    assert_eq!(
        concurrent, 2,
        "{label} NC: both clients were inside the store at once"
    );
    assert_eq!(
        usize::from(left.is_some()) + usize::from(right.is_some()),
        1,
        "{label} D1 exactly one reclaim wins"
    );
    let (winner, loser_owner) = match (left, right) {
        (Some(winner), None) => (winner, "worker-b"),
        (None, Some(winner)) => (winner, "worker-a"),
        _ => unreachable!(),
    };
    assert!(winner.recovered, "{label} D1 recovered claim");
    assert_eq!(
        awaken_lease_contract::fence::Epoch::from_persisted(winner.lease.epoch()),
        awaken_lease_contract::fence::Epoch::from_persisted(old.epoch)
            .and_then(awaken_lease_contract::fence::Epoch::next),
        "{label} D1 new epoch"
    );
    let current = RunClaim::from(&winner.lease);
    let loser = RunClaim {
        owner: loser_owner.into(),
        ..current.clone()
    };
    assert!(
        pair.b.claim_is_current(&current).await.unwrap(),
        "{label} D1"
    );
    assert!(
        !pair.a.claim_is_current(&loser).await.unwrap(),
        "{label} D1 a failed reclaim does not widen authority"
    );
    assert!(
        !pair.a.claim_is_current(&old).await.unwrap(),
        "{label} D1 old fenced"
    );

    // D2: the stale owner keeps committing while the winner renews.
    let (stale, renewed, concurrent) = race(
        false,
        {
            let (store, run, epoch) = (pair.a.clone(), run.clone(), old.epoch);
            async move {
                store
                    .settle(&run, epoch, DispatchOutcome::Done, &[])
                    .await
                    .unwrap()
            }
        },
        {
            let (store, current) = (pair.b.clone(), current.clone());
            async move { store.renew_lease(&current, LONG_LEASE_MS).await.unwrap() }
        },
    )
    .await;
    assert_eq!(concurrent, 2, "{label} D2 NC");
    assert_eq!(stale, SettleOutcome::Fenced, "{label} D2 stale settle");
    assert_eq!(renewed, LeaseRenewalOutcome::Current, "{label} D2 renew");
    assert_eq!(
        pair.a.renew_lease(&old, LONG_LEASE_MS).await.unwrap(),
        LeaseRenewalOutcome::Lost,
        "{label} D2 stale renew"
    );
    assert_eq!(
        pair.a.renew_lease(&loser, LONG_LEASE_MS).await.unwrap(),
        LeaseRenewalOutcome::Lost,
        "{label} D2 loser renew"
    );

    // D3
    assert_eq!(
        pair.b.begin_attempt(&current, None).await.unwrap(),
        AttemptAdmission::Applied,
        "{label} D3"
    );
    assert_eq!(
        pair.b.finish_attempt(&current, None).await.unwrap(),
        SettleOutcome::Applied,
        "{label} D3"
    );
    assert_eq!(
        pair.a
            .settle(&run, current.epoch, DispatchOutcome::Done, &[])
            .await
            .unwrap(),
        SettleOutcome::Applied,
        "{label} D3 current settles"
    );
    assert_eq!(
        pair.b
            .settle(&run, old.epoch, DispatchOutcome::Done, &[])
            .await
            .unwrap(),
        SettleOutcome::Fenced,
        "{label} D3 stale replay after settlement"
    );
    assert!(
        !pair
            .a
            .list_dispatches()
            .await
            .unwrap()
            .iter()
            .any(|row| row.run_id == run),
        "{label} D3 settled once"
    );
    let _ = pair.time.now().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_takeover_admits_only_the_current_claim_on_volatile_and_file_sqlite() {
    // NC: the same probe reports a harness-side lock as non-concurrent.
    let (_, _, concurrent) = race(true, async { tokio::task::yield_now().await }, async {
        tokio::task::yield_now().await
    })
    .await;
    assert_eq!(concurrent, 1, "NC: a serialising harness is detected");
    dispatch_takeover(&Pair::volatile()).await;
    dispatch_takeover(&Pair::sqlite_file().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_dispatch_takeover_admits_only_the_current_claim() {
    let Some(pair) = Pair::postgres("t_dispatch_lease_takeover").await else {
        return;
    };
    dispatch_takeover(&pair).await;
}

#[test]
fn postgres_dispatch_takeover_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_dispatch_takeover_admits_only_the_current_claim",
    );
}

#[test]
#[ignore = "product gap: Work and Dispatch lease epochs are both exposed as raw u64, so one domain's epoch type-checks as the other's"]
fn work_and_dispatch_epochs_are_distinct_types() {
    let dispatch = awaken_dispatch_contract::DispatchLease::from_persisted(
        RunId("run".into()),
        "owner".into(),
        0,
        3,
    )
    .unwrap();
    let work = awaken_session_types::SessionWorkLease::from_persisted(
        "work",
        "env",
        awaken_session_types::SessionId::from("session"),
        "owner",
        3,
        0,
    )
    .unwrap();
    // `DispatchSettlement::settle` takes this same type as its fence.
    assert_ne!(
        std::any::type_name_of_val(&dispatch.epoch()),
        std::any::type_name_of_val(&work.epoch()),
        "a Work epoch can be passed wherever a Dispatch epoch is expected"
    );
}
