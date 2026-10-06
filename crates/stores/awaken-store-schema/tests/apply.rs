//! The commit schema bundle is portable, but "portable" only means anything if it
//! actually APPLIES on a real backend. These tests run the bundle end-to-end:
//! embedded SQLite (always) and a real Postgres (skip-on-unreachable), asserting
//! the tables materialize with the token vocabulary expanded to each dialect's
//! concrete column types, plus a v1→full forward migration through the ledger.
//!
//! The schema crate itself names no SQL driver; these are dev-dep-only test
//! harnesses (the two migration runners + their drivers) exercising the shared
//! bundle exactly as the SQLite and Postgres store backends do.

use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
use awaken_store_schema::{COMMIT_BUNDLE_ID, commit_bundle};
use sqlx::Row as _;
use sqlx::SqlitePool;

const NS: &str = "runtime";

fn failure_atomicity_bundle(
    second_statement: &str,
) -> Result<awaken_scoped_migration::MigrationBundle, awaken_scoped_migration::MigrationError> {
    use awaken_scoped_migration::{Migration, MigrationBundle};

    MigrationBundle::new(
        "failure-atomicity",
        vec![
            Migration::new(
                1,
                "create durable value",
                "CREATE TABLE {prefix}_value (id INTEGER)",
            )?,
            Migration::new(2, "complete durable value", second_statement)?,
        ],
    )
}

async fn isolated_postgres_pool(
    url: &str,
    label: &str,
) -> Result<(sqlx::PgPool, sqlx::PgPool, String), Box<dyn std::error::Error>> {
    use sqlx::Executor;

    let admin = sqlx::PgPool::connect(url).await?;
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let schema = format!("t_{label}_{suffix}");
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await?;
    let selected_schema = schema.clone();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |connection, _| {
            let selected_schema = selected_schema.clone();
            Box::pin(async move {
                connection
                    .execute(format!("SET search_path = {selected_schema}").as_str())
                    .await?;
                Ok(())
            })
        })
        .connect(url)
        .await?;
    Ok((admin, pool, schema))
}

async fn drop_postgres_pool(
    admin: sqlx::PgPool,
    pool: sqlx::PgPool,
    schema: &str,
) -> Result<(), sqlx::Error> {
    use sqlx::Executor;

    pool.close().await;
    admin
        .execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
        .await?;
    admin.close().await;
    Ok(())
}

/// The eight committed-thread tables, prefixed with the runtime namespace.
const TABLES: [&str; 8] = [
    "runtime_commit",
    "runtime_message",
    "runtime_state_command",
    "runtime_event",
    "runtime_run_record",
    "runtime_waiting",
    "runtime_thread_version",
    "runtime_commit_receipt",
];

// The bundle applies against a real (embedded) SQLite database: all eight tables
// materialize, and the portable tokens rendered to SQLite's concrete forms —
// `{json}`/`{timestamptz}` → TEXT, `{pk_autoinc}` → INTEGER PRIMARY KEY
// AUTOINCREMENT, `{now}` → CURRENT_TIMESTAMP (asserted by inserting a row and
// reading the defaulted timestamp back).
#[tokio::test]
async fn bundle_applies_on_real_sqlite() {
    let conn = memory_sqlite().await;
    let bundle = commit_bundle().expect("bundle builds");
    let applied = SqliteMigrationRunner::with_prefix(conn.clone(), NS)
        .expect("runner")
        .run_bundle(&bundle)
        .await
        .expect("apply bundle on sqlite");
    assert_eq!(
        applied.len(),
        bundle.migrations().len(),
        "all migrations applied"
    );

    for table in TABLES {
        let count = table_count_sqlite(&conn, table).await;
        assert_eq!(count, 1, "table {table} was created");
    }

    // {json} → TEXT and {timestamptz} → TEXT on the commit table.
    let commit_types = column_types_sqlite(&conn, "runtime_commit").await;
    assert_eq!(
        commit_types.get("phase").map(String::as_str),
        Some("TEXT"),
        "{{json}} rendered to TEXT on SQLite"
    );
    assert_eq!(
        commit_types.get("committed_at").map(String::as_str),
        Some("TEXT"),
        "{{timestamptz}} rendered to TEXT on SQLite"
    );
    // {pk_autoinc} → INTEGER PRIMARY KEY AUTOINCREMENT on the message table.
    let message_types = column_types_sqlite(&conn, "runtime_message").await;
    assert_eq!(
        message_types.get("id").map(String::as_str),
        Some("INTEGER"),
        "{{pk_autoinc}} rendered to an INTEGER primary key on SQLite"
    );

    // {now} default: insert a run record without a timestamp; SQLite fills it.
    sqlx::query(
        "INSERT INTO runtime_run_record (run_id, thread_id, phase) VALUES ('r','t','\"Running\"')",
    )
    .execute(&conn)
    .await
    .expect("insert run record");
    let updated_at: Option<String> =
        sqlx::query_scalar("SELECT updated_at FROM runtime_run_record WHERE run_id='r'")
            .fetch_one(&conn)
            .await
            .expect("read updated_at");
    assert!(
        updated_at.is_some_and(|s| !s.is_empty()),
        "{{now}} default (CURRENT_TIMESTAMP) populated updated_at"
    );
    assert!(
        sqlx::query(
            "INSERT INTO runtime_commit (sequence, thread_id, run_id, phase) VALUES (-1,'t','r-negative','\"Running\"')",
        )
        .execute(&conn)
        .await
        .is_err(),
        "negative SQLite authority is rejected by the forward-compatible constraint trigger"
    );
}

/// A private in-memory SQLite database on one connection that stays open, so
/// every statement of a test sees the same database. Foreign keys stay off, the
/// SQLite default the migration runner is specified against.
async fn memory_sqlite() -> SqlitePool {
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .min_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .in_memory(true)
                .foreign_keys(false),
        )
        .await
        .expect("open sqlite")
}

