//! Portable work-queue schema, row codec, and lease-time normalization shared
//! by the SQLite and PostgreSQL adapters.

use std::collections::BTreeMap;

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};
use awaken_session_contract::work_queue::{
    WorkItem, WorkPayload, WorkState, next_heartbeat_receipt,
};
use rusqlite::{Connection, OptionalExtension};
use sqlx::postgres::PgPool;

/// Frozen presence timestamp used by the open-tier Managed projection.
pub(super) const OBJECT_AT: &str = "2026-01-01T00:00:00Z";
pub(super) const HEARTBEAT_TTL_SECONDS: u64 = 60;
pub(super) const NS: &str = "work_queue";
pub(super) const BUNDLE_ID: &str = "awaken.work_queue";
const CONVERGED_BUNDLE_ID: &str = "awaken.work_queue.converged";
const LEGACY_V1_CHECKSUM: &str = "e3ce97e39ea1c9e5b34a851c47ce237c4168778f8665342a116bde35f364c2f0";
const PUBLISHED_LEGACY_MIGRATION_COUNT: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PublishedWorkStream {
    Current,
    Legacy,
}

pub(super) fn work_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        BUNDLE_ID,
        vec![Migration::new(
            1,
            "current self-hosted work queue authority",
            "CREATE TABLE {prefix}_item (\
             work_id             TEXT PRIMARY KEY, \
             seq                 BIGINT NOT NULL CHECK (seq >= 0), \
             environment_id      TEXT NOT NULL CHECK (length(environment_id) > 0), \
             data_type           TEXT NOT NULL CHECK (data_type IN ('healthcheck', 'session')), \
             data_id             TEXT NOT NULL CHECK (length(data_id) > 0), \
             metadata_json       TEXT NOT NULL, \
             state               TEXT NOT NULL CHECK (state IN ('queued', 'starting', 'active', 'stopping', 'stopped')), \
             acknowledged_at     TEXT, \
             latest_heartbeat_at TEXT, \
             started_at          TEXT, \
             stop_requested_at   TEXT, \
             stopped_at          TEXT, \
             lease_owner         TEXT, \
             lease_epoch         BIGINT NOT NULL DEFAULT 0 CHECK (lease_epoch >= 0), \
             lease_expires_ms    BIGINT CHECK (lease_expires_ms IS NULL OR lease_expires_ms >= 0), \
             lease_refreshed_ms  BIGINT CHECK (lease_refreshed_ms IS NULL OR lease_refreshed_ms >= 0), \
             session_token_sha256 TEXT); \
             CREATE UNIQUE INDEX {prefix}_session_projection_unique \
             ON {prefix}_item (environment_id, data_type, data_id)",
        )?],
    )
}

