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

/// The six committed-thread tables, prefixed with the runtime namespace.
const TABLES: [&str; 6] = [
    "runtime_commit",
    "runtime_message",
    "runtime_state_command",
    "runtime_event",
    "runtime_run_record",
    "runtime_waiting",
];

// The bundle applies against a real (embedded) SQLite database: all six tables
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
    assert_eq!(applied.len(), 6, "all six migrations applied");

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

// Forward migration: applying only v1 first, then the full bundle, applies exactly
// the v2..=6 delta (the ledger skips the already-applied v1). This proves a store
// opened at an older schema version migrates forward to the current one, applying
// only the new specs — the real upgrade path.
#[test]
fn bundle_migrates_forward_v1_to_full() {
    let conn = Connection::open_in_memory().expect("open sqlite");
    let full = commit_bundle().expect("bundle builds");

    // A partial bundle carrying only migration v1 (cloned from the shared bundle,
    // so it is byte-identical — same version, description, and checksum).
    let v1_only = awaken_scoped_migration::MigrationBundle::new(
        COMMIT_BUNDLE_ID,
        vec![full.migrations()[0].clone()],
    )
    .expect("v1-only bundle");

    let runner = SqliteMigrationRunner::with_prefix(NS).expect("runner");
    let first = runner.run_bundle(&conn, &v1_only).expect("apply v1");
    assert_eq!(first.len(), 1, "only v1 applied on the first pass");
    assert_eq!(first[0].version, 1);

    // Re-run with the full bundle: v1 is already recorded, so only v2..=6 apply.
    let delta = runner.run_bundle(&conn, &full).expect("apply forward");
    let versions: Vec<i64> = delta.iter().map(|m| m.version).collect();
    assert_eq!(
        versions,
        vec![2, 3, 4, 5, 6],
        "forward migration applied exactly the v2..=6 delta"
    );

    // All six tables now exist.
    for table in TABLES {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )
            .expect("query sqlite_master");
        assert_eq!(count, 1, "table {table} exists after forward migration");
    }
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
    assert_eq!(applied.len(), 6, "all six migrations applied");

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

    pool.close().await;
}