/// How many tables named `table` exist.
async fn table_count_sqlite(conn: &SqlitePool, table: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1")
        .bind(table)
        .fetch_one(conn)
        .await
        .expect("query sqlite_master")
}

/// Column name → declared type for a SQLite table, via `PRAGMA table_info`.
async fn column_types_sqlite(
    conn: &SqlitePool,
    table: &str,
) -> std::collections::HashMap<String, String> {
    sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(conn)
        .await
        .expect("query pragma")
        .iter()
        .map(|row| (row.get::<String, _>(1), row.get::<String, _>(2)))
        .collect()
}

/// The column names of a SQLite index, in key order, via `PRAGMA index_info`.
async fn index_columns_sqlite(conn: &SqlitePool, index: &str) -> Vec<String> {
    sqlx::query(&format!("PRAGMA index_info({index})"))
        .fetch_all(conn)
        .await
        .unwrap()
        .iter()
        .map(|row| row.get::<String, _>(2))
        .collect()
}

/// The detail lines of SQLite's `EXPLAIN QUERY PLAN` for one query.
async fn query_plan_sqlite<'q>(
    conn: &SqlitePool,
    query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
) -> String {
    query
        .fetch_all(conn)
        .await
        .unwrap()
        .iter()
        .map(|row| row.get::<String, _>(3))
        .collect::<Vec<_>>()
        .join("\n")
}

// Historical-upgrade cause/effect graph: C1 the ledger is empty or ends at any
// published prefix; C2 the same full declaration is started once or repeatedly.
// Effects: E1 only the missing suffix applies in order, E2 every table reaches the
// current shape, and E3 a repeated start is a no-op. Every prefix is a distinct
// compatibility state; checking only v1 would leave intermediate releases blind.
//
// | Rule | historical prefix | startup | Effect |
// | H1 | 0 | full | E1 versions 1..=tip + E2 |
// | H2 | 1..tip-1 | full | E1 missing suffix + E2 |
// | H3 | tip | full | E3 no-op + E2 |
// | H4 | any after E2 | full again | E3 no-op |
#[tokio::test]
async fn every_sqlite_historical_prefix_migrates_to_full() {
    let full = commit_bundle().expect("bundle builds");
    for prefix_len in 0..=full.migrations().len() {
        let conn = memory_sqlite().await;
        let runner = SqliteMigrationRunner::with_prefix(conn.clone(), NS).expect("runner");
        if prefix_len > 0 {
            let prefix = awaken_scoped_migration::MigrationBundle::new(
                COMMIT_BUNDLE_ID,
                full.migrations()[..prefix_len].to_vec(),
            )
            .expect("historical prefix");
            runner
                .run_bundle(&prefix)
                .await
                .expect("apply historical prefix");
        }
        let delta = runner.run_bundle(&full).await.expect("apply forward");
        let versions: Vec<i64> = delta.iter().map(|migration| migration.version).collect();
        let expected =
            ((prefix_len + 1) as i64..=full.migrations().len() as i64).collect::<Vec<_>>();
        assert_eq!(versions, expected, "H1/H2/H3 prefix={prefix_len}");
        assert!(
            runner
                .run_bundle(&full)
                .await
                .expect("repeat full")
                .is_empty(),
            "H4 prefix={prefix_len}"
        );
        for table in TABLES {
            let count = table_count_sqlite(&conn, table).await;
            assert_eq!(count, 1, "H1/H2/H3 {table} prefix={prefix_len}");
        }
    }
}

#[tokio::test]
async fn sqlite_v10_canonicalizes_only_historical_audit_kind_values() {
    // Causes: C1 v9 contains RunPhaseChanged; C2 it contains RunWaiting; C3 it
    // contains either current spelling; C4 it contains an unknown/corrupt value;
    // C5 the full bundle is retried. Effects: E1 C1/C2 become their current
    // names; E2 C3/C4 remain byte-for-byte values; E3 v10 is recorded once and
    // retry is a no-op. Decision table: R1=C1->E1, R2=C2->E1, R3=C3|C4->E2,
    // R4=C5->E3. Exact-match updates avoid turning migration into a decoder.
    let connection = memory_sqlite().await;
    let runner = SqliteMigrationRunner::with_prefix(connection.clone(), NS).unwrap();
    let full = commit_bundle().unwrap();
    let full = awaken_scoped_migration::MigrationBundle::new(
        COMMIT_BUNDLE_ID,
        full.migrations()[..10].to_vec(),
    )
    .unwrap();
    let v9 = awaken_scoped_migration::MigrationBundle::new(
        COMMIT_BUNDLE_ID,
        full.migrations()[..9].to_vec(),
    )
    .unwrap();
    runner.run_bundle(&v9).await.unwrap();
    for (sequence, kind) in [
        (1_i64, "\"RunPhaseChanged\""),
        (2, "\"RunWaiting\""),
        (3, "\"RunStateChanged\""),
        (4, "\"RunAwaiting\""),
        (5, "\"UnknownHistoricalValue\""),
    ] {
        sqlx::query(
            "INSERT INTO runtime_event (sequence, run_id, kind, payload) VALUES (?1, 'run', ?2, 'null')",
        )
        .bind(sequence)
        .bind(kind)
        .execute(&connection)
        .await
        .unwrap();
    }

    let applied = runner.run_bundle(&full).await.unwrap();
    assert_eq!(
        applied
            .iter()
            .map(|migration| migration.version)
            .collect::<Vec<_>>(),
        [10],
        "R1/R2/R3"
    );
    let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM runtime_event ORDER BY sequence")
        .fetch_all(&connection)
        .await
        .unwrap();
    assert_eq!(
        kinds,
        [
            "\"RunStateChanged\"",
            "\"RunAwaiting\"",
            "\"RunStateChanged\"",
            "\"RunAwaiting\"",
            "\"UnknownHistoricalValue\"",
        ],
        "R1/R2/E1 and R3/E2"
    );
    assert!(runner.run_bundle(&full).await.unwrap().is_empty(), "R4/E3");
}

