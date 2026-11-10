//! T05-C03 (事务与进程死亡), PostgreSQL rows. The file-SQLite rows are
//! `src/sqlite/transaction_crash_tests.rs`.
//!
//! An owned child process runs a PRODUCTION root write on a fresh schema. A
//! `BEFORE INSERT` trigger on the receipt and outbox tables waits on an
//! advisory lock the parent holds at the selected table write, so the child is
//! provably stopped inside the production transaction; the parent then kills
//! the child process and observes through a newly opened pool.
//!
//! | Row | Command | Cut | Expected after the kill |
//! |---|---|---|---|
//! | K1 | create (root + receipt + fact) | outbox INSERT | nothing committed; the retry applies once |
//! | K2 | commit Replace + fact | receipt INSERT | the old root, no receipt, no fact; the retry applies once |
//! | K3 | commit Replace + fact | outbox INSERT | the same |
//! | A1/A2 | create / commit | after the acknowledgement | the complete commit; the retry replays it |
//!
//! Oracles (groups-1): O0 all of one transaction or none; O1 a restart only
//! continues the original intent, with no partial fact. Negative control
//! (executed): the blocked backend's statement is the production INSERT of
//! the child's own session, not a hand-written test transaction.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, PersistedSession, SessionCreateResult,
    SessionIdempotencyReceipt, SessionMutation, SessionMutationPayload, SessionMutationResult,
    SessionRepositoryError,
};
use awaken_session_contract::{LifecycleOutbox, SessionRepository, SessionRepositoryReads};
use awaken_session_store::PostgresManagedSessionRepository;
use awaken_session_types::{SessionId, SessionRevision};
use awaken_store_conformance::session_store::{fact, record, record_update, session};
use sqlx::Executor as _;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

const ID: &str = "sesn_t05_pg_crash";
const CREATE_KEY: &str = "t05:create";
const REPLACE_KEY: &str = "t05:replace";
const CHILD: &str = "postgres_root_write_crash_child";
const CHILD_APPLICATION: &str = "t05-crash-child";

fn create_command() -> (
    PersistedSession,
    IdempotencyRecord,
    Vec<ManagedLifecycleFact>,
) {
    let mut value = session(ID, "before");
    value.revision = SessionRevision(0);
    let payload = SessionMutationPayload::Replace(value.clone());
    (
        value,
        record(CREATE_KEY, &payload),
        vec![fact("t05-created", ID, "session.status_idled")],
    )
}

fn replace_command(seeded: &PersistedSession) -> SessionMutation {
    let mut candidate = seeded.clone();
    candidate.title = Some("after".into());
    let candidate = record_update(seeded, candidate, 0).into_snapshot();
    let payload = SessionMutationPayload::Replace(candidate);
    SessionMutation {
        expected_revision: seeded.revision,
        idempotency: record(REPLACE_KEY, &payload),
        payload,
        lifecycle_facts: vec![fact("t05-replaced", ID, "session.status_idled")],
    }
}

#[derive(Debug, PartialEq)]
struct Observed {
    root: Result<PersistedSession, SessionRepositoryError>,
    create_receipt: Option<SessionIdempotencyReceipt>,
    replace_receipt: Option<SessionIdempotencyReceipt>,
    pending: Vec<ManagedLifecycleFact>,
}

async fn observe(repository: &PostgresManagedSessionRepository) -> Observed {
    let id = SessionId::from(ID);
    Observed {
        root: repository.get(&id).await,
        create_receipt: repository
            .idempotency_receipt(&id, CREATE_KEY)
            .await
            .unwrap(),
        replace_receipt: repository
            .idempotency_receipt(&id, REPLACE_KEY)
            .await
            .unwrap(),
        pending: repository.pending_lifecycle().await.unwrap(),
    }
}

