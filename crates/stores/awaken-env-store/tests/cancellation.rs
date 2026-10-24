//! T34-C02 / T20-C03: durability of the production Environment `update` and
//! `archive` adapters under cancellation and child-process death INSIDE their
//! write transaction.
//!
//! Pause mechanisms (both stop the production statement after its transaction
//! has begun):
//! - PostgreSQL: a `BEFORE UPDATE` trigger takes `pg_advisory_xact_lock(K)`
//!   while an observer connection holds `K`; the waiter is visible in
//!   `pg_locks`.
//! - File SQLite: a `BEFORE UPDATE` trigger runs an unbounded cross join, so the
//!   production `BEGIN IMMEDIATE` transaction stays open inside its UPDATE
//!   step; an independent `BEGIN IMMEDIATE` probe with `busy_timeout = 0` must
//!   observe SQLITE_BUSY before the child publishes its marker.
//!
//! | Rule | Cause | Effect |
//! |---|---|---|
//! | D1 | caller future aborted while paused (PG) | nothing committed (head, history, intents unchanged) |
//! | D2 | after D1, same single-connection pool | connection is reusable within a bounded wait |
//! | D3 | original operation retried | commits exactly once (one revision + one intent, prior history intact); replay cannot commit twice |
//! | D4 | child process killed while paused (CrashProcess): PG update/archive, file SQLite update/archive | after reopen: D1, then D3, then history intact after a second reopen |
//!
//! The required PG fixture cannot skip: see the `_fixture_admission` twins.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use awaken_env_store::{PostgresEnvRegistry, SqliteEnvRegistry};
use awaken_environment_contract::{
    EnvItem, EnvUpdate, EnvironmentConfig, EnvironmentDefinitionQueries,
    EnvironmentDefinitionStore, EnvironmentRegistrationIntent, EnvironmentRegistrationIntentFilter,
    EnvironmentRegistrationOutbox, EnvironmentRevision, EnvironmentStoreError,
};
use sqlx::Executor;
use sqlx::postgres::{PgConnectOptions, PgPool};

const LOCK_KEY: i64 = 0x7342_0002;
const CHILD: &str = "AWAKEN_T34_ENV_CHILD";
const CHILD_SCHEMA: &str = "AWAKEN_T34_ENV_SCHEMA";
const CHILD_ID: &str = "AWAKEN_T34_ENV_ID";
const CHILD_MARKER: &str = "AWAKEN_T34_ENV_MARKER";
const CHILD_DB: &str = "AWAKEN_T34_ENV_DB";

trait Registry:
    EnvironmentDefinitionQueries + EnvironmentDefinitionStore + EnvironmentRegistrationOutbox
{
}
impl<T> Registry for T where
    T: EnvironmentDefinitionQueries + EnvironmentDefinitionStore + EnvironmentRegistrationOutbox
{
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Update,
    Archive,
}

async fn connect(url: &str, schema: &str, max: u32) -> PgPool {
    let options = url
        .parse::<PgConnectOptions>()
        .unwrap()
        .options([("search_path", schema)]);
    awaken_sql_storage::postgres_pool_options()
        .max_connections(max)
        .acquire_timeout(Duration::from_secs(20))
        .connect_with(options)
        .await
        .expect("connect to the test schema")
}

async fn fresh_schema(url: &str) -> String {
    let admin = awaken_sql_storage::postgres_pool_options()
        .max_connections(1)
        .connect(url)
        .await
        .expect("configured Environment PostgreSQL fixture must connect");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("t_env_t34_{}_{nonce:x}", std::process::id());
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await
        .unwrap();
    admin.close().await;
    schema
}

async fn install_pause_trigger(pool: &PgPool) {
    pool.execute(
        format!(
            "CREATE FUNCTION t34_pause() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN PERFORM pg_advisory_xact_lock({LOCK_KEY}); RETURN NEW; END $$"
        )
        .as_str(),
    )
    .await
    .unwrap();
    pool.execute(
        "CREATE TRIGGER t34_pause BEFORE UPDATE ON env_registry_env \
         FOR EACH ROW EXECUTE FUNCTION t34_pause()",
    )
    .await
    .unwrap();
}

