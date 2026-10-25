//! T22-C02 (生产写路径崩溃), PostgreSQL rows. The SQLite rows of the same
//! decision table are `src/sqlite/transaction_crash_tests.rs`
//! (`process_death_inside_mutation_rolls_back_heads_history_and_conflicts`,
//! `process_death_after_acknowledgement_preserves_committed_content_and_redaction`).
//!
//! An owned child process runs the PRODUCTION `update_head_as` / `rename`
//! against a fresh schema. A `BEFORE INSERT` trigger on the version history
//! counts history inserts and, at the selected ordinal, either raises (SQL
//! error rows) or waits on an advisory lock the parent holds (process-death
//! rows), so the child is provably stopped inside the production transaction.
//! The parent then terminates the child process, and every observation goes
//! through a newly opened pool.
//!
//! | Row | Operation | Cut | Expected after reopen |
//! |---|---|---|---|
//! | U1 | update | death before the 1st history insert (head already rewritten) | old state, then the retry applies once |
//! | N1 | rename-overwrite | death after the destination delete, before its Deleted history | old state |
//! | N2 | rename-overwrite | death after the Deleted history, before the Modified history | old state |
//! | U2/N3 | update / rename | death after acknowledgement | the complete new state |
//! | E1/E2 | update / rename | SQL error at the 1st history insert | error, old state, retry applies once |
//!
//! Oracles (groups-4): O0 a reopen sees only the complete old or new state;
//! O1 target, author and history agree; O2 no half overwrite (both paths and
//! the destination's identity are intact or both moved).
//! Negative control (executed): the cut is proven to be inside the production
//! statement (the blocked backend's query is the production history INSERT of
//! the child's own session), not a hand-written test transaction.

#![cfg(feature = "postgres")]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use awaken_memory_store::{
    Memory, MemoryActor, MemoryCopyConflictPreservation, MemoryCopyConflictQueries,
    MemoryFilesystem, MemoryQueries, MemorySnapshotSource, MemoryVersion, MemoryVersionOperation,
    MemoryVersionQueries, MemoryWriter, PostgresMemoryRepository,
};
use awaken_resource_contract::MemoryCopyConflict;
use sqlx::Executor as _;
use sqlx::postgres::PgPool;

#[path = "common/pg_schema.rs"]
mod pg;

const CHILD: &str = "postgres_write_path_crash_child";
// Independent tests may run concurrently in the same database. Both the lock
// and the backend witness must identify only this test's owned child.
static NEXT_LOCK: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);

fn child_application(schema: &str) -> String {
    let name = format!("t22-child-{schema}");
    assert!(
        name.len() <= 63,
        "PostgreSQL must not truncate the child identity"
    );
    name
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    heads: Vec<Memory>,
    versions: Vec<MemoryVersion>,
    conflicts: Vec<MemoryCopyConflict>,
    neighbor: Vec<Memory>,
    neighbor_versions: Vec<MemoryVersion>,
}

async fn snapshot(repository: &PostgresMemoryRepository) -> Snapshot {
    Snapshot {
        heads: repository.snapshot_heads("s").await.unwrap(),
        versions: repository.list_versions("s").await.unwrap(),
        conflicts: repository.list_copy_conflicts("s").await.unwrap(),
        neighbor: repository.snapshot_heads("neighbor").await.unwrap(),
        neighbor_versions: repository.list_versions("neighbor").await.unwrap(),
    }
}

fn actor() -> MemoryActor {
    MemoryActor::ApiActor {
        api_key_id: "t22-postgres-crash".into(),
    }
}

async fn repository(url: &str, schema: &str, application: &str) -> PostgresMemoryRepository {
    PostgresMemoryRepository::with_existing_pool(
        pg::connect(url, schema, application).await,
        awaken_time::system_clock(),
    )
    .await
    .expect("open the existing Memory schema")
}

/// The production command under test.
async fn mutate(repository: &PostgresMemoryRepository, operation: &str) {
    match operation {
        "update" => {
            let base = MemoryQueries::get_by_path(repository, "s", "/from.md")
                .await
                .unwrap()
                .unwrap();
            MemoryWriter::update_head_as(
                repository,
                "s",
                &base.generation(),
                "changed",
                None,
                Some(&actor()),
            )
            .await
            .unwrap();
        }
        "rename" => {
            MemoryFilesystem::rename(repository, "s", "/from.md", "/to.md")
                .await
                .unwrap();
        }
        _ => panic!("unknown T22 operation {operation}"),
    }
}