#[tokio::test]
async fn sqlite_v10_rolls_back_both_updates_and_its_ledger_row() {
    // Causes: C1 v9 contains both historical values; C2 the second UPDATE is
    // forced to fail; C3 the failure is removed and startup retries. Effects:
    // E1 neither the first UPDATE nor v10's ledger row survives C2; E2 C3 applies
    // both updates once. Decision table: R1=C1+C2->E1; R2=C1+C3->E2. This pins
    // the two-statement data upgrade to the existing runner transaction.
    let connection = memory_sqlite().await;
    let runner = SqliteMigrationRunner::with_prefix(connection.clone(), NS).unwrap();
    let full = commit_bundle().unwrap();
    let full = awaken_scoped_migration::MigrationBundle::new(
        COMMIT_BUNDLE_ID,
        full.migrations()[..10].to_vec(),
    )
    .unwrap();
    let v9 = awaken_scoped_migration::MigrationBundle::new(
        COMMIT_BUNDLE_ID,
        full.migrations()[..9].to_vec(),
    )
    .unwrap();
    runner.run_bundle(&v9).await.unwrap();
    sqlx::query(
        "INSERT INTO runtime_event (sequence, run_id, kind, payload) VALUES (1, 'run', '\"RunPhaseChanged\"', 'null'), (2, 'run', '\"RunWaiting\"', 'null')",
    )
    .execute(&connection)
    .await
    .unwrap();
    sqlx::raw_sql(
        "CREATE TRIGGER reject_waiting_upgrade BEFORE UPDATE ON runtime_event \
         WHEN OLD.kind = '\"RunWaiting\"' BEGIN SELECT RAISE(ABORT, 'injected v10 failure'); END;",
    )
    .execute(&connection)
    .await
    .unwrap();

    assert!(runner.run_bundle(&full).await.is_err(), "R1");
    let kinds: String =
        sqlx::query_scalar("SELECT group_concat(kind, ',') FROM runtime_event ORDER BY sequence")
            .fetch_one(&connection)
            .await
            .unwrap();
    assert_eq!(kinds, "\"RunPhaseChanged\",\"RunWaiting\"", "R1/E1");
    let v10_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM runtime_schema_migrations WHERE bundle_id=?1 AND version=10",
    )
    .bind(COMMIT_BUNDLE_ID)
    .fetch_one(&connection)
    .await
    .unwrap();
    assert_eq!(v10_rows, 0, "R1/E1");

    sqlx::query("DROP TRIGGER reject_waiting_upgrade")
        .execute(&connection)
        .await
        .unwrap();
    assert_eq!(runner.run_bundle(&full).await.unwrap().len(), 1, "R2/E2");
    let kinds: String =
        sqlx::query_scalar("SELECT group_concat(kind, ',') FROM runtime_event ORDER BY sequence")
            .fetch_one(&connection)
            .await
            .unwrap();
    assert_eq!(kinds, "\"RunStateChanged\",\"RunAwaiting\"", "R2/E2");
}

#[tokio::test]
async fn sqlite_failed_migration_rolls_back_schema_and_ledger()
-> Result<(), Box<dyn std::error::Error>> {
    // Failure-atomicity decision table: C1=a bundle contains an earlier valid
    // DDL statement; C2=a later statement fails; C3=startup retries with a
    // corrected declaration. R1(C1+C2)->neither user schema nor ledger commits;
    // R2(C3)->the complete bundle applies from version one. This validates the
    // product runner's BEGIN IMMEDIATE guard and rollback as one boundary.
    let connection = memory_sqlite().await;
    let runner = SqliteMigrationRunner::with_prefix(connection.clone(), "failure_atomicity")?;
    let broken = failure_atomicity_bundle("THIS IS NOT SQL")?;
    assert!(runner.run_bundle(&broken).await.is_err());
    let table_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('failure_atomicity_value', 'failure_atomicity_schema_migrations', 'failure_atomicity_schema_migrations_meta')",
    )
    .fetch_one(&connection)
    .await?;
    assert_eq!(table_count, 0, "R1: failed migration leaked durable state");

    let corrected = failure_atomicity_bundle("ALTER TABLE {prefix}_value ADD COLUMN value TEXT")?;
    let applied = runner.run_bundle(&corrected).await?;
    assert_eq!(
        applied
            .iter()
            .map(|migration| migration.version)
            .collect::<Vec<_>>(),
        [1, 2],
        "R2: retry must apply the complete corrected declaration"
    );
    Ok(())
}