async fn wait_until_paused(observer: &PgPool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    // awaken-allow(wait): the pause is only observable as an ungranted advisory
    // lock in pg_locks of another backend; no committed fact exists mid-transaction.
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted \
             AND objid = $1::bigint::oid",
        )
        .bind(LOCK_KEY)
        .fetch_one(observer)
        .await
        .unwrap();
        if waiting > 0 {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "production write never reached the in-transaction pause"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Every durable fact of one Environment: head, full revision chain, intents.
#[derive(Debug, PartialEq)]
struct Durable {
    head: Option<EnvItem>,
    history: Vec<EnvItem>,
    intents: Vec<EnvironmentRegistrationIntent>,
}

async fn durable<R: Registry + ?Sized>(registry: &R, id: &str) -> Durable {
    let mut history = Vec::new();
    while let Some(item) = registry
        .get_revision(id, EnvironmentRevision(history.len() as u64 + 1))
        .await
        .unwrap()
    {
        history.push(item);
    }
    let mut intents: Vec<_> = registry
        .registration_intents(EnvironmentRegistrationIntentFilter::All)
        .await
        .unwrap()
        .into_iter()
        .filter(|intent| intent.environment_id == id)
        .collect();
    intents.sort_by_key(|intent| intent.revision.0);
    Durable {
        head: registry.get(id).await.unwrap(),
        history,
        intents,
    }
}

fn patch() -> EnvUpdate {
    EnvUpdate {
        name: Some("t34-updated".into()),
        ..Default::default()
    }
}

async fn run_op<R: Registry + ?Sized>(
    registry: &R,
    id: &str,
    base: EnvironmentRevision,
    op: Op,
) -> Result<Option<EnvItem>, EnvironmentStoreError> {
    match op {
        Op::Update => registry.update(id, base, patch()).await,
        Op::Archive => registry.archive(id).await,
    }
}

/// D3: the original operation commits exactly once; returns the new image.
async fn retry_commits_once<R: Registry + ?Sized>(
    registry: &R,
    id: &str,
    before: &Durable,
    op: Op,
) -> Durable {
    let base = before.head.as_ref().unwrap().revision;
    let done = tokio::time::timeout(Duration::from_secs(20), run_op(registry, id, base, op))
        .await
        .expect("D2 connection reusable within a bounded wait")
        .unwrap()
        .unwrap();
    assert_eq!(done.revision.0, base.0 + 1, "D3 {op:?}");
    assert_eq!(
        done.archived_at.is_some(),
        matches!(op, Op::Archive),
        "D3 {op:?}"
    );
    let after = durable(registry, id).await;
    assert_eq!(
        after.history.len(),
        before.history.len() + 1,
        "D3 one revision"
    );
    assert_eq!(
        after.intents.len(),
        before.intents.len() + 1,
        "D3 one intent"
    );
    assert_eq!(
        after.history[..before.history.len()],
        before.history[..],
        "D3 prior history intact"
    );
    assert_eq!(
        after.head.as_ref(),
        Some(&done),
        "D3 head is the retried result"
    );
    assert_eq!(
        after.history.last(),
        Some(&done),
        "D3 last revision is the retry"
    );
    match op {
        Op::Update => assert!(
            matches!(
                registry.update(id, base, patch()).await,
                Err(EnvironmentStoreError::RevisionConflict)
            ),
            "D3 the stale replay cannot commit twice"
        ),
        Op::Archive => assert_eq!(
            registry.archive(id).await.unwrap(),
            Some(done.clone()),
            "D3 archive replay returns the original terminal item"
        ),
    }
    assert_eq!(durable(registry, id).await, after, "D3 exactly once");
    after
}

async fn create<R: Registry + ?Sized>(registry: &R, name: &str) -> EnvItem {
    registry
        .create(
            name.into(),
            "t34".into(),
            BTreeMap::new(),
            EnvironmentConfig::SelfHosted,
        )
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_update_aborted_inside_its_transaction_rolls_back_and_retries_once() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let schema = fresh_schema(&url).await;
    // One connection: a leaked or poisoned connection would block D2.
    let pool = connect(&url, &schema, 1).await;
    let registry = Arc::new(
        PostgresEnvRegistry::with_pool(pool.clone(), awaken_time::system_clock())
            .await
            .unwrap(),
    );
    let item = create(registry.as_ref(), "t34").await;
    let observer = connect(&url, &schema, 2).await;
    install_pause_trigger(&observer).await;
    let before = durable(registry.as_ref(), &item.id).await;

    let mut holder = observer.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(LOCK_KEY)
        .execute(&mut *holder)
        .await
        .unwrap();
    let pending = tokio::spawn({
        let (registry, id) = (registry.clone(), item.id.clone());
        async move { registry.update(&id, item.revision, patch()).await }
    });
    wait_until_paused(&observer).await;
    pending.abort();
    assert!(
        pending.await.unwrap_err().is_cancelled(),
        "D1 caller aborted"
    );
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(LOCK_KEY)
        .execute(&mut *holder)
        .await
        .unwrap();
    drop(holder);

    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(20),
            durable(registry.as_ref(), &item.id)
        )
        .await
        .expect("D2 the single connection is reusable"),
        before,
        "D1 nothing committed"
    );
    retry_commits_once(registry.as_ref(), &item.id, &before, Op::Update).await;
}