fn legacy_work_bundle() -> Result<MigrationBundle, MigrationError> {
    const SQLITE_UPGRADE: &str = "ALTER TABLE {prefix}_item RENAME TO {prefix}_item_legacy; \
        CREATE TABLE {prefix}_item (\
            work_id TEXT PRIMARY KEY, \
            seq BIGINT NOT NULL CHECK (seq >= 0), \
            environment_id TEXT NOT NULL CHECK (length(environment_id) > 0), \
            data_type TEXT NOT NULL CHECK (data_type IN ('healthcheck', 'session')), \
            data_id TEXT NOT NULL CHECK (length(data_id) > 0), \
            metadata_json TEXT NOT NULL, \
            state TEXT NOT NULL CHECK (state IN ('queued', 'starting', 'active', 'stopping', 'stopped')), \
            acknowledged_at TEXT, latest_heartbeat_at TEXT, started_at TEXT, \
            stop_requested_at TEXT, stopped_at TEXT, lease_owner TEXT, \
            lease_epoch BIGINT NOT NULL DEFAULT 0 CHECK (lease_epoch >= 0), \
            lease_expires_ms BIGINT CHECK (lease_expires_ms IS NULL OR lease_expires_ms >= 0), \
            lease_refreshed_ms BIGINT CHECK (lease_refreshed_ms IS NULL OR lease_refreshed_ms >= 0), \
            session_token_sha256 TEXT); \
        INSERT INTO {prefix}_item SELECT * FROM {prefix}_item_legacy; \
        DROP TABLE {prefix}_item_legacy; \
        CREATE UNIQUE INDEX {prefix}_session_projection_unique \
            ON {prefix}_item (environment_id, data_type, data_id)";
    const POSTGRES_UPGRADE: &str = "ALTER TABLE {prefix}_item \
        ADD CONSTRAINT {prefix}_seq_nonnegative CHECK (seq >= 0), \
        ADD CONSTRAINT {prefix}_environment_nonempty CHECK (length(environment_id) > 0), \
        ADD CONSTRAINT {prefix}_data_type_known CHECK (data_type IN ('healthcheck', 'session')), \
        ADD CONSTRAINT {prefix}_data_id_nonempty CHECK (length(data_id) > 0), \
        ADD CONSTRAINT {prefix}_state_known CHECK (state IN ('queued', 'starting', 'active', 'stopping', 'stopped')), \
        ADD CONSTRAINT {prefix}_lease_epoch_nonnegative CHECK (lease_epoch >= 0), \
        ADD CONSTRAINT {prefix}_lease_expiry_nonnegative CHECK (lease_expires_ms IS NULL OR lease_expires_ms >= 0), \
        ADD CONSTRAINT {prefix}_lease_refresh_nonnegative CHECK (lease_refreshed_ms IS NULL OR lease_refreshed_ms >= 0)";
    let published = [
        (
            1,
            "self-hosted environment work queue: one row per work item",
            include_str!("migrations/expanded/V0001__work_item.sql").trim(),
            LEGACY_V1_CHECKSUM,
        ),
        (
            2,
            "persist work ownership, fencing epoch, and lease expiry",
            include_str!("migrations/expanded/V0002__lease_authority.sql").trim(),
            "1e673b1593b8f9ce63218217f78525068aaa515f7838faef9e1da8a810fe7a0f",
        ),
        (
            3,
            "persist the lease refresh clock independently of its requested ttl",
            include_str!("migrations/expanded/V0003__lease_refresh.sql").trim(),
            "9cc9aad143bd7734d4850fbdf72d02f2a5c875c9e068e6be5ee5a26372e8e325",
        ),
        (
            4,
            "one canonical work projection per Environment Session",
            include_str!("migrations/expanded/V0004__session_projection.sql").trim(),
            "e05e2a3dbf6e85329519de8271091d6770a5576a30f6a5631d94b252f6125c4b",
        ),
        (
            5,
            "bind a digest-only per-Session bearer to the current Work lease",
            include_str!("migrations/expanded/V0005__session_token.sql").trim(),
            "8dab9735b5dcd55c9cc6adbfc55522d66d813b18bdd3cdcf434c33792480c37e",
        ),
    ];
    assert_eq!(published.len(), PUBLISHED_LEGACY_MIGRATION_COUNT);
    let mut migrations = published
        .into_iter()
        .map(|(version, description, sql, checksum)| {
            Migration::published_legacy(version, description, sql, checksum)
        })
        .collect::<Result<Vec<_>, _>>()?;
    migrations.push(Migration::per_dialect(
        6,
        "converge the published Work queue into one constrained authority",
        POSTGRES_UPGRADE,
        SQLITE_UPGRADE,
    )?);
    MigrationBundle::new(BUNDLE_ID, migrations)
}

pub(super) fn selected_work_bundle(
    v1_checksum: Option<&str>,
) -> Result<(PublishedWorkStream, MigrationBundle), MigrationError> {
    if v1_checksum == Some(LEGACY_V1_CHECKSUM) {
        Ok((PublishedWorkStream::Legacy, legacy_work_bundle()?))
    } else {
        Ok((PublishedWorkStream::Current, work_bundle()?))
    }
}

pub(super) fn converged_work_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        CONVERGED_BUNDLE_ID,
        vec![Migration::new(
            1,
            "seal the converged Work queue migration history",
            "SELECT 1",
        )?],
    )
}

pub(super) fn apply_sqlite_migrations(conn: &Connection) -> Result<(), String> {
    let ledger_exists = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [format!("{NS}_schema_migrations")],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| error.to_string())?;
    let v1_checksum = if ledger_exists {
        conn.query_row(
            &format!(
                "SELECT checksum FROM {NS}_schema_migrations WHERE bundle_id=?1 AND version=1"
            ),
            [BUNDLE_ID],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| error.to_string())?
    } else {
        None
    };
    let (_, published) =
        selected_work_bundle(v1_checksum.as_deref()).map_err(|error| error.to_string())?;
    let converged = converged_work_bundle().map_err(|error| error.to_string())?;
    let runner = awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
        .map_err(|error| error.to_string())?;
    runner
        .run_bundle(conn, &published)
        .and_then(|_| runner.run_bundle(conn, &converged))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