#[tokio::test]
async fn every_postgres_historical_prefix_migrates_to_full() {
    use sqlx::Executor;
    use sqlx::postgres::PgPool;

    let url = std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
    });
    let admin = match PgPool::connect(&url).await {
        Ok(pool) => pool,
        Err(error) => {
            println!("[skip] no Postgres reachable: {error}");
            return;
        }
    };
    let full = commit_bundle().expect("bundle builds");
    for prefix_len in 0..=full.migrations().len() {
        let schema = format!("t_schema_history_{prefix_len}");
        admin
            .execute(format!("DROP SCHEMA IF EXISTS {schema} CASCADE").as_str())
            .await
            .expect("drop historical schema");
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .expect("create historical schema");
        let selected_schema = schema.clone();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .after_connect(move |connection, _| {
                let selected_schema = selected_schema.clone();
                Box::pin(async move {
                    connection
                        .execute(format!("SET search_path = {selected_schema}").as_str())
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .expect("historical schema pool");
        let runner = awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
            pool.clone(),
            NS,
        )
        .expect("runner");
        if prefix_len > 0 {
            let prefix = awaken_scoped_migration::MigrationBundle::new(
                COMMIT_BUNDLE_ID,
                full.migrations()[..prefix_len].to_vec(),
            )
            .expect("historical prefix");
            runner
                .run_bundle(&prefix)
                .await
                .expect("apply historical prefix");
        }
        let delta = runner.run_bundle(&full).await.expect("apply forward");
        let versions = delta
            .iter()
            .map(|migration| migration.version)
            .collect::<Vec<_>>();
        let expected =
            ((prefix_len + 1) as i64..=full.migrations().len() as i64).collect::<Vec<_>>();
        assert_eq!(versions, expected, "H1/H2/H3 prefix={prefix_len}");
        assert!(
            runner
                .run_bundle(&full)
                .await
                .expect("repeat full")
                .is_empty(),
            "H4 prefix={prefix_len}"
        );
        pool.close().await;
    }
    admin.close().await;
}

#[tokio::test]
async fn concurrent_postgres_startup_has_one_migration_applier()
-> Result<(), Box<dyn std::error::Error>> {
    // Concurrency decision table: C1=two application replicas start against an
    // empty namespace; C2=both declare the identical full bundle. R1(C1+C2)->
    // one replica applies every version, the other waits then applies none;
    // R2=the ledger contains each version exactly once. This is the live product
    // proof for the transaction-scoped PostgreSQL advisory lock.
    let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL") else {
        return Ok(());
    };
    let (admin, pool, schema) = isolated_postgres_pool(&url, "concurrent_migration").await?;
    let runner =
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)?;
    let bundle = commit_bundle()?;
    let (left, right) = tokio::join!(runner.run_bundle(&bundle), runner.run_bundle(&bundle));
    let mut applied_counts = [left?.len(), right?.len()];
    applied_counts.sort_unstable();
    assert_eq!(applied_counts, [0, bundle.migrations().len()], "R1");
    let ledger_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM runtime_schema_migrations WHERE bundle_id = $1")
            .bind(COMMIT_BUNDLE_ID)
            .fetch_one(&pool)
            .await?;
    assert_eq!(ledger_rows as usize, bundle.migrations().len(), "R2");

    drop(runner);
    drop_postgres_pool(admin, pool, &schema).await?;
    Ok(())
}

#[tokio::test]
async fn postgres_failed_migration_rolls_back_schema_and_ledger_rows()
-> Result<(), Box<dyn std::error::Error>> {
    // This is the PostgreSQL row of the same portable failure-atomicity table as
    // SQLite. PostgreSQL may retain its empty migration-ledger bootstrap, but
    // the backend-neutral contract is identical: no user DDL and no applied
    // version survives, and a corrected retry starts at version one.
    let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL") else {
        return Ok(());
    };
    let (admin, pool, schema) = isolated_postgres_pool(&url, "failed_migration").await?;
    let runner = awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
        pool.clone(),
        "failure_atomicity",
    )?;
    assert!(
        runner
            .run_bundle(&failure_atomicity_bundle("THIS IS NOT SQL")?)
            .await
            .is_err()
    );
    let user_table: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('failure_atomicity_value')::text")
            .fetch_one(&pool)
            .await?;
    assert!(user_table.is_none(), "failed user DDL must roll back");
    let ledger_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM failure_atomicity_schema_migrations WHERE bundle_id = $1",
    )
    .bind("failure-atomicity")
    .fetch_one(&pool)
    .await?;
    assert_eq!(ledger_rows, 0, "failed migration must not claim a version");

    let corrected = failure_atomicity_bundle("ALTER TABLE {prefix}_value ADD COLUMN value TEXT")?;
    assert_eq!(runner.run_bundle(&corrected).await?.len(), 2);
    drop(runner);
    drop_postgres_pool(admin, pool, &schema).await?;
    Ok(())
}

#[tokio::test]
async fn postgres_v10_canonicalizes_historical_kinds_and_rolls_back_as_one_unit()
-> Result<(), Box<dyn std::error::Error>> {
    // Causes: C1 a v9 PostgreSQL store contains both historical Audit Kinds; C2
    // a database constraint rejects the second canonical value; C3 the blocking
    // condition is removed and migration retries. Effects: E1 C2 rolls back the
    // first UPDATE, the second UPDATE, and v10's ledger row; E2 C3 publishes both
    // canonical values; E3 a repeat is a no-op. Decision table:
    // R1=C1+C2->E1; R2=C1+C3->E2; R3=R2+repeat->E3.
    let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL") else {
        return Ok(());
    };
    let (admin, pool, schema) = isolated_postgres_pool(&url, "audit_kind_v10").await?;
    let runner =
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)?;
    let full = commit_bundle()?;
    let full = awaken_scoped_migration::MigrationBundle::new(
        COMMIT_BUNDLE_ID,
        full.migrations()[..10].to_vec(),
    )?;
    let v9 = awaken_scoped_migration::MigrationBundle::new(
        COMMIT_BUNDLE_ID,
        full.migrations()[..9].to_vec(),
    )?;
    runner.run_bundle(&v9).await?;
    sqlx::query(
        "INSERT INTO runtime_event (sequence, run_id, kind, payload) VALUES \
         (1, 'run', '\"RunPhaseChanged\"'::jsonb, 'null'::jsonb), \
         (2, 'run', '\"RunWaiting\"'::jsonb, 'null'::jsonb)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runtime_event ADD CONSTRAINT reject_run_awaiting \
         CHECK (kind <> '\"RunAwaiting\"'::jsonb)",
    )
    .execute(&pool)
    .await?;

    assert!(runner.run_bundle(&full).await.is_err(), "R1");
    let kinds: Vec<String> =
        sqlx::query_scalar("SELECT kind::text FROM runtime_event ORDER BY sequence")
            .fetch_all(&pool)
            .await?;
    assert_eq!(kinds, ["\"RunPhaseChanged\"", "\"RunWaiting\""], "R1/E1");
    let v10_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM runtime_schema_migrations WHERE bundle_id=$1 AND version=10",
    )
    .bind(COMMIT_BUNDLE_ID)
    .fetch_one(&pool)
    .await?;
    assert_eq!(v10_rows, 0, "R1/E1");

    sqlx::query("ALTER TABLE runtime_event DROP CONSTRAINT reject_run_awaiting")
        .execute(&pool)
        .await?;
    assert_eq!(runner.run_bundle(&full).await?.len(), 1, "R2/E2");
    let kinds: Vec<String> =
        sqlx::query_scalar("SELECT kind::text FROM runtime_event ORDER BY sequence")
            .fetch_all(&pool)
            .await?;
    assert_eq!(kinds, ["\"RunStateChanged\"", "\"RunAwaiting\""], "R2/E2");
    assert!(runner.run_bundle(&full).await?.is_empty(), "R3/E3");

    drop(runner);
    drop_postgres_pool(admin, pool, &schema).await?;
    Ok(())
}