async fn run(
    repository: &PostgresManagedSessionRepository,
    command: &str,
    seeded: Option<&PersistedSession>,
) -> bool {
    match command {
        "create" => {
            let (value, identity, facts) = create_command();
            match repository
                .create("ws_a", value, identity, facts)
                .await
                .unwrap()
            {
                SessionCreateResult::Applied(_) => true,
                SessionCreateResult::Replayed(_) => false,
            }
        }
        "replace" => {
            let mutation = replace_command(seeded.expect("a Replace row has a seeded root"));
            match repository.commit("ws_a", mutation).await.unwrap() {
                SessionMutationResult::Applied { .. } => true,
                SessionMutationResult::Replayed { .. } => false,
                other => panic!("replace answered {other:?}"),
            }
        }
        _ => panic!("unknown T05 command {command}"),
    }
}

fn assert_committed(command: &str, original: &Observed, current: &Observed) {
    let root = current
        .root
        .as_ref()
        .expect("O0 the committed root is visible");
    let pending: Vec<&str> = current.pending.iter().map(|f| f.id.as_str()).collect();
    match command {
        "create" => {
            assert_eq!(root.revision, SessionRevision(1), "O0 revision one");
            assert_eq!(
                current
                    .create_receipt
                    .as_ref()
                    .map(|r| r.committed_revision),
                Some(SessionRevision(1)),
                "O0 the receipt committed with the root"
            );
            assert_eq!(pending, ["t05-created"], "O1 exactly the command's fact");
        }
        "replace" => {
            assert_eq!(
                root.revision,
                SessionRevision(2),
                "O0 the replace committed"
            );
            assert_eq!(root.title.as_deref(), Some("after"), "O0 the new content");
            assert_eq!(
                current.create_receipt, original.create_receipt,
                "create receipt untouched"
            );
            assert_eq!(
                current
                    .replace_receipt
                    .as_ref()
                    .map(|r| r.committed_revision),
                Some(SessionRevision(2)),
                "O0 the receipt committed with the root"
            );
            assert_eq!(pending, ["t05-replaced"], "O1 exactly the command's fact");
        }
        _ => unreachable!(),
    }
}

async fn fresh_schema(url: &str, tag: &str) -> String {
    assert!(matches!(tag, "in" | "after"));
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(url)
        .await
        .expect("configured Session PostgreSQL fixture must connect");
    let schema = format!("t05_{tag}_{}", uuid::Uuid::new_v4().simple());
    admin
        // SQL safety: fixture-owned identifiers are UUID/PID-derived or closed literals.
        .execute(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .await
        .expect("create the owned schema");
    admin.close().await;
    schema
}

async fn connect(url: &str, schema: &str, application: &str) -> PgPool {
    let options = url
        .parse::<PgConnectOptions>()
        .expect("parse the fixture URL")
        .options([("search_path", schema)])
        .application_name(application);
    PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options)
        .await
        .expect("connect the owned schema")
}