async fn selected_postgres_bundles(
    pool: &PgPool,
) -> Result<(MigrationBundle, MigrationBundle), String> {
    let ledger: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
        .bind(format!("{NS}_schema_migrations"))
        .fetch_one(pool)
        .await
        .map_err(|error| error.to_string())?;
    let v1_checksum: Option<String> = if ledger.is_some() {
        sqlx::query_scalar(&format!(
            "SELECT checksum FROM {NS}_schema_migrations WHERE bundle_id=$1 AND version=1"
        ))
        .bind(BUNDLE_ID)
        .fetch_optional(pool)
        .await
        .map_err(|error| error.to_string())?
    } else {
        None
    };
    Ok((
        selected_work_bundle(v1_checksum.as_deref())
            .map_err(|error| error.to_string())?
            .1,
        converged_work_bundle().map_err(|error| error.to_string())?,
    ))
}

pub(super) async fn apply_postgres_migrations(pool: &PgPool) -> Result<(), String> {
    let (published, converged) = selected_postgres_bundles(pool).await?;
    let runner =
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|error| error.to_string())?;
    runner
        .run_bundle(&published)
        .await
        .map_err(|error| error.to_string())?;
    runner
        .run_bundle(&converged)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

pub(super) async fn verify_postgres_migrations(pool: &PgPool) -> Result<(), String> {
    let (published, converged) = selected_postgres_bundles(pool).await?;
    let runner =
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|error| error.to_string())?;
    runner
        .verify_bundle(&published)
        .await
        .map_err(|error| error.to_string())?;
    runner
        .verify_bundle(&converged)
        .await
        .map_err(|error| error.to_string())
}

fn state_from_wire(state: &str) -> Result<WorkState, String> {
    WorkState::from_wire(state).ok_or_else(|| format!("unknown persisted work state `{state}`"))
}

fn data_of(data_type: &str, data_id: String) -> Result<WorkPayload, String> {
    match data_type {
        "healthcheck" => Ok(WorkPayload::HealthCheck { id: data_id }),
        "session" => Ok(WorkPayload::Session { id: data_id }),
        _ => Err(format!("unknown persisted work payload type `{data_type}`")),
    }
}

pub(super) const COLS: &str = "work_id, environment_id, data_type, data_id, metadata_json, state, \
     acknowledged_at, latest_heartbeat_at, started_at, stop_requested_at, stopped_at";

pub(super) fn metadata_str(metadata: &BTreeMap<String, String>) -> String {
    serde_json::to_string(metadata).expect("work metadata serializes")
}

pub(super) fn db_millis(now_ms: u64) -> i64 {
    i64::try_from(now_ms).unwrap_or(i64::MAX)
}

pub(super) fn effective_ttl_seconds(desired: Option<u64>) -> u64 {
    desired.unwrap_or(HEARTBEAT_TTL_SECONDS).max(1)
}

pub(super) fn lease_expiry(now_ms: u64, ttl_seconds: u64) -> i64 {
    db_millis(now_ms.saturating_add(ttl_seconds.saturating_mul(1000)))
}

/// Produce the monotonic RFC-3339 compare token returned by the Managed API.
pub(crate) fn heartbeat_at(now_ms: u64, previous: Option<&str>) -> String {
    next_heartbeat_receipt(now_ms, previous)
}

pub(super) fn ack_next_state(current: &WorkItem) -> &'static str {
    current.state.after_ack().as_str()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_item(
    id: String,
    environment_id: String,
    data_type: &str,
    data_id: String,
    metadata_json: &str,
    state: &str,
    acknowledged_at: Option<String>,
    latest_heartbeat_at: Option<String>,
    started_at: Option<String>,
    stop_requested_at: Option<String>,
    stopped_at: Option<String>,
) -> Result<WorkItem, String> {
    let metadata = serde_json::from_str(metadata_json)
        .map_err(|error| format!("invalid persisted work metadata: {error}"))?;
    Ok(WorkItem {
        id,
        environment_id,
        data: data_of(data_type, data_id)?,
        metadata,
        state: state_from_wire(state)?,
        acknowledged_at,
        latest_heartbeat_at,
        started_at,
        stop_requested_at,
        stopped_at,
    })
}

