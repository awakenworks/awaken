//! T05-C01 (并发赢家) and the T11-C03/T18-C01 SQL substrate: two
//! independently opened Session store clients race the same root CAS.
//!
//! The shared oracle rows W1-W6 live in
//! `awaken_store_conformance::session_store::assert_independent_race`. This
//! target supplies the real independent clients per backend and the
//! database-level contention witness that the negative control needs:
//!
//! | Row | Backend | Clients | Contention witness | Expected |
//! |---|---|---|---|---|
//! | B1 | file SQLite | two `open(path)` handles | a read through the second handle completes while the first handle's commit waits on the database write lock | W1-W6; witness holds; reopen keeps W2-W4 |
//! | B2 | PostgreSQL | two pools, distinct `application_name` | both backends wait on the same row lock in `pg_stat_activity` | W1-W6; witness holds |
//! | B3 | in-memory SQLite (Volatile logical model) | one shared handle | n/a: two `open_in_memory` calls are two different authorities | W1-W6 logically; no independent-client claim |
//! | NC1 | file SQLite | ONE shared handle | the second caller's read cannot reach the database | witness fails (detected); W1-W6 still pass, so outcome rows alone cannot prove contention |
//! | NC2 | PostgreSQL | two pools behind one in-process test lock | only one backend ever waits on the row lock | witness fails (detected) |
use std::sync::Arc;
use std::time::Duration;

use awaken_session_application::SessionStore;
use awaken_session_contract::{SessionMutationResult, SessionRepositoryError};
use awaken_session_store::{PostgresManagedSessionRepository, SqliteManagedSessionRepository};
use awaken_store_conformance::session_store::{
    IndependentRace, assert_independent_race, prepare_independent_race,
};
use sqlx::Executor as _;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

type Outcome = Result<SessionMutationResult, SessionRepositoryError>;

async fn raw_sqlite(path: &std::path::Path) -> sqlx::SqlitePool {
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(path)
                .busy_timeout(Duration::from_secs(30)),
        )
        .await
        .expect("open raw SQLite lock holder")
}

/// Hold the SQLite database write lock from a connection outside both clients.
async fn hold_sqlite_write_lock(
    raw: &sqlx::SqlitePool,
) -> sqlx::Transaction<'static, sqlx::Sqlite> {
    raw.begin_with("BEGIN IMMEDIATE")
        .await
        .expect("raw connection takes the database write lock")
}

fn commit_task(
    client: Arc<dyn SessionStore>,
    mutation: awaken_session_contract::SessionMutation,
    gate: Option<Arc<tokio::sync::Mutex<()>>>,
) -> tokio::task::JoinHandle<Outcome> {
    tokio::spawn(async move {
        // `gate` is the NC "shared test lock": it serialises the callers in
        // this process before they can reach the database.
        let _serialised = match &gate {
            Some(gate) => Some(gate.clone().lock_owned().await),
            None => None,
        };
        client.commit("ws_a", mutation).await
    })
}