async fn drop_schema(url: &str, schema: &str) {
    assert!(
        schema.starts_with("t05_")
            && schema
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    );
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .unwrap();
    admin
        // SQL safety: fixture-owned identifiers are UUID/PID-derived or closed literals.
        .execute(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .await
        .expect("drop the owned schema");
    admin.close().await;
}

async fn install_cut(pool: &PgPool, table: &str, key: i64) {
    pool.execute(
        "CREATE TABLE t05_cut_state (id INT PRIMARY KEY, target_table TEXT NOT NULL, lock_key BIGINT NOT NULL)",
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO t05_cut_state (id, target_table, lock_key) VALUES (1, $1, $2)")
        .bind(table)
        .bind(key)
        .execute(pool)
        .await
        .unwrap();
    pool.execute(
        "CREATE FUNCTION t05_cut() RETURNS trigger LANGUAGE plpgsql AS $$ \
         DECLARE state t05_cut_state%ROWTYPE; \
         BEGIN \
           SELECT * INTO state FROM t05_cut_state WHERE id = 1; \
           IF state.target_table = TG_TABLE_NAME THEN PERFORM pg_advisory_xact_lock(state.lock_key); END IF; \
           RETURN NEW; \
         END $$",
    )
    .await
    .unwrap();
    for target in ["managed_session_idempotency", "managed_lifecycle_outbox"] {
        pool.execute(
            sqlx::AssertSqlSafe(format!(
                "CREATE TRIGGER t05_cut BEFORE INSERT ON {target} FOR EACH ROW EXECUTE FUNCTION t05_cut()"
            )),
        )
        .await
        .unwrap();
    }
}

async fn remove_cut(pool: &PgPool) {
    for target in ["managed_session_idempotency", "managed_lifecycle_outbox"] {
        // SQL safety: fixture-owned identifiers are UUID/PID-derived or closed literals.
        pool.execute(sqlx::AssertSqlSafe(format!(
            "DROP TRIGGER t05_cut ON {target}"
        )))
        .await
        .unwrap();
    }
    pool.execute("DROP FUNCTION t05_cut()").await.unwrap();
    pool.execute("DROP TABLE t05_cut_state").await.unwrap();
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
    command: &str,
    marker: &Path,
    diagnostics: &Path,
) -> OwnedChild {
    OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CHILD, "--ignored", "--nocapture"])
            .env("AWAKEN_T05_URL", url)
            .env("AWAKEN_T05_SCHEMA", schema)
            .env("AWAKEN_T05_COMMAND", command)
            .env("AWAKEN_T05_MARKER", marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(diagnostics).unwrap())
            .spawn()
            .unwrap(),
    )
}