// The bundle applies against a real Postgres (skip-on-unreachable), rendering the
// portable tokens to Postgres's concrete forms: `{json}` → JSONB, `{timestamptz}`
// → TIMESTAMPTZ, `{pk_autoinc}` → BIGSERIAL, `{now}` → now() (asserted via a
// defaulted insert). Mirrors the SQLite test on the other dialect so the single
// portable schema is proven on both backends it drives.
#[tokio::test]
async fn bundle_applies_on_real_postgres() {
    use sqlx::Row;
    use sqlx::postgres::PgPool;

    let url = std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
    });
    let admin = match PgPool::connect(&url).await {
        Ok(pool) => pool,
        Err(err) => {
            println!("[skip] no Postgres reachable: {err}");
            return;
        }
    };
    // Isolate in a fresh schema so this never collides with other suites.
    let schema = "t_schema_apply";
    use sqlx::Executor;
    let _ = admin
        .execute(format!("DROP SCHEMA IF EXISTS {schema} CASCADE").as_str())
        .await;
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await
        .expect("create schema");
    admin.close().await;

    let pool = sqlx::postgres::PgPoolOptions::new()
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                conn.execute(format!("SET search_path = {schema}").as_str())
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .expect("schema pool");

    let bundle = commit_bundle().expect("bundle builds");
    let applied =
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .expect("runner")
            .run_bundle(&bundle)
            .await
            .expect("apply bundle on postgres");
    assert_eq!(
        applied.len(),
        bundle.migrations().len(),
        "all migrations applied"
    );

    // Column types rendered to Postgres forms (information_schema.columns).
    let legacy_phase_type: String = sqlx::query(
        "SELECT data_type FROM information_schema.columns \
         WHERE table_schema=$1 AND table_name='runtime_commit' AND column_name='phase'",
    )
    .bind(schema)
    .fetch_one(&pool)
    .await
    .expect("legacy phase type")
    .get("data_type");
    assert_eq!(
        legacy_phase_type, "jsonb",
        "{{json}} rendered to JSONB on Postgres"
    );

    let ts_type: String = sqlx::query(
        "SELECT data_type FROM information_schema.columns \
         WHERE table_schema=$1 AND table_name='runtime_commit' AND column_name='committed_at'",
    )
    .bind(schema)
    .fetch_one(&pool)
    .await
    .expect("committed_at type")
    .get("data_type");
    assert_eq!(
        ts_type, "timestamp with time zone",
        "{{timestamptz}} rendered to TIMESTAMPTZ on Postgres"
    );

    // {pk_autoinc} → BIGSERIAL: an auto-incrementing bigint with a sequence default.
    let id_default: Option<String> = sqlx::query(
        "SELECT column_default FROM information_schema.columns \
         WHERE table_schema=$1 AND table_name='runtime_message' AND column_name='id'",
    )
    .bind(schema)
    .fetch_one(&pool)
    .await
    .expect("id default")
    .get("column_default");
    assert!(
        id_default.is_some_and(|d| d.contains("nextval")),
        "{{pk_autoinc}} rendered to a BIGSERIAL (nextval default) on Postgres"
    );

    // {now} default: insert without a timestamp; Postgres fills it via now().
    sqlx::query(
        "INSERT INTO runtime_run_record (run_id, thread_id, phase) VALUES ('r','t','\"Running\"'::jsonb)",
    )
    .execute(&pool)
    .await
    .expect("insert run record");
    let has_ts: bool = sqlx::query_scalar(
        "SELECT updated_at IS NOT NULL FROM runtime_run_record WHERE run_id='r'",
    )
    .fetch_one(&pool)
    .await
    .expect("read updated_at presence");
    assert!(has_ts, "{{now}} default populated updated_at on Postgres");

    assert!(
        sqlx::query(
            "INSERT INTO runtime_commit (sequence, thread_id, run_id, phase) VALUES (-1,'t','r-negative','\"Running\"'::jsonb)",
        )
        .execute(&pool)
        .await
        .is_err(),
        "negative Postgres authority is rejected by CHECK"
    );

    pool.close().await;
}