/// SQLite witness: while `first`'s commit waits on the database lock held by
/// a third connection, a read through `second` must complete. A handle that
/// shares `first`'s one connection queues behind it and cannot.
async fn second_client_reaches_database_while_first_waits(
    second: &dyn SessionStore,
    race: &IndependentRace,
) -> bool {
    tokio::time::timeout(Duration::from_secs(3), second.get(&race.session_id))
        .await
        .is_ok_and(|read| read.is_ok())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_independent_file_clients_have_one_cas_winner() {
    let directory = tempfile::tempdir().expect("T05 SQLite directory");
    let path = directory.path().join("t05-independent.db");
    let path_str = path.to_str().expect("UTF-8 T05 path");
    let left: Arc<dyn SessionStore> = Arc::new(
        SqliteManagedSessionRepository::open(path_str)
            .await
            .expect("left SQLite client"),
    );
    let right: Arc<dyn SessionStore> = Arc::new(
        SqliteManagedSessionRepository::open(path_str)
            .await
            .expect("right SQLite client"),
    );
    let race = prepare_independent_race(left.as_ref(), "sesn_t05_sqlite").await;
    let raw = raw_sqlite(&path).await;
    let lock = hold_sqlite_write_lock(&raw).await;
    let first = commit_task(left.clone(), race.commands[0].mutation.clone(), None);
    // awaken-allow(wait): proves non-occurrence; the raw connection holds the SQLite write lock, so left's commit must still be blocked.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!first.is_finished(), "B1 left waits on the database lock");
    assert!(
        second_client_reaches_database_while_first_waits(right.as_ref(), &race).await,
        "B1 witness: the right client has its own database connection"
    );
    let second = commit_task(right.clone(), race.commands[1].mutation.clone(), None);
    // awaken-allow(wait): proves non-occurrence; both commits must still be blocked at the held database write lock before release.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !first.is_finished() && !second.is_finished(),
        "B1 both commands are in flight at the database lock"
    );
    lock.rollback().await.expect("release the database lock");
    let outcomes = [first.await.unwrap(), second.await.unwrap()];
    let winner =
        assert_independent_race(left.as_ref(), right.as_ref(), &race, outcomes.clone()).await;

    // Persistence clause: every handle closes; two fresh handles reopen the
    // file and observe the same winner, outbox and receipts.
    drop(left);
    drop(right);
    raw.close().await;
    let reopened_left = SqliteManagedSessionRepository::open(path_str)
        .await
        .expect("reopen left");
    let reopened_right = SqliteManagedSessionRepository::open(path_str)
        .await
        .expect("reopen right");
    assert_eq!(
        assert_independent_race(&reopened_left, &reopened_right, &race, outcomes).await,
        winner,
        "B1 reopen keeps the same single winner"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_shared_handle_serialises_before_the_database() {
    // NC1: both callers share ONE handle (one connection). The outcome rows
    // still pass, but the witness detects that the second caller never
    // reached the database while the first was waiting there.
    let directory = tempfile::tempdir().expect("NC1 SQLite directory");
    let path = directory.path().join("t05-shared.db");
    let shared: Arc<dyn SessionStore> = Arc::new(
        SqliteManagedSessionRepository::open(path.to_str().unwrap())
            .await
            .expect("shared SQLite client"),
    );
    let race = prepare_independent_race(shared.as_ref(), "sesn_t05_shared").await;
    let raw = raw_sqlite(&path).await;
    let lock = hold_sqlite_write_lock(&raw).await;
    let first = commit_task(shared.clone(), race.commands[0].mutation.clone(), None);
    // awaken-allow(wait): proves non-occurrence; the NC1 first caller must still be blocked at the held database write lock.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !first.is_finished(),
        "NC1 first caller waits on the database"
    );
    assert!(
        !second_client_reaches_database_while_first_waits(shared.as_ref(), &race).await,
        "NC1 the witness detects a shared connection"
    );
    let second = commit_task(shared.clone(), race.commands[1].mutation.clone(), None);
    lock.rollback().await.expect("release the database lock");
    let outcomes = [first.await.unwrap(), second.await.unwrap()];
    assert_independent_race(shared.as_ref(), shared.as_ref(), &race, outcomes).await;
}

#[tokio::test]
async fn volatile_session_store_has_no_independent_clients() {
    // B3: the only volatile Session store is a private in-memory SQLite
    // database. Capability: a second `open_in_memory` is a different
    // authority, so no independent-client race exists for Volatile. The W1-W6
    // logical model still holds for two callers on the one shared handle.
    let first = SqliteManagedSessionRepository::open_in_memory()
        .await
        .expect("first volatile store");
    let other = SqliteManagedSessionRepository::open_in_memory()
        .await
        .expect("second volatile store");
    let race = prepare_independent_race(&first, "sesn_t05_volatile").await;
    use awaken_session_contract::SessionRepositoryReads as _;
    assert_eq!(
        other.get(&race.session_id).await,
        Err(SessionRepositoryError::NotFound),
        "B3 capability: a second volatile client is another authority"
    );
    let barrier = tokio::sync::Barrier::new(2);
    let submit = |index: usize| {
        let mutation = race.commands[index].mutation.clone();
        let first = &first;
        let barrier = &barrier;
        async move {
            barrier.wait().await;
            awaken_session_contract::SessionRepository::commit(first, "ws_a", mutation).await
        }
    };
    let (left, right) = tokio::join!(submit(0), submit(1));
    assert_independent_race(&first, &first, &race, [left, right]).await;
}

struct PgClients {
    url: String,
    schema: String,
    names: [String; 2],
    pools: [sqlx::PgPool; 2],
    clients: [Arc<dyn SessionStore>; 2],
}

async fn postgres_clients(url: &str) -> PgClients {
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(url)
        .await
        .expect("explicit T05 PostgreSQL fixture must connect");
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let schema = format!("t_session_t05_{tag}");
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await
        .expect("create owned T05 schema");
    admin.close().await;
    let names = [format!("t05-left-{tag}"), format!("t05-right-{tag}")];
    let mut pools = Vec::new();
    for name in &names {
        let options: PgConnectOptions = url.parse().expect("PostgreSQL fixture URL");
        let owned_schema = schema.clone();
        pools.push(
            PgPoolOptions::new()
                .max_connections(2)
                .acquire_timeout(Duration::from_secs(5))
                .after_connect(move |connection, _| {
                    let schema = owned_schema.clone();
                    Box::pin(async move {
                        connection
                            .execute(format!("SET search_path = {schema}").as_str())
                            .await?;
                        Ok(())
                    })
                })
                .connect_with(options.application_name(name))
                .await
                .expect("independent T05 PostgreSQL pool"),
        );
    }
    let pools: [sqlx::PgPool; 2] = pools.try_into().unwrap();
    let left = PostgresManagedSessionRepository::with_pool(pools[0].clone())
        .await
        .expect("left PostgreSQL client migrates the owned schema");
    let right = PostgresManagedSessionRepository::with_existing_pool(pools[1].clone())
        .await
        .expect("right PostgreSQL client opens the existing schema");
    let left_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&pools[0])
        .await
        .unwrap();
    let right_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&pools[1])
        .await
        .unwrap();
    assert_ne!(left_pid, right_pid, "B2 distinct server backends");
    PgClients {
        url: url.into(),
        schema,
        names,
        pools,
        clients: [Arc::new(left), Arc::new(right)],
    }
}