/// D4 on PostgreSQL: the child pauses inside the production `op`
/// transaction and is killed; a fresh pool observes nothing partial, the
/// retry commits once, and another fresh pool sees the same history.
async fn postgres_child_kill(test_name: &str, op: Op) {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    if std::env::var_os(CHILD).is_some() {
        let schema = std::env::var(CHILD_SCHEMA).unwrap();
        let id = std::env::var(CHILD_ID).unwrap();
        let marker = std::env::var(CHILD_MARKER).unwrap();
        let pool = connect(&url, &schema, 1).await;
        let registry = PostgresEnvRegistry::with_existing_pool(pool, awaken_time::system_clock())
            .await
            .unwrap();
        let observer = connect(&url, &schema, 1).await;
        let base = registry.get(&id).await.unwrap().unwrap().revision;
        let write = run_op(&registry, &id, base, op);
        tokio::pin!(write);
        tokio::select! {
            _ = &mut write => panic!("the parent holds the pause lock"),
            () = wait_until_paused(&observer) => {}
        }
        std::fs::write(marker, b"paused").unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(60), write).await;
        panic!("parent failed to kill the paused child");
    }

    let schema = fresh_schema(&url).await;
    let pool = connect(&url, &schema, 2).await;
    let registry = PostgresEnvRegistry::with_pool(pool.clone(), awaken_time::system_clock())
        .await
        .unwrap();
    let item = create(&registry, &format!("t34-child-{op:?}")).await;
    install_pause_trigger(&pool).await;
    let before = durable(&registry, &item.id).await;
    let mut holder = pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(LOCK_KEY)
        .execute(&mut *holder)
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("paused.marker");
    let status = awaken_reliability_testkit::CrashProcess::new(test_name, &marker)
        .env(CHILD, "1")
        .env(CHILD_SCHEMA, &schema)
        .env(CHILD_ID, &item.id)
        .env(CHILD_MARKER, &marker)
        .timeout(Duration::from_secs(60))
        .run()
        .expect("child pauses inside the production transaction and is killed");
    assert!(!status.success());
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(LOCK_KEY)
        .execute(&mut *holder)
        .await
        .unwrap();
    drop(holder);
    // The server notices the dead client and rolls its transaction back.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    // awaken-allow(wait): the server reaps a killed client's open transaction
    // asynchronously; pg_stat_activity is the only signal that the rollback ran.
    loop {
        let other_backends: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_stat_activity WHERE datname = current_database() \
             AND state LIKE 'idle in transaction%' AND pid <> pg_backend_pid() \
             AND query LIKE '%env_registry%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if other_backends == 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "D4 the killed client's transaction is never released"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    pool.execute("DROP TRIGGER t34_pause ON env_registry_env")
        .await
        .unwrap();
    drop(registry);
    pool.close().await;

    let reopened = PostgresEnvRegistry::with_existing_pool(
        connect(&url, &schema, 2).await,
        awaken_time::system_clock(),
    )
    .await
    .unwrap();
    assert_eq!(
        durable(&reopened, &item.id).await,
        before,
        "D4 nothing committed"
    );
    let after = retry_commits_once(&reopened, &item.id, &before, op).await;
    drop(reopened);
    let again = PostgresEnvRegistry::with_existing_pool(
        connect(&url, &schema, 1).await,
        awaken_time::system_clock(),
    )
    .await
    .unwrap();
    assert_eq!(
        durable(&again, &item.id).await,
        after,
        "D4 history intact after reopen"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_update_child_killed_inside_its_transaction_rolls_back_and_retries_once() {
    postgres_child_kill(
        "postgres_update_child_killed_inside_its_transaction_rolls_back_and_retries_once",
        Op::Update,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_archive_child_killed_inside_its_transaction_rolls_back_and_retries_once() {
    postgres_child_kill(
        "postgres_archive_child_killed_inside_its_transaction_rolls_back_and_retries_once",
        Op::Archive,
    )
    .await;
}

#[test]
fn postgres_update_aborted_inside_its_transaction_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_update_aborted_inside_its_transaction_rolls_back_and_retries_once",
    );
}

#[test]
fn postgres_update_child_killed_inside_its_transaction_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_update_child_killed_inside_its_transaction_rolls_back_and_retries_once",
    );
}

#[test]
fn postgres_archive_child_killed_inside_its_transaction_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_archive_child_killed_inside_its_transaction_rolls_back_and_retries_once",
    );
}

// ── File SQLite ──────────────────────────────────────────────────────────────

async fn sqlite_raw(path: &Path, busy: Duration) -> sqlx::SqliteConnection {
    use sqlx::ConnectOptions as _;
    sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .busy_timeout(busy)
        .connect()
        .await
        .unwrap()
}