pub(super) fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkItem> {
    let metadata_json: String = row.get(4)?;
    build_item(
        row.get(0)?,
        row.get(1)?,
        &row.get::<_, String>(2)?,
        row.get(3)?,
        &metadata_json,
        &row.get::<_, String>(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    )
    .map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use awaken_scoped_migration::{Dialect, MigrationBundle, MigrationError, plan};
    use awaken_scoped_migration_sqlite::SqliteMigrationRunner;
    use rusqlite::Connection;

    use super::*;

    fn legacy_v3() -> MigrationBundle {
        let legacy = legacy_work_bundle().expect("legacy bundle");
        MigrationBundle::new(BUNDLE_ID, legacy.migrations()[..3].to_vec())
            .expect("published V1..V3")
    }

    #[test]
    fn published_work_histories_select_exactly_and_converge_once() {
        // Causes: H1 no ledger; H2 current V1; H3 published legacy V1; H4
        // unknown V1. Effects: E1 current bundle; E2 legacy V1..V6; E3 exact
        // checksum rejection; E4 one shared future append stream.
        // Rules: H1|H2=>E1+E4; H3=>E2+E4; H4=>E3.
        let current = work_bundle().expect("current");
        let current_checksum = current.migrations()[0].checksum_for(Dialect::Sqlite);
        assert_eq!(
            selected_work_bundle(None).expect("H1").0,
            PublishedWorkStream::Current,
            "H1"
        );
        assert_eq!(
            selected_work_bundle(Some(&current_checksum)).expect("H2").0,
            PublishedWorkStream::Current,
            "H2"
        );
        let (stream, legacy) = selected_work_bundle(Some(LEGACY_V1_CHECKSUM)).expect("H3");
        assert_eq!(stream, PublishedWorkStream::Legacy, "H3");
        assert_eq!(legacy.migrations().last().unwrap().version(), 6, "E2");
        assert_eq!(
            legacy.migrations()[0].checksum_for(Dialect::Sqlite),
            LEGACY_V1_CHECKSUM,
            "published production receipt"
        );
        let unknown = BTreeMap::from([(1, "f".repeat(64))]);
        assert!(matches!(
            plan(
                &selected_work_bundle(Some(&"f".repeat(64)))
                    .expect("H4 selection")
                    .1,
                &unknown,
                Dialect::Sqlite,
            ),
            Err(MigrationError::ChecksumMismatch { version: 1, .. })
        ));
        awaken_scoped_migration::lint(std::slice::from_ref(&converged_work_bundle().expect("E4")))
            .expect("E4");
    }

    #[test]
    fn sqlite_published_v3_converges_rows_and_constraints_atomically() {
        // Causes: M1 exact published V3; M2 duplicate Session projection; M3
        // valid durable fields. Effects: E1 earliest projection retained; E2
        // V6 and convergence receipts; E3 current CHECK constraints reject bad
        // writes. Rule W1=M1+M2+M3=>E1+E2+E3.
        let conn = Connection::open_in_memory().expect("sqlite");
        let runner = SqliteMigrationRunner::with_prefix(NS).expect("runner");
        runner.run_bundle(&conn, &legacy_v3()).expect("M1");
        for (id, seq) in [("oldest", 0), ("duplicate", 1)] {
            conn.execute(
                "INSERT INTO work_queue_item \
                 (work_id,seq,environment_id,data_type,data_id,metadata_json,state) \
                 VALUES (?1,?2,'env','session','session-a','{}','queued')",
                (id, seq),
            )
            .expect("M2 seed");
        }
        let legacy = selected_work_bundle(Some(LEGACY_V1_CHECKSUM))
            .expect("legacy")
            .1;
        runner.run_bundle(&conn, &legacy).expect("V6");
        runner
            .run_bundle(&conn, &converged_work_bundle().expect("converged"))
            .expect("convergence");
        assert_eq!(
            conn.query_row("SELECT work_id FROM work_queue_item", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "oldest",
            "E1"
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM work_queue_schema_migrations \
                 WHERE (bundle_id=?1 AND version=6) OR bundle_id=?2",
                (BUNDLE_ID, CONVERGED_BUNDLE_ID),
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            2,
            "E2"
        );
        assert!(
            conn.execute(
                "INSERT INTO work_queue_item \
                 (work_id,seq,environment_id,data_type,data_id,metadata_json,state,lease_epoch) \
                 VALUES ('bad',-1,'env','session','bad','{}','queued',0)",
                [],
            )
            .is_err(),
            "E3"
        );
    }

    #[test]
    fn sqlite_invalid_legacy_row_keeps_the_v6_cutover_uncommitted() {
        // Causes: F1 exact legacy V3; F2 a negative durable sequence. Effects:
        // E1 V6 rejects; E2 the row and pre-cutover table remain readable; E3
        // no convergence receipt. Rule W2=F1+F2=>E1+E2+E3.
        let conn = Connection::open_in_memory().expect("sqlite");
        let runner = SqliteMigrationRunner::with_prefix(NS).expect("runner");
        runner.run_bundle(&conn, &legacy_v3()).expect("F1");
        conn.execute(
            "INSERT INTO work_queue_item \
             (work_id,seq,environment_id,data_type,data_id,metadata_json,state) \
             VALUES ('bad',-1,'env','session','bad','{}','queued')",
            [],
        )
        .expect("F2");
        let legacy = legacy_work_bundle().expect("legacy");
        assert!(runner.run_bundle(&conn, &legacy).is_err(), "E1");
        assert_eq!(
            conn.query_row(
                "SELECT seq FROM work_queue_item WHERE work_id='bad'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            -1,
            "E2"
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM work_queue_schema_migrations \
                 WHERE bundle_id=?1 AND version=6",
                [BUNDLE_ID],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0,
            "E3"
        );
    }

    #[tokio::test]
    async fn postgres_published_v3_converges_with_the_same_domain_effects() {
        use sqlx::Executor;
        use sqlx::postgres::{PgPool, PgPoolOptions};

        // Same W1 causes/effects as SQLite: exact V3 + duplicate valid Session
        // projections => earliest row, V6+convergence, and enforced constraints.
        // Backend DDL differs; the Work identity and terminal effects do not.
        let url = std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".into()
        });
        let Ok(admin) = PgPool::connect(&url).await else {
            println!("[skip] no Postgres reachable");
            return;
        };
        admin
            .execute("DROP SCHEMA IF EXISTS t_work_queue_migration CASCADE")
            .await
            .expect("drop test schema");
        admin
            .execute("CREATE SCHEMA t_work_queue_migration")
            .await
            .expect("create test schema");
        admin.close().await;
        let pool = PgPoolOptions::new()
            .after_connect(|connection, _| {
                Box::pin(async move {
                    connection
                        .execute("SET search_path=t_work_queue_migration")
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .expect("test schema pool");
        let runner = awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
            pool.clone(),
            NS,
        )
        .expect("runner");
        runner.run_bundle(&legacy_v3()).await.expect("published V3");
        for (id, seq) in [("oldest", 0_i64), ("duplicate", 1_i64)] {
            sqlx::query(
                "INSERT INTO work_queue_item \
                 (work_id,seq,environment_id,data_type,data_id,metadata_json,state) \
                 VALUES ($1,$2,'env','session','session-a','{}','queued')",
            )
            .bind(id)
            .bind(seq)
            .execute(&pool)
            .await
            .expect("legacy row");
        }
        runner
            .run_bundle(&legacy_work_bundle().expect("legacy"))
            .await
            .expect("V6");
        runner
            .run_bundle(&converged_work_bundle().expect("converged"))
            .await
            .expect("convergence");
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT work_id FROM work_queue_item")
                .fetch_one(&pool)
                .await
                .unwrap(),
            "oldest",
            "W1/E1"
        );
        assert!(
            sqlx::query(
                "INSERT INTO work_queue_item \
                 (work_id,seq,environment_id,data_type,data_id,metadata_json,state,lease_epoch) \
                 VALUES ('bad',-1,'env','session','bad','{}','queued',0)",
            )
            .execute(&pool)
            .await
            .is_err(),
            "W1/E3"
        );
        pool.close().await;
    }
}
