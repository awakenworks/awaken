// @formal surface obligation=work.single_active_claim
// @formal surface obligation=work.exact_boundary_reclaim
// @formal surface obligation=work.lease_authority_snapshot_and_owner_mutations_are_atomic
// @formal reason: Owns construction, schema admission, idempotent row creation, and owned-row reads for
// @formal reason: the two durable WorkQueue adapters.
//! Backend construction and backend-neutral row ownership helpers.
//!
//! The [`WorkQueue`](awaken_session_work_contract::WorkQueue) command
//! implementations remain in the crate root. This module owns only connection
//! lifecycle, schema admission, idempotent row creation, and owned-row reads for
//! the two durable adapters.

use super::*;
use awaken_time::WallClockExt;
use sqlx::postgres::PgRow;
use sqlx::sqlite::{Sqlite, SqliteConnection};

/// Decode the selected PostgreSQL Work row through the same item constructor
/// used by the durable backend's other reads.
pub(super) fn pg_row_to_item(row: &PgRow) -> Result<WorkItem, WorkQueueError> {
    let metadata_json: String = row.get("metadata_json");
    build_item(
        row.get("work_id"),
        row.get("environment_id"),
        &row.get::<String, _>("data_type"),
        row.get("data_id"),
        &metadata_json,
        &row.get::<String, _>("state"),
        row.get("acknowledged_at"),
        row.get("latest_heartbeat_at"),
        row.get("started_at"),
        row.get("stop_requested_at"),
        row.get("stopped_at"),
        row.get("created_at"),
    )
    .map_err(storage)
}

impl SqliteWorkQueue {
    /// A private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_in_memory(clock: Arc<dyn WallClock>) -> Result<Self, String> {
        Self::from_location(awaken_sqlite_pool::EmbeddedSqlite::memory(), clock).await
    }

    /// Open (or create) the work-queue database at `path` and apply migrations.
    ///
    /// The wall clock is a constructor argument, not something the queue reaches
    /// for: every `*_at` this backend writes is a reading of exactly this clock,
    /// so a test can name the instant a record must carry.
    pub async fn open(path: &str, clock: Arc<dyn WallClock>) -> Result<Self, String> {
        Self::from_location(awaken_sqlite_pool::EmbeddedSqlite::file(path), clock).await
    }

    async fn from_location(
        location: awaken_sqlite_pool::EmbeddedSqlite,
        clock: Arc<dyn WallClock>,
    ) -> Result<Self, String> {
        let pool = location.open().await.map_err(|error| error.to_string())?;
        apply_sqlite_migrations(&pool).await?;
        Ok(Self {
            pool,
            book: LeaseBook::default(),
            clock,
        })
    }

    /// The store's one connection, held for a whole queue operation exactly as
    /// the former shared-connection scheduler held it.
    pub(super) async fn connection(
        &self,
    ) -> Result<awaken_sqlite_pool::EmbeddedConnection, WorkQueueError> {
        self.pool.acquire().await.map_err(storage)
    }

    /// Reclaim `env_id`'s `active` rows whose durable lease has lapsed.
    pub(super) async fn reclaim_lapsed(
        tx: &mut SqliteConnection,
        env_id: &str,
        now_ms: u64,
    ) -> Result<(), WorkQueueError> {
        let cleared = reclaimed_lease_authority();
        sqlx::query(
            "UPDATE work_queue_item \
             SET state = 'queued', lease_owner = ?1, lease_expires_ms = ?2, \
                 lease_refreshed_ms = ?3, latest_heartbeat_at = NULL, session_token_sha256 = NULL \
             WHERE environment_id = ?4 AND state = 'active' \
               AND (lease_expires_ms IS NULL OR lease_expires_ms <= ?5)",
        )
        .bind(cleared.owner)
        .bind(cleared.expires_at)
        .bind(cleared.refreshed_at)
        .bind(env_id)
        .bind(db_millis(now_ms))
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        Ok(())
    }

    /// Insert a queued row and return its work id. A Session uses its id as
    /// `data_id`; a healthcheck references its own generated work id.
    pub(super) async fn insert(
        conn: &mut SqliteConnection,
        environment_id: &str,
        data_type: &str,
        session_id: Option<&str>,
        created_at: String,
    ) -> Result<String, WorkQueueError> {
        let mut tx = immediate(conn).await?;
        if let Some(session_id) = session_id
            && let Some(existing) = sqlx::query_scalar::<_, String>(
                "SELECT work_id FROM work_queue_item \
                 WHERE environment_id = ?1 AND data_type = 'session' AND data_id = ?2 \
                 ORDER BY seq ASC LIMIT 1",
            )
            .bind(environment_id)
            .bind(session_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage)?
        {
            tx.commit().await.map_err(storage)?;
            return Ok(existing);
        }
        let next: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(seq), -1) + 1 FROM work_queue_item")
                .fetch_one(&mut *tx)
                .await
                .map_err(storage)?;
        let work_id = format!("work_{next:016}");
        let data_id = session_id.unwrap_or(&work_id);
        sqlx::query(
            "INSERT INTO work_queue_item \
                (work_id, seq, environment_id, data_type, data_id, metadata_json, state, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, '{}', 'queued', ?6)",
        )
        .bind(&work_id)
        .bind(next)
        .bind(environment_id)
        .bind(data_type)
        .bind(data_id)
        .bind(created_at)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(work_id)
    }

    pub(super) async fn owned(
        tx: &mut SqliteConnection,
        env_id: &str,
        wid: &str,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        sqlx::query(&format!(
            "SELECT {COLS} FROM work_queue_item WHERE work_id = ?1 AND environment_id = ?2"
        ))
        .bind(wid)
        .bind(env_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| row_to_item(&row).map_err(storage))
        .transpose()
    }
}