/// A trigger that never finishes on its own: the production UPDATE inside
/// `BEGIN IMMEDIATE` stays open until the process dies.
async fn install_sqlite_spin(path: &Path) {
    let mut admin = sqlite_raw(path, Duration::from_secs(5)).await;
    admin
        .execute(
            "CREATE TABLE t34_spin(x INTEGER); \
             WITH RECURSIVE c(x) AS \
               (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 4000) \
               INSERT INTO t34_spin SELECT x FROM c; \
             CREATE TRIGGER t34_spin BEFORE UPDATE ON env_registry_env BEGIN \
               SELECT COUNT(*) FROM t34_spin a, t34_spin b, t34_spin c; \
             END",
        )
        .await
        .unwrap();
    sqlx::Connection::close(admin).await.unwrap();
}

/// True while some connection holds the database's write transaction.
async fn sqlite_write_transaction_open(path: &Path) -> bool {
    let mut probe = sqlite_raw(path, Duration::ZERO).await;
    let open = match probe.execute("BEGIN IMMEDIATE").await {
        Ok(_) => {
            probe.execute("ROLLBACK").await.unwrap();
            false
        }
        Err(sqlx::Error::Database(error)) if error.message().contains("locked") => true,
        Err(error) => panic!("unexpected SQLite probe error: {error}"),
    };
    sqlx::Connection::close(probe).await.unwrap();
    open
}

/// D4 on file SQLite: kill the child while the production `op` transaction
/// is open inside its UPDATE step; reopen observes nothing partial; the
/// original operation then commits exactly once and survives another reopen.
async fn sqlite_child_kill(test_name: &str, op: Op) {
    if std::env::var_os(CHILD).is_some() {
        let path = std::path::PathBuf::from(std::env::var(CHILD_DB).unwrap());
        let id = std::env::var(CHILD_ID).unwrap();
        let marker = std::env::var(CHILD_MARKER).unwrap();
        let registry = SqliteEnvRegistry::open(path.to_str().unwrap(), awaken_time::system_clock())
            .await
            .unwrap();
        let base = registry.get(&id).await.unwrap().unwrap().revision;
        assert!(
            !sqlite_write_transaction_open(&path).await,
            "no writer before the production operation starts"
        );
        let write = run_op(&registry, &id, base, op);
        tokio::pin!(write);
        let inside = async {
            while !sqlite_write_transaction_open(&path).await {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::select! {
            _ = &mut write => panic!("the spin trigger must keep the write open"),
            () = inside => {}
        }
        std::fs::write(marker, b"paused").unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(60), write).await;
        panic!("parent failed to kill the paused child");
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("env.db");
    let registry = SqliteEnvRegistry::open(path.to_str().unwrap(), awaken_time::system_clock())
        .await
        .unwrap();
    let item = create(&registry, &format!("t34-sqlite-{op:?}")).await;
    let before = durable(&registry, &item.id).await;
    drop(registry);
    install_sqlite_spin(&path).await;

    let marker = dir.path().join("paused.marker");
    let status = awaken_reliability_testkit::CrashProcess::new(test_name, &marker)
        .env(CHILD, "1")
        .env(CHILD_DB, &path)
        .env(CHILD_ID, &item.id)
        .env(CHILD_MARKER, &marker)
        .timeout(Duration::from_secs(60))
        .run()
        .expect("child pauses inside the production transaction and is killed");
    assert!(!status.success());

    let mut admin = sqlite_raw(&path, Duration::from_secs(20)).await;
    admin.execute("DROP TRIGGER t34_spin").await.unwrap();
    sqlx::Connection::close(admin).await.unwrap();
    assert!(
        !sqlite_write_transaction_open(&path).await,
        "D4 the killed child released the write lock"
    );

    let reopened = SqliteEnvRegistry::open(path.to_str().unwrap(), awaken_time::system_clock())
        .await
        .unwrap();
    assert_eq!(
        durable(&reopened, &item.id).await,
        before,
        "D4 nothing committed"
    );
    let after = retry_commits_once(&reopened, &item.id, &before, op).await;
    drop(reopened);
    let again = SqliteEnvRegistry::open(path.to_str().unwrap(), awaken_time::system_clock())
        .await
        .unwrap();
    assert_eq!(
        durable(&again, &item.id).await,
        after,
        "D4 history intact after reopen"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_file_update_child_killed_inside_its_transaction_rolls_back_and_retries_once() {
    sqlite_child_kill(
        "sqlite_file_update_child_killed_inside_its_transaction_rolls_back_and_retries_once",
        Op::Update,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_file_archive_child_killed_inside_its_transaction_rolls_back_and_retries_once() {
    sqlite_child_kill(
        "sqlite_file_archive_child_killed_inside_its_transaction_rolls_back_and_retries_once",
        Op::Archive,
    )
    .await;
}