async fn blocked_child_query(
    observer: &PgPool,
    key: i64,
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
            .bind(CHILD_APPLICATION)
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

async fn await_child_backend_gone(observer: &PgPool) {
    tokio::time::timeout(Duration::from_secs(60), async {
        // awaken-allow(wait): waits for PostgreSQL to reap the dead client's backend; the rollback has no product-visible fact.
        loop {
            let remaining: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity WHERE application_name = $1",
            )
            .bind(CHILD_APPLICATION)
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
    .expect("the child acknowledges within the deadline");
}

fn kill(child: &mut OwnedChild) {
    child.0.kill().unwrap();
    assert!(
        !child.0.wait().unwrap().success(),
        "a normal exit is not process interruption"
    );
}

/// Seed (for Replace rows) and return the seeded root and the observation.
async fn seed(url: &str, schema: &str, command: &str) -> (Option<PersistedSession>, Observed) {
    let repository =
        PostgresManagedSessionRepository::with_pool(connect(url, schema, "t05-setup").await)
            .await
            .expect("Session schema migrations");
    let seeded = if command == "replace" {
        assert!(
            run(&repository, "create", None).await,
            "seed create applies"
        );
        repository.complete_lifecycle("t05-created").await.unwrap();
        Some(repository.get(&SessionId::from(ID)).await.unwrap())
    } else {
        None
    };
    (seeded, observe(&repository).await)
}

async fn reopen(url: &str, schema: &str) -> PostgresManagedSessionRepository {
    PostgresManagedSessionRepository::with_existing_pool(connect(url, schema, "t05-reopen").await)
        .await
        .expect("an independent pool over the existing schema")
}

async fn death_inside(url: &str, command: &str, table: &str, row: i64) {
    let schema = fresh_schema(url, "in").await;
    let (seeded, original) = seed(url, &schema, command).await;
    let admin = connect(url, &schema, "t05-parent").await;
    let key = i64::from(std::process::id() % 1_000_000) * 100 + row;
    install_cut(&admin, table, key).await;
    let mut holder = tokio::time::timeout(Duration::from_secs(10), admin.acquire())
        .await
        .expect("the parent acquires its lock-holder connection")
        .unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(key)
        .execute(&mut *holder)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let (marker, diagnostics) = (root.path().join("marker"), root.path().join("child.stderr"));
    let mut child = spawn_child(url, &schema, command, &marker, &diagnostics);
    let query = blocked_child_query(&admin, key, &mut child, &diagnostics).await;
    assert!(
        query.contains(&format!("INSERT INTO {table}")),
        "NC: the cut is inside the production {table} insert: {query}"
    );
    let reader = reopen(url, &schema).await;
    assert_eq!(
        observe(&reader).await,
        original,
        "O0 {command}/{table}: invisible while held"
    );
    kill(&mut child);
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(key)
        .execute(&mut *holder)
        .await
        .unwrap();
    drop(holder);
    await_child_backend_gone(&admin).await;
    let reopened = reopen(url, &schema).await;
    assert_eq!(
        observe(&reopened).await,
        original,
        "O0 {command}/{table}: nothing committed"
    );
    remove_cut(&admin).await;
    assert!(
        run(&reopened, command, seeded.as_ref()).await,
        "O1 the retry applies once"
    );
    assert_committed(command, &original, &observe(&reopened).await);
    admin.close().await;
    drop_schema(url, &schema).await;
}

async fn death_after(url: &str, command: &str) {
    let schema = fresh_schema(url, "after").await;
    let (seeded, original) = seed(url, &schema, command).await;
    let root = tempfile::tempdir().unwrap();
    let (marker, diagnostics) = (root.path().join("marker"), root.path().join("child.stderr"));
    let mut child = spawn_child(url, &schema, command, &marker, &diagnostics);
    await_marker(&mut child, &marker, &diagnostics).await;
    kill(&mut child);
    let reopened = reopen(url, &schema).await;
    assert_committed(command, &original, &observe(&reopened).await);
    assert!(
        !run(&reopened, command, seeded.as_ref()).await,
        "O1 the retry replays the committed command"
    );
    assert_committed(command, &original, &observe(&reopened).await);
    drop_schema(url, &schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_process_death_inside_a_root_write_rolls_back_root_receipt_and_outbox() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    for (row, (command, table)) in [
        ("create", "managed_lifecycle_outbox"),
        ("replace", "managed_session_idempotency"),
        ("replace", "managed_lifecycle_outbox"),
    ]
    .into_iter()
    .enumerate()
    {
        death_inside(&url, command, table, i64::try_from(row).unwrap()).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_process_death_after_a_root_write_acknowledgement_keeps_the_complete_commit() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    for command in ["create", "replace"] {
        death_after(&url, command).await;
    }
}

#[test]
fn postgres_process_death_inside_a_root_write_rolls_back_root_receipt_and_outbox_fixture_admission()
{
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_process_death_inside_a_root_write_rolls_back_root_receipt_and_outbox",
    );
}

#[test]
fn postgres_process_death_after_a_root_write_acknowledgement_keeps_the_complete_commit_fixture_admission()
 {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_process_death_after_a_root_write_acknowledgement_keeps_the_complete_commit",
    );
}

#[tokio::test]
#[ignore = "owned child invoked and killed by the PostgreSQL T05-C03 process-death tests"]
async fn postgres_root_write_crash_child() {
    let url = std::env::var("AWAKEN_T05_URL").unwrap();
    let schema = std::env::var("AWAKEN_T05_SCHEMA").unwrap();
    let command = std::env::var("AWAKEN_T05_COMMAND").unwrap();
    let marker = std::env::var_os("AWAKEN_T05_MARKER").unwrap();
    let repository = PostgresManagedSessionRepository::with_existing_pool(
        connect(&url, &schema, CHILD_APPLICATION).await,
    )
    .await
    .unwrap();
    let seeded = match command.as_str() {
        "replace" => Some(repository.get(&SessionId::from(ID)).await.unwrap()),
        _ => None,
    };
    run(&repository, &command, seeded.as_ref()).await;
    std::fs::write(&marker, b"acknowledged").unwrap();
    // awaken-allow(wait): the child is parked at its post-acknowledgement cut; the parent owns and kills it.
    std::future::pending::<()>().await;
}