/// O0-O2 for the complete new state of `operation`.
fn assert_committed(operation: &str, original: &Snapshot, current: &Snapshot) {
    assert_eq!(
        current.neighbor, original.neighbor,
        "{operation}: neighbour heads"
    );
    assert_eq!(
        current.neighbor_versions, original.neighbor_versions,
        "{operation}: neighbour history"
    );
    assert_eq!(
        current.conflicts, original.conflicts,
        "{operation}: conflicts"
    );
    assert_eq!(
        &current.versions[..original.versions.len()],
        original.versions.as_slice(),
        "{operation}: history is append-only"
    );
    let source = original
        .heads
        .iter()
        .find(|head| head.path == "/from.md")
        .unwrap();
    let destination = original
        .heads
        .iter()
        .find(|head| head.path == "/to.md")
        .unwrap();
    match operation {
        "update" => {
            assert_eq!(current.heads.len(), 2, "update keeps both heads");
            let updated = current.heads.iter().find(|h| h.path == "/from.md").unwrap();
            assert_eq!(updated.id, source.id, "O1 same identity");
            assert_eq!(updated.version, source.version + 1, "O1 one version step");
            assert_eq!(
                updated.content.as_deref(),
                Some("changed"),
                "O0 new content"
            );
            assert!(current.heads.contains(destination), "update leaves /to.md");
            assert_eq!(
                current.versions.len(),
                original.versions.len() + 1,
                "O1 one history row"
            );
            let row = current.versions.last().unwrap();
            assert_eq!(row.memory_id, source.id, "O1 history targets the head");
            assert_eq!(row.created_by, Some(actor()), "O1 the author is recorded");
            assert_eq!(
                row.content.as_deref(),
                Some("changed"),
                "O1 history content"
            );
        }
        "rename" => {
            assert_eq!(current.heads.len(), 1, "O2 the destination was replaced");
            let moved = &current.heads[0];
            assert_eq!(moved.id, source.id, "O2 the source identity moved");
            assert_eq!(moved.path, "/to.md", "O2 at the destination path");
            assert_eq!(moved.content, source.content, "O2 with the source content");
            assert_eq!(
                current.versions.len(),
                original.versions.len() + 2,
                "O1 two history rows"
            );
            let displaced = &current.versions[original.versions.len()];
            assert_eq!(displaced.memory_id, destination.id, "O1 displaced target");
            assert_eq!(displaced.operation, MemoryVersionOperation::Deleted);
            let renamed = &current.versions[original.versions.len() + 1];
            assert_eq!(renamed.memory_id, source.id, "O1 moved head");
            assert_eq!(renamed.operation, MemoryVersionOperation::Modified);
            assert_eq!(renamed.path, "/to.md");
        }
        _ => unreachable!(),
    }
}

/// Seed the decision-table state through the production writers.
async fn seed(repository: &PostgresMemoryRepository) {
    MemoryWriter::create_as(repository, "s", "/from.md", "source", Some(&actor()))
        .await
        .unwrap();
    MemoryWriter::create(repository, "s", "/to.md", "destination")
        .await
        .unwrap();
    MemoryWriter::create(repository, "neighbor", "/from.md", "untouched")
        .await
        .unwrap();
    repository
        .preserve_copy_conflict(
            "s",
            &MemoryCopyConflict {
                path: "/from.md".into(),
                base: None,
                content: Some("losing proposal".into()),
            },
        )
        .await
        .unwrap();
}

/// Install the history-insert cut: at `ordinal`, `mode` is `error` (raise) or
/// `wait` (block on advisory lock `key`, which the parent holds).
async fn install_cut(pool: &PgPool, ordinal: i32, mode: &str, key: i64) {
    pool.execute(
        "CREATE TABLE t22_cut_state (id INT PRIMARY KEY, target INT NOT NULL, \
         mode TEXT NOT NULL, lock_key BIGINT NOT NULL, seen INT NOT NULL DEFAULT 0)",
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO t22_cut_state (id, target, mode, lock_key) VALUES (1, $1, $2, $3)")
        .bind(ordinal)
        .bind(mode)
        .bind(key)
        .execute(pool)
        .await
        .unwrap();
    pool.execute(
        "CREATE FUNCTION t22_cut() RETURNS trigger LANGUAGE plpgsql AS $$ \
         DECLARE state t22_cut_state%ROWTYPE; \
         BEGIN \
           UPDATE t22_cut_state SET seen = seen + 1 WHERE id = 1 RETURNING * INTO state; \
           IF state.seen = state.target THEN \
             IF state.mode = 'error' THEN RAISE EXCEPTION 't22 injected write-path failure'; END IF; \
             PERFORM pg_advisory_xact_lock(state.lock_key); \
           END IF; \
           RETURN NEW; \
         END $$",
    )
    .await
    .unwrap();
    pool.execute(
        "CREATE TRIGGER t22_cut BEFORE INSERT ON memory_store_versions \
         FOR EACH ROW EXECUTE FUNCTION t22_cut()",
    )
    .await
    .unwrap();
}

async fn remove_cut(pool: &PgPool) {
    pool.execute("DROP TRIGGER t22_cut ON memory_store_versions")
        .await
        .unwrap();
    pool.execute("DROP FUNCTION t22_cut()").await.unwrap();
    pool.execute("DROP TABLE t22_cut_state").await.unwrap();
}

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn spawn_child(
    url: &str,
    schema: &str,
    operation: &str,
    marker: &Path,
    diagnostics: &Path,
) -> OwnedChild {
    OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CHILD, "--ignored", "--nocapture"])
            .env("AWAKEN_T22_URL", url)
            .env("AWAKEN_T22_SCHEMA", schema)
            .env("AWAKEN_T22_OPERATION", operation)
            .env("AWAKEN_T22_MARKER", marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(diagnostics).unwrap())
            .spawn()
            .unwrap(),
    )
}