#[tokio::test]
async fn sqlite_v11_indexes_run_predecessors_without_rewriting_history() {
    // Cause/effect graph: C1 a populated v10 store interleaves repeated Run ids;
    // C2 predecessor reads use Run equality + an exclusive sequence bound;
    // C3 v11 upgrades the store; C4 startup repeats. R1=C1+C2 before/after C3
    // -> identical exact predecessor, including no-match and boundary cases;
    // R2=C3 -> the planner uses the ordered Run index, all stored bytes survive;
    // R3=C4 -> no migration and one index. Fresh/all-prefix/concurrent-start
    // cases are owned by the existing bundle migration tables above. No elapsed
    // time assertion: host load is not a deterministic query-plan oracle.
    let connection = memory_sqlite().await;
    let runner = SqliteMigrationRunner::with_prefix(connection.clone(), NS).unwrap();
    let current = commit_bundle().unwrap();
    let full = awaken_scoped_migration::MigrationBundle::new(
        COMMIT_BUNDLE_ID,
        current.migrations()[..11].to_vec(),
    )
    .unwrap();
    let previous = awaken_scoped_migration::MigrationBundle::new(
        COMMIT_BUNDLE_ID,
        full.migrations()[..10].to_vec(),
    )
    .unwrap();
    runner.run_bundle(&previous).await.unwrap();
    for sequence in 1..=1000_i64 {
        sqlx::query(
            "INSERT INTO runtime_event (sequence, run_id, kind, payload) VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(sequence)
        .bind(format!("run-{}", sequence % 100))
        .bind("\"RunStateChanged\"")
        .bind(format!("{{\"sequence\":{sequence}}}"))
        .execute(&connection)
        .await
        .unwrap();
    }
    let read = || async {
        sqlx::query_as::<_, (i64, String, String, String)>(
            "SELECT sequence, run_id, kind, payload FROM runtime_event ORDER BY sequence",
        )
        .fetch_all(&connection)
        .await
        .unwrap()
    };
    let before = read().await;
    let delta = runner.run_bundle(&full).await.unwrap();
    assert_eq!(
        delta.iter().map(|row| row.version).collect::<Vec<_>>(),
        [11],
        "R2"
    );
    assert_eq!(read().await, before, "R2 retains every stored byte");
    let columns = index_columns_sqlite(&connection, "runtime_event_run_sequence").await;
    assert_eq!(
        columns,
        ["run_id", "sequence"],
        "R2 ordered equality/range index"
    );
    let query = "SELECT sequence FROM runtime_event WHERE run_id=?1 AND sequence<?2 AND kind='\"RunStateChanged\"' ORDER BY sequence DESC LIMIT 1";
    for (run, bound, expected) in [
        ("run-7", 508_i64, Some(507_i64)),
        ("run-7", 507, Some(407)),
        ("run-7", 7, None),
        ("missing", 1001, None),
    ] {
        let actual: Option<i64> = sqlx::query_scalar(query)
            .bind(run)
            .bind(bound)
            .fetch_optional(&connection)
            .await
            .unwrap();
        assert_eq!(actual, expected, "R1 run={run} bound={bound}");
    }
    let explain = format!("EXPLAIN QUERY PLAN {query}");
    let plan = query_plan_sqlite(
        &connection,
        sqlx::query(&explain).bind("run-7").bind(508_i64),
    )
    .await;
    assert!(plan.contains("runtime_event_run_sequence"), "R2 {plan}");
    assert!(runner.run_bundle(&full).await.unwrap().is_empty(), "R3");
}

#[tokio::test]
async fn postgres_v11_indexes_run_predecessors_without_rewriting_history()
-> Result<(), Box<dyn std::error::Error>> {
    // The PostgreSQL row of SQLite's C1-C4/R1-R3 table above. A real migrated
    // database and its actual query planner cover dialect expansion and restart;
    // the existing peer lifecycle tests cover Running/Awaiting/Resumed meaning.
    let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL") else {
        return Ok(());
    };
    let (admin, pool, schema) = isolated_postgres_pool(&url, "event_predecessor_v11").await?;
    let runner =
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)?;
    let current = commit_bundle()?;
    let full = awaken_scoped_migration::MigrationBundle::new(
        COMMIT_BUNDLE_ID,
        current.migrations()[..11].to_vec(),
    )?;
    let previous = awaken_scoped_migration::MigrationBundle::new(
        COMMIT_BUNDLE_ID,
        full.migrations()[..10].to_vec(),
    )?;
    runner.run_bundle(&previous).await?;
    sqlx::query("INSERT INTO runtime_event (sequence, run_id, kind, payload) SELECT n, 'run-' || (n % 100), '\"RunStateChanged\"'::jsonb, jsonb_build_object('sequence', n) FROM generate_series(1, 1000) AS n")
        .execute(&pool).await?;
    let read =
        "SELECT sequence, run_id, kind::text, payload::text FROM runtime_event ORDER BY sequence";
    let before: Vec<(i64, String, String, String)> = sqlx::query_as(read).fetch_all(&pool).await?;
    let delta = runner.run_bundle(&full).await?;
    assert_eq!(
        delta.iter().map(|row| row.version).collect::<Vec<_>>(),
        [11],
        "R2"
    );
    let after: Vec<(i64, String, String, String)> = sqlx::query_as(read).fetch_all(&pool).await?;
    assert_eq!(after, before, "R2 retains every stored value");
    let definition: String = sqlx::query_scalar("SELECT indexdef FROM pg_indexes WHERE schemaname=current_schema() AND indexname='runtime_event_run_sequence'")
        .fetch_one(&pool).await?;
    assert!(definition.contains("(run_id, sequence)"), "R2 {definition}");
    let query = "SELECT sequence FROM runtime_event WHERE run_id=$1 AND sequence<$2 AND kind::text='\"RunStateChanged\"' ORDER BY sequence DESC LIMIT 1";
    for (run, bound, expected) in [
        ("run-7", 508_i64, Some(507_i64)),
        ("run-7", 507, Some(407)),
        ("run-7", 7, None),
        ("missing", 1001, None),
    ] {
        let actual: Option<i64> = sqlx::query_scalar(query)
            .bind(run)
            .bind(bound)
            .fetch_optional(&pool)
            .await?;
        assert_eq!(actual, expected, "R1 run={run} bound={bound}");
    }
    sqlx::query("ANALYZE runtime_event").execute(&pool).await?;
    let plan: Vec<String> = sqlx::query_scalar(&format!("EXPLAIN {query}"))
        .bind("run-7")
        .bind(508_i64)
        .fetch_all(&pool)
        .await?;
    assert!(
        plan.join("\n").contains("runtime_event_run_sequence"),
        "R2 {plan:?}"
    );
    assert!(runner.run_bundle(&full).await?.is_empty(), "R3");
    drop(runner);
    drop_postgres_pool(admin, pool, &schema).await?;
    Ok(())
}

