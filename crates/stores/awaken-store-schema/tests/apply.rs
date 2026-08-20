//! The commit schema bundle is portable, but "portable" only means anything if it
//! actually APPLIES on a real backend. These tests run the bundle end-to-end:
//! embedded SQLite (always) and a real Postgres (skip-on-unreachable), asserting
//! the tables materialize with the token vocabulary expanded to each dialect's
//! concrete column types, plus a v1→full forward migration through the ledger.
//!
//! The schema crate itself names no SQL driver; these are dev-dep-only test
//! harnesses (the two migration runners + their drivers) exercising the shared
//! bundle exactly as the SQLite and Postgres store backends do.

use awaken_scoped_migration_sqlite::SqliteMigrationRunner;
use awaken_store_schema::{COMMIT_BUNDLE_ID, commit_bundle};
use rusqlite::Connection;

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
#[test]
fn bundle_applies_on_real_sqlite() {
    let conn = Connection::open_in_memory().expect("open sqlite");
    let bundle = commit_bundle().expect("bundle builds");
    let applied = SqliteMigrationRunner::with_prefix(NS)
        .expect("runner")
        .run_bundle(&conn, &bundle)
        .expect("apply bundle on sqlite");
    assert_eq!(applied.len(), 9, "all nine migrations applied");

    for table in TABLES {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )
            .expect("query sqlite_master");
        assert_eq!(count, 1, "table {table} was created");
    }

    // {json} → TEXT and {timestamptz} → TEXT on the commit table.
    let commit_types = column_types_sqlite(&conn, "runtime_commit");
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
    let message_types = column_types_sqlite(&conn, "runtime_message");
    assert_eq!(
        message_types.get("id").map(String::as_str),
        Some("INTEGER"),
        "{{pk_autoinc}} rendered to an INTEGER primary key on SQLite"
    );

    // {now} default: insert a run record without a timestamp; SQLite fills it.
    conn.execute(
        "INSERT INTO runtime_run_record (run_id, thread_id, phase) VALUES ('r','t','\"Running\"')",
        [],
    )
    .expect("insert run record");
    let updated_at: Option<String> = conn
        .query_row(
            "SELECT updated_at FROM runtime_run_record WHERE run_id='r'",
            [],
            |row| row.get(0),
        )
        .expect("read updated_at");
    assert!(
        updated_at.is_some_and(|s| !s.is_empty()),
        "{{now}} default (CURRENT_TIMESTAMP) populated updated_at"
    );
    assert!(
        conn.execute(
            "INSERT INTO runtime_commit (sequence, thread_id, run_id, phase) VALUES (-1,'t','r-negative','\"Running\"')",
            [],
        )
        .is_err(),
        "negative SQLite authority is rejected by the forward-compatible constraint trigger"
    );
}

/// Column name → declared type for a SQLite table, via `PRAGMA table_info`.
fn column_types_sqlite(
    conn: &Connection,
    table: &str,
) -> std::collections::HashMap<String, String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("prepare pragma");
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })
        .expect("query pragma");
    rows.map(|r| r.expect("pragma row")).collect()
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
#[test]
fn every_sqlite_historical_prefix_migrates_to_full() {
    let full = commit_bundle().expect("bundle builds");
    for prefix_len in 0..=full.migrations().len() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        let runner = SqliteMigrationRunner::with_prefix(NS).expect("runner");
        if prefix_len > 0 {
            let prefix = awaken_scoped_migration::MigrationBundle::new(
                COMMIT_BUNDLE_ID,
                full.migrations()[..prefix_len].to_vec(),
            )
            .expect("historical prefix");
            runner
                .run_bundle(&conn, &prefix)
                .expect("apply historical prefix");
        }
        let delta = runner.run_bundle(&conn, &full).expect("apply forward");
        let versions: Vec<i64> = delta.iter().map(|migration| migration.version).collect();
        let expected =
            ((prefix_len + 1) as i64..=full.migrations().len() as i64).collect::<Vec<_>>();
        assert_eq!(versions, expected, "H1/H2/H3 prefix={prefix_len}");
        assert!(
            runner
                .run_bundle(&conn, &full)
                .expect("repeat full")
                .is_empty(),
            "H4 prefix={prefix_len}"
        );
        for table in TABLES {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .expect("query sqlite_master");
            assert_eq!(count, 1, "H1/H2/H3 {table} prefix={prefix_len}");
        }
    }
}

#[test]
fn sqlite_failed_migration_rolls_back_schema_and_ledger() -> Result<(), Box<dyn std::error::Error>>
{
    // Failure-atomicity decision table: C1=a bundle contains an earlier valid
    // DDL statement; C2=a later statement fails; C3=startup retries with a
    // corrected declaration. R1(C1+C2)->neither user schema nor ledger commits;
    // R2(C3)->the complete bundle applies from version one. This validates the
    // product runner's BEGIN IMMEDIATE guard and rollback as one boundary.
    let connection = Connection::open_in_memory()?;
    let runner = SqliteMigrationRunner::with_prefix("failure_atomicity")?;
    let broken = failure_atomicity_bundle("THIS IS NOT SQL")?;
    assert!(runner.run_bundle(&connection, &broken).is_err());
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('failure_atomicity_value', 'failure_atomicity_schema_migrations', 'failure_atomicity_schema_migrations_meta')",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(table_count, 0, "R1: failed migration leaked durable state");

    let corrected = failure_atomicity_bundle("ALTER TABLE {prefix}_value ADD COLUMN value TEXT")?;
    let applied = runner.run_bundle(&connection, &corrected)?;
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
    assert_eq!(applied.len(), 9, "all nine migrations applied");

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