/// The query text of the child's backend while it waits on `key`, once it does.
async fn blocked_child_query(
    observer: &PgPool,
    key: i64,
    application: &str,
    child: &mut OwnedChild,
    diagnostics: &Path,
) -> String {
    tokio::time::timeout(Duration::from_secs(60), async {
        // awaken-allow(wait): observes PostgreSQL's own lock table for the child backend's wait; no product fact marks a blocked transaction.
        loop {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "child exited before its cut: {}",
                std::fs::read_to_string(diagnostics).unwrap_or_default()
            );
            let blocked: Option<String> = sqlx::query_scalar(
                "SELECT a.query FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid \
                 WHERE l.locktype = 'advisory' AND l.objid = $1::bigint::oid AND NOT l.granted \
                 AND a.application_name = $2",
            )
            .bind(key)
            .bind(application)
            .fetch_optional(observer)
            .await
            .unwrap();
            if let Some(query) = blocked {
                return query;
            }
            // awaken-allow(wait): poll interval of the bounded lock-table observation above.
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the child reaches its production cut within the deadline")
}

/// After the kill and the lock release, the child's backend must be gone.
async fn await_child_backend_gone(observer: &PgPool, application: &str) {
    tokio::time::timeout(Duration::from_secs(60), async {
        // awaken-allow(wait): waits for PostgreSQL to reap the dead client's backend; the rollback has no product-visible fact.
        loop {
            let remaining: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity WHERE application_name = $1",
            )
            .bind(application)
            .fetch_one(observer)
            .await
            .unwrap();
            if remaining == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the dead child's backend is reaped within the deadline");
}

async fn await_marker(child: &mut OwnedChild, marker: &Path, diagnostics: &Path) {
    tokio::time::timeout(Duration::from_secs(60), async {
        // awaken-allow(wait): bounded native child readiness at its post-acknowledgement cut.
        loop {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "child exited before acknowledging: {}",
                std::fs::read_to_string(diagnostics).unwrap_or_default()
            );
            if std::fs::read(marker).ok().as_deref() == Some(b"acknowledged") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the child acknowledges its mutation within the deadline");
}

fn kill(child: &mut OwnedChild) {
    child.0.kill().unwrap();
    assert!(
        !child.0.wait().unwrap().success(),
        "a normal exit is not process interruption"
    );
}

/// U1/N1/N2: process death inside the production transaction.
async fn death_inside(url: &str, operation: &str, ordinal: i32) {
    let Some((_, schema)) = pg::fresh_schema(&format!("t22_in_{operation}_{ordinal}")).await else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let setup = repository_with_schema(url, &schema).await;
    seed(&setup).await;
    let original = snapshot(&setup).await;
    let admin = pg::connect(url, &schema, "t22-parent").await;
    let key = i64::from(std::process::id() % 1_000_000) * 1_000
        + NEXT_LOCK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let application = child_application(&schema);
    install_cut(&admin, ordinal, "wait", key).await;
    let mut holder = tokio::time::timeout(Duration::from_secs(10), admin.acquire())
        .await
        .expect("the parent acquires its lock-holder connection")
        .unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(key)
        .execute(&mut *holder)
        .await
        .unwrap();

    let (marker, diagnostics) = (root.path().join("marker"), root.path().join("child.stderr"));
    let mut child = spawn_child(url, &schema, operation, &marker, &diagnostics);
    let query = blocked_child_query(&admin, key, &application, &mut child, &diagnostics).await;
    assert!(
        query.contains("INSERT INTO memory_store_versions"),
        "NC: the cut is inside the production history insert, not a test transaction: {query}"
    );
    kill(&mut child);
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(key)
        .execute(&mut *holder)
        .await
        .unwrap();
    drop(holder);
    await_child_backend_gone(&admin, &application).await;

    let reopened = repository(url, &schema, "t22-reopen").await;
    assert_eq!(
        snapshot(&reopened).await,
        original,
        "O0/O2 {operation}#{ordinal}: only the complete old state is visible"
    );
    remove_cut(&admin).await;
    mutate(&reopened, operation).await;
    assert_committed(operation, &original, &snapshot(&reopened).await);
    admin.close().await;
    pg::drop_schema(url, &schema).await;
}

/// U2/N3: process death after the acknowledgement.
async fn death_after(url: &str, operation: &str) {
    let Some((_, schema)) = pg::fresh_schema(&format!("t22_after_{operation}")).await else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let setup = repository_with_schema(url, &schema).await;
    seed(&setup).await;
    let original = snapshot(&setup).await;
    let (marker, diagnostics) = (root.path().join("marker"), root.path().join("child.stderr"));
    let mut child = spawn_child(url, &schema, operation, &marker, &diagnostics);
    await_marker(&mut child, &marker, &diagnostics).await;
    kill(&mut child);
    let reopened = repository(url, &schema, "t22-reopen").await;
    let current = snapshot(&reopened).await;
    assert_committed(operation, &original, &current);
    pg::drop_schema(url, &schema).await;
}

/// E1/E2: an SQL error inside the production transaction.
async fn sql_error_inside(url: &str, operation: &str) {
    let Some((_, schema)) = pg::fresh_schema(&format!("t22_err_{operation}")).await else {
        return;
    };
    let setup = repository_with_schema(url, &schema).await;
    seed(&setup).await;
    let original = snapshot(&setup).await;
    let admin = pg::connect(url, &schema, "t22-parent").await;
    install_cut(&admin, 1, "error", 0).await;
    let failed = match operation {
        "update" => {
            let base = MemoryQueries::get_by_path(&setup, "s", "/from.md")
                .await
                .unwrap()
                .unwrap();
            MemoryWriter::update_head_as(
                &setup,
                "s",
                &base.generation(),
                "changed",
                None,
                Some(&actor()),
            )
            .await
            .map(|_| ())
        }
        _ => MemoryFilesystem::rename(&setup, "s", "/from.md", "/to.md")
            .await
            .map(|_| ()),
    };
    assert!(
        failed.is_err(),
        "{operation}: the injected SQL error is reported"
    );
    let reopened = repository(url, &schema, "t22-reopen").await;
    assert_eq!(
        snapshot(&reopened).await,
        original,
        "O0/O2 {operation}: the SQL error rolled the whole command back"
    );
    remove_cut(&admin).await;
    mutate(&reopened, operation).await;
    assert_committed(operation, &original, &snapshot(&reopened).await);
    admin.close().await;
    pg::drop_schema(url, &schema).await;
}

async fn repository_with_schema(url: &str, schema: &str) -> PostgresMemoryRepository {
    let repository = PostgresMemoryRepository::with_pool(
        pg::connect(url, schema, "t22-setup").await,
        awaken_time::system_clock(),
    );
    repository.ensure_schema().await.unwrap();
    repository
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_process_death_inside_the_production_write_path_exposes_only_the_old_state() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    for (operation, ordinal) in [("update", 1), ("rename", 1), ("rename", 2)] {
        death_inside(&url, operation, ordinal).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_process_death_after_acknowledgement_exposes_the_complete_new_state() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    for operation in ["update", "rename"] {
        death_after(&url, operation).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_sql_error_inside_the_production_write_path_rolls_the_command_back() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    for operation in ["update", "rename"] {
        sql_error_inside(&url, operation).await;
    }
}

#[test]
fn postgres_process_death_inside_the_production_write_path_exposes_only_the_old_state_fixture_admission()
 {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_process_death_inside_the_production_write_path_exposes_only_the_old_state",
    );
}

#[test]
fn postgres_process_death_after_acknowledgement_exposes_the_complete_new_state_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_process_death_after_acknowledgement_exposes_the_complete_new_state",
    );
}

#[test]
fn postgres_sql_error_inside_the_production_write_path_rolls_the_command_back_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_sql_error_inside_the_production_write_path_rolls_the_command_back",
    );
}

/// The owned child: runs the production command once, then parks until the
/// parent terminates it. When a `wait` cut is installed it never returns from
/// the command; otherwise it writes the acknowledgement marker.
#[tokio::test]
#[ignore = "owned child invoked and killed by the PostgreSQL process-death tests"]
async fn postgres_write_path_crash_child() {
    let url = std::env::var("AWAKEN_T22_URL").unwrap();
    let schema = std::env::var("AWAKEN_T22_SCHEMA").unwrap();
    let operation = std::env::var("AWAKEN_T22_OPERATION").unwrap();
    let marker = std::env::var_os("AWAKEN_T22_MARKER").unwrap();
    let repository = repository(&url, &schema, &child_application(&schema)).await;
    mutate(&repository, &operation).await;
    std::fs::write(&marker, b"acknowledged").unwrap();
    // awaken-allow(wait): the child is parked at its post-acknowledgement cut; the parent owns and kills it.
    std::future::pending::<()>().await;
}