const RECOVERY_INDEX_COLUMNS: [(&str, [&str; 2]); 4] = [
    ("commit_thread_sequence", ["thread_id", "sequence"]),
    ("commit_run_sequence", ["run_id", "sequence"]),
    ("message_thread_id", ["thread_id", "id"]),
    ("state_command_thread_id", ["thread_id", "id"]),
];
const RECOVERY_RANGE_READS: [(&str, &str); 5] = [
    (
        "commit_thread_sequence",
        "SELECT sequence, CAST(phase AS TEXT) FROM runtime_commit WHERE thread_id='thread-7' ORDER BY sequence",
    ),
    (
        "commit_run_sequence",
        "SELECT COUNT(*), '' FROM runtime_commit WHERE run_id='run-7'",
    ),
    (
        "message_thread_id",
        "SELECT id, CAST(data AS TEXT) FROM runtime_message WHERE thread_id='thread-7' ORDER BY id",
    ),
    (
        "message_thread_id",
        "SELECT id, CAST(data AS TEXT) FROM runtime_message WHERE thread_id='thread-7' AND id<208 ORDER BY id DESC LIMIT 2",
    ),
    (
        "state_command_thread_id",
        "SELECT id, CAST(data AS TEXT) FROM runtime_state_command WHERE thread_id='thread-7' ORDER BY id",
    ),
];
const RECOVERY_STORED_ROWS: [&str; 3] = [
    "SELECT sequence, thread_id, run_id, CAST(phase AS TEXT) || '|' || CAST(committed_at AS TEXT) FROM runtime_commit ORDER BY sequence",
    "SELECT id, thread_id, CAST(commit_sequence AS TEXT), CAST(data AS TEXT) FROM runtime_message ORDER BY id",
    "SELECT id, thread_id, CAST(commit_sequence AS TEXT), CAST(data AS TEXT) FROM runtime_state_command ORDER BY id",
];

#[tokio::test]
async fn sqlite_v12_indexes_recovery_ranges_atomically_without_changing_rows() {
    // C1 populated v11 with interleaved Threads/Runs; C2 a conflicting name on
    // the SECOND new index; C3 migration retry; C4 repeated startup; C5 another
    // prefix. R1=C1+C2 => no v12 ledger row or partially installed first index,
    // and all facts survive. R2=C1+C3 => exact old query results and stored rows,
    // all four equality/range index definitions. R3=C4 => no migration/duplicate.
    // R4=C5 => isolated index names. Query-plan checks use real normal planner
    // decisions; do not disable sequential scans or assert noisy wall time.
    let conn = memory_sqlite().await;
    let runner = SqliteMigrationRunner::with_prefix(conn.clone(), NS).unwrap();
    let full = commit_bundle().unwrap();
    let previous = awaken_scoped_migration::MigrationBundle::new(
        COMMIT_BUNDLE_ID,
        full.migrations()[..11].to_vec(),
    )
    .unwrap();
    runner.run_bundle(&previous).await.unwrap();
    // Physical access-path data; typed Runtime semantics remain tested by the
    // existing store conformance suites against the same migrated schema.
    for n in 1..=10000_i64 {
        sqlx::query(
            "INSERT INTO runtime_commit(sequence,thread_id,run_id,phase) VALUES(?1,?2,?3,?4)",
        )
        .bind(n)
        .bind(format!("thread-{}", n % 100))
        .bind(format!("run-{}", n % 1000))
        .bind("\"Running\"")
        .execute(&conn)
        .await
        .unwrap();
        for table in ["message", "state_command"] {
            sqlx::query(&format!(
                "INSERT INTO runtime_{table}(commit_sequence,thread_id,data) VALUES(?1,?2,?3)"
            ))
            .bind(n)
            .bind(format!("thread-{}", n % 100))
            .bind(format!("{{\"sequence\":{n}}}"))
            .execute(&conn)
            .await
            .unwrap();
        }
    }
    let stored = || async {
        let mut rows = Vec::new();
        for query in RECOVERY_STORED_ROWS {
            rows.push(
                sqlx::query_as::<_, (i64, String, String, String)>(query)
                    .fetch_all(&conn)
                    .await
                    .unwrap(),
            );
        }
        rows
    };
    let read = |query: &'static str| {
        let conn = conn.clone();
        async move {
            sqlx::query_as::<_, (i64, String)>(query)
                .fetch_all(&conn)
                .await
                .unwrap()
        }
    };
    let before = stored().await;
    let mut results = Vec::new();
    for (_, query) in RECOVERY_RANGE_READS {
        results.push(read(query).await);
    }
    sqlx::query("CREATE INDEX runtime_commit_run_sequence ON runtime_commit(sequence)")
        .execute(&conn)
        .await
        .unwrap();
    assert!(
        runner.run_bundle(&full).await.is_err(),
        "R1 collision must fail"
    );
    assert_eq!(stored().await, before);
    let partial: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE name='runtime_commit_thread_sequence'",
    )
    .fetch_one(&conn)
    .await
    .unwrap();
    assert_eq!(partial, 0, "R1 first index rolled back");
    let ledger: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM runtime_schema_migrations WHERE version=12 AND bundle_id=?1",
    )
    .bind(COMMIT_BUNDLE_ID)
    .fetch_one(&conn)
    .await
    .unwrap();
    assert_eq!(ledger, 0, "R1 no false version receipt");
    sqlx::query("DROP INDEX runtime_commit_run_sequence")
        .execute(&conn)
        .await
        .unwrap();
    assert_eq!(
        runner
            .run_bundle(&full)
            .await
            .unwrap()
            .iter()
            .map(|m| m.version)
            .collect::<Vec<_>>(),
        [12]
    );
    assert_eq!(
        stored().await,
        before,
        "R2 unchanged facts/times/coordinates"
    );
    for ((name, query), expected) in RECOVERY_RANGE_READS.into_iter().zip(results) {
        assert_eq!(read(query).await, expected, "R2 {name}");
        let explain = format!("EXPLAIN QUERY PLAN {query}");
        let plan = query_plan_sqlite(&conn, sqlx::query(&explain)).await;
        assert!(plan.contains(&format!("runtime_{name}")), "R2 {plan}");
    }
    assert!(runner.run_bundle(&full).await.unwrap().is_empty(), "R3");
    SqliteMigrationRunner::with_prefix(conn.clone(), "peer")
        .unwrap()
        .run_bundle(&full)
        .await
        .unwrap();
    for prefix in ["runtime", "peer"] {
        for (name, expected) in RECOVERY_INDEX_COLUMNS {
            let columns = index_columns_sqlite(&conn, &format!("{prefix}_{name}")).await;
            assert_eq!(columns, expected, "R2/R4 {prefix}_{name}");
        }
    }
}