impl PgClients {
    async fn lock_holder(&self) -> sqlx::PgPool {
        let schema = self.schema.clone();
        PgPoolOptions::new()
            .max_connections(1)
            .after_connect(move |connection, _| {
                let schema = schema.clone();
                Box::pin(async move {
                    connection
                        .execute(format!("SET search_path = {schema}").as_str())
                        .await?;
                    Ok(())
                })
            })
            .connect(&self.url)
            .await
            .expect("T05 PostgreSQL lock holder")
    }

    /// PostgreSQL witness: how many distinct client backends wait on a lock.
    async fn lock_waiters(&self, observer: &sqlx::PgPool) -> i64 {
        sqlx::query_scalar(
            "SELECT count(DISTINCT application_name) FROM pg_stat_activity \
             WHERE application_name = ANY($1) AND wait_event_type = 'Lock'",
        )
        .bind(self.names.to_vec())
        .fetch_one(observer)
        .await
        .expect("read pg_stat_activity")
    }

    async fn wait_for_waiters(&self, observer: &sqlx::PgPool, expected: i64) -> i64 {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        // awaken-allow(wait): polls pg_stat_activity for the database's own lock-wait state; no product fact exists for a blocked backend.
        loop {
            let waiters = self.lock_waiters(observer).await;
            if waiters >= expected || tokio::time::Instant::now() >= deadline {
                return waiters;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn close(self) {
        let [left, right] = self.clients;
        drop(left);
        drop(right);
        for pool in &self.pools {
            pool.close().await;
        }
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&self.url)
            .await
            .expect("T05 cleanup connection");
        admin
            .execute(format!("DROP SCHEMA {} CASCADE", self.schema).as_str())
            .await
            .expect("drop exact owned T05 schema");
        admin.close().await;
    }
}

/// Race both PostgreSQL clients while a third connection holds the root row
/// lock; return the number of client backends observed waiting at once.
async fn postgres_contended_race(
    clients: &PgClients,
    race: &IndependentRace,
    gate: Option<Arc<tokio::sync::Mutex<()>>>,
) -> (i64, [Outcome; 2]) {
    let holder = clients.lock_holder().await;
    let mut lock = holder.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM managed_session WHERE session_id = $1 FOR UPDATE")
        .bind(race.session_id.as_str())
        .fetch_one(&mut *lock)
        .await
        .expect("third connection holds the root row lock");
    let first = commit_task(
        clients.clients[0].clone(),
        race.commands[0].mutation.clone(),
        gate.clone(),
    );
    let second = commit_task(
        clients.clients[1].clone(),
        race.commands[1].mutation.clone(),
        gate,
    );
    let observer = clients.lock_holder().await;
    let waiters = clients.wait_for_waiters(&observer, 2).await;
    lock.rollback().await.expect("release the root row lock");
    let outcomes = [first.await.unwrap(), second.await.unwrap()];
    observer.close().await;
    holder.close().await;
    (waiters, outcomes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_independent_pools_have_one_cas_winner() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let clients = postgres_clients(&url).await;
    let race = prepare_independent_race(clients.clients[0].as_ref(), "sesn_t05_postgres").await;
    let (waiters, outcomes) = postgres_contended_race(&clients, &race, None).await;
    assert_eq!(
        waiters, 2,
        "B2 witness: both independent backends wait on the database row lock"
    );
    assert_independent_race(
        clients.clients[0].as_ref(),
        clients.clients[1].as_ref(),
        &race,
        outcomes,
    )
    .await;
    clients.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_test_lock_serialisation_hides_database_contention() {
    // NC2: the same two pools, but a process-local test lock serialises the
    // callers. The outcome rows pass; the witness detects that the database
    // never saw two contending clients.
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let clients = postgres_clients(&url).await;
    let race = prepare_independent_race(clients.clients[0].as_ref(), "sesn_t05_serialised").await;
    let (waiters, outcomes) =
        postgres_contended_race(&clients, &race, Some(Arc::new(tokio::sync::Mutex::new(())))).await;
    assert_eq!(
        waiters, 1,
        "NC2 the witness detects that a test lock serialised the clients"
    );
    assert_independent_race(
        clients.clients[0].as_ref(),
        clients.clients[1].as_ref(),
        &race,
        outcomes,
    )
    .await;
    clients.close().await;
}

#[test]
fn postgres_independent_pools_have_one_cas_winner_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_independent_pools_have_one_cas_winner",
    );
}

#[test]
fn postgres_test_lock_serialisation_hides_database_contention_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_test_lock_serialisation_hides_database_contention",
    );
}