/// Open the write transaction every SQLite queue mutation runs in: SQLite's
/// `BEGIN IMMEDIATE` takes the database write lock before the first read.
pub(super) async fn immediate(
    conn: &mut SqliteConnection,
) -> Result<sqlx::Transaction<'_, Sqlite>, WorkQueueError> {
    sqlx::Connection::begin_with(conn, "BEGIN IMMEDIATE")
        .await
        .map_err(storage)
}

impl PostgresWorkQueue {
    /// Connect and apply the work-queue migrations under the `work_queue` namespace.
    pub async fn connect(url: &str, clock: Arc<dyn WallClock>) -> Result<Self, String> {
        let pool = awaken_sql_storage::postgres_pool_options()
            .connect(url)
            .await
            .map_err(|error| error.to_string())?;
        Self::with_pool(pool, clock).await
    }

    /// Build from an existing pool and apply the work-queue migrations.
    pub async fn with_pool(pool: PgPool, clock: Arc<dyn WallClock>) -> Result<Self, String> {
        apply_postgres_migrations(&pool).await?;
        Ok(Self {
            pool,
            book: LeaseBook::default(),
            clock,
        })
    }

    /// Connect only when the published schema is already present and exact.
    pub async fn connect_existing(url: &str, clock: Arc<dyn WallClock>) -> Result<Self, String> {
        let pool = awaken_sql_storage::postgres_pool_options()
            .connect(url)
            .await
            .map_err(|error| error.to_string())?;
        Self::with_existing_pool(pool, clock).await
    }

    /// Verify the existing scoped ledger over the composition-owned pool.
    pub async fn with_existing_pool(
        pool: PgPool,
        clock: Arc<dyn WallClock>,
    ) -> Result<Self, String> {
        verify_postgres_migrations(&pool).await?;
        Ok(Self {
            pool,
            book: LeaseBook::default(),
            clock,
        })
    }

    pub(super) async fn insert(
        &self,
        env_id: &str,
        data_type: &str,
        session_id: Option<&str>,
    ) -> Result<String, WorkQueueError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        sqlx::query("LOCK TABLE work_queue_item IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        if let Some(session_id) = session_id
            && let Some(existing) = sqlx::query_scalar::<_, String>(
                "SELECT work_id FROM work_queue_item \
                 WHERE environment_id = $1 AND data_type = 'session' AND data_id = $2 \
                 ORDER BY seq ASC LIMIT 1",
            )
            .bind(env_id)
            .bind(session_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage)?
        {
            tx.commit().await.map_err(storage)?;
            return Ok(existing);
        }
        let next: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(seq), -1) + 1 FROM work_queue_item")
                .fetch_one(&mut *tx)
                .await
                .map_err(storage)?;
        let work_id = format!("work_{next:016}");
        let data_id = session_id.unwrap_or(&work_id).to_string();
        sqlx::query(
            "INSERT INTO work_queue_item \
                (work_id, seq, environment_id, data_type, data_id, metadata_json, state, created_at) \
             VALUES ($1, $2, $3, $4, $5, '{}', 'queued', $6)",
        )
        .bind(&work_id)
        .bind(next)
        .bind(env_id)
        .bind(data_type)
        .bind(&data_id)
        .bind(self.clock.observed_at())
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(work_id)
    }

    pub(super) async fn fetch_owned(
        &self,
        env_id: &str,
        wid: &str,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        sqlx::query(&format!(
            "SELECT {COLS} FROM work_queue_item WHERE work_id = $1 AND environment_id = $2"
        ))
        .bind(wid)
        .bind(env_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?
        .map(|row| pg_row_to_item(&row))
        .transpose()
    }
}