#[tokio::test]
async fn postgres_v12_indexes_recovery_ranges_atomically_without_changing_rows()
-> Result<(), Box<dyn std::error::Error>> {
    // PostgreSQL executes the same C1-C5/R1-R4 table as SQLite above. All rows
    // are retained, a second-DDL failure rolls back the first index, and real
    // EXPLAIN must select each access path with normal planner settings.
    let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL") else {
        return Ok(());
    };
    let (admin, pool, schema) = isolated_postgres_pool(&url, "recovery_ranges_v12").await?;
    let runner =
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)?;
    let full = commit_bundle()?;
    let previous = awaken_scoped_migration::MigrationBundle::new(
        COMMIT_BUNDLE_ID,
        full.migrations()[..11].to_vec(),
    )?;
    runner.run_bundle(&previous).await?;
    sqlx::query("INSERT INTO runtime_commit(sequence,thread_id,run_id,phase) SELECT n, 'thread-'||(n%100), 'run-'||(n%1000), '\"Running\"'::jsonb FROM generate_series(1,10000) n").execute(&pool).await?;
    for table in ["message", "state_command"] {
        sqlx::query(&format!("INSERT INTO runtime_{table}(commit_sequence,thread_id,data) SELECT sequence,thread_id,jsonb_build_object('sequence',sequence) FROM runtime_commit ORDER BY sequence")).execute(&pool).await?;
    }
    let mut before = Vec::new();
    for query in RECOVERY_STORED_ROWS {
        before.push(
            sqlx::query_as::<_, (i64, String, String, String)>(query)
                .fetch_all(&pool)
                .await?,
        );
    }
    let mut results = Vec::new();
    for (_, query) in RECOVERY_RANGE_READS {
        results.push(
            sqlx::query_as::<_, (i64, String)>(query)
                .fetch_all(&pool)
                .await?,
        );
    }
    sqlx::query("CREATE INDEX runtime_commit_run_sequence ON runtime_commit(sequence)")
        .execute(&pool)
        .await?;
    assert!(
        runner.run_bundle(&full).await.is_err(),
        "R1 collision must fail"
    );
    let partial:i64=sqlx::query_scalar("SELECT COUNT(*) FROM pg_indexes WHERE schemaname=current_schema() AND indexname='runtime_commit_thread_sequence'").fetch_one(&pool).await?;
    assert_eq!(partial, 0, "R1 first index rolled back");
    let ledger: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM runtime_schema_migrations WHERE version=12 AND bundle_id=$1",
    )
    .bind(COMMIT_BUNDLE_ID)
    .fetch_one(&pool)
    .await?;
    assert_eq!(ledger, 0, "R1 no false version receipt");
    sqlx::query("DROP INDEX runtime_commit_run_sequence")
        .execute(&pool)
        .await?;
    assert_eq!(
        runner
            .run_bundle(&full)
            .await?
            .iter()
            .map(|m| m.version)
            .collect::<Vec<_>>(),
        [12]
    );
    for (query, expected) in RECOVERY_STORED_ROWS.into_iter().zip(before) {
        assert_eq!(
            sqlx::query_as::<_, (i64, String, String, String)>(query)
                .fetch_all(&pool)
                .await?,
            expected,
            "R1/R2 unchanged facts"
        );
    }
    for table in ["commit", "message", "state_command"] {
        sqlx::query(&format!("ANALYZE runtime_{table}"))
            .execute(&pool)
            .await?;
    }
    for ((name, query), expected) in RECOVERY_RANGE_READS.into_iter().zip(results) {
        assert_eq!(
            sqlx::query_as::<_, (i64, String)>(query)
                .fetch_all(&pool)
                .await?,
            expected,
            "R2 {name}"
        );
        let plan = sqlx::query_scalar::<_, String>(&format!("EXPLAIN {query}"))
            .fetch_all(&pool)
            .await?
            .join("\n");
        assert!(plan.contains(&format!("runtime_{name}")), "R2 {plan}");
    }
    assert!(runner.run_bundle(&full).await?.is_empty(), "R3");
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), "peer")?
        .run_bundle(&full)
        .await?;
    for prefix in ["runtime", "peer"] {
        for (name, columns) in RECOVERY_INDEX_COLUMNS {
            let definition:String=sqlx::query_scalar("SELECT indexdef FROM pg_indexes WHERE schemaname=current_schema() AND indexname=$1").bind(format!("{prefix}_{name}")).fetch_one(&pool).await?;
            assert!(
                definition.contains(&format!("({}, {})", columns[0], columns[1])),
                "R2/R4 {definition}"
            );
        }
    }
    drop(runner);
    drop_postgres_pool(admin, pool, &schema).await?;
    Ok(())
}
