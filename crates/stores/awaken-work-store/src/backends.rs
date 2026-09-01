//! Backend construction and backend-neutral row ownership helpers.
//!
//! The [`WorkQueue`](awaken_session_contract::work_queue::WorkQueue) command
//! implementations remain in the crate root. This module owns only connection
//! lifecycle, schema admission, idempotent row creation, and owned-row reads for
//! the two durable adapters.

use super::*;

impl SqliteWorkQueue {
    /// Open (or create) the work-queue database at `path` and apply migrations.
    pub fn open(path: &str) -> Result<Self, String> {
        Self::from_connection(
            awaken_sqlite_runtime::SqliteConnectionFactory::file(path)
                .open()
                .map_err(|error| error.to_string())?,
        )
    }

    pub(super) fn from_connection(conn: Connection) -> Result<Self, String> {
        apply_sqlite_migrations(&conn)?;
        Ok(Self {
            conn: SharedSqliteConnection::new(conn),
            book: LeaseBook::default(),
        })
    }

    pub(super) async fn with_connection<T, F>(&self, operation: F) -> Result<T, WorkQueueError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, WorkQueueError> + Send + 'static,
    {
        awaken_sqlite_runtime::with_connection(self.conn.clone(), operation)
            .await
            .map_err(storage)?
    }

    /// Reclaim `env_id`'s `active` rows whose durable lease has lapsed.
    pub(super) fn reclaim_lapsed(
        tx: &Transaction<'_>,
        env_id: &str,
        now_ms: u64,
    ) -> Result<(), WorkQueueError> {
        tx.execute(
            "UPDATE work_queue_item \
             SET state = 'queued', lease_owner = NULL, lease_expires_ms = NULL, \
                 lease_refreshed_ms = NULL, latest_heartbeat_at = NULL, session_token_sha256 = NULL \
             WHERE environment_id = ?1 AND state = 'active' \
               AND (lease_expires_ms IS NULL OR lease_expires_ms <= ?2)",
            params![env_id, db_millis(now_ms)],
        )
        .map_err(storage)?;
        Ok(())
    }

    /// Insert a queued row and return its work id. A Session uses its id as
    /// `data_id`; a healthcheck references its own generated work id.
    pub(super) fn insert(
        conn: &mut Connection,
        environment_id: &str,
        data_type: &str,
        session_id: Option<&str>,
    ) -> Result<String, WorkQueueError> {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        if let Some(session_id) = session_id
            && let Some(existing) = tx
                .query_row(
                    "SELECT work_id FROM work_queue_item \
                     WHERE environment_id = ?1 AND data_type = 'session' AND data_id = ?2 \
                     ORDER BY seq ASC LIMIT 1",
                    params![environment_id, session_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage)?
        {
            tx.commit().map_err(storage)?;
            return Ok(existing);
        }
        let next: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), -1) + 1 FROM work_queue_item",
                [],
                |row| row.get(0),
            )
            .map_err(storage)?;
        let work_id = format!("work_{next:016}");
        let data_id = session_id.unwrap_or(&work_id);
        tx.execute(
            "INSERT INTO work_queue_item \
                (work_id, seq, environment_id, data_type, data_id, metadata_json, state) \
             VALUES (?1, ?2, ?3, ?4, ?5, '{}', 'queued')",
            params![work_id, next, environment_id, data_type, data_id],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(work_id)
    }

    pub(super) fn owned(
        tx: &Transaction<'_>,
        env_id: &str,
        wid: &str,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        tx.query_row(
            &format!(
                "SELECT {COLS} FROM work_queue_item WHERE work_id = ?1 AND environment_id = ?2"
            ),
            params![wid, env_id],
            row_to_item,
        )
        .optional()
        .map_err(storage)
    }
}

impl PostgresWorkQueue {
    /// Connect and apply the work-queue migrations under the `work_queue` namespace.
    pub async fn connect(url: &str) -> Result<Self, String> {
        let pool = PgPool::connect(url)
            .await
            .map_err(|error| error.to_string())?;
        Self::with_pool(pool).await
    }

    /// Build from an existing pool and apply the work-queue migrations.
    pub async fn with_pool(pool: PgPool) -> Result<Self, String> {
        apply_postgres_migrations(&pool).await?;
        Ok(Self {
            pool,
            book: LeaseBook::default(),
        })
    }

    /// Connect only when the published schema is already present and exact.
    pub async fn connect_existing(url: &str) -> Result<Self, String> {
        let pool = PgPool::connect(url)
            .await
            .map_err(|error| error.to_string())?;
        verify_postgres_migrations(&pool).await?;
        Ok(Self {
            pool,
            book: LeaseBook::default(),
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
                (work_id, seq, environment_id, data_type, data_id, metadata_json, state) \
             VALUES ($1, $2, $3, $4, $5, '{}', 'queued')",
        )
        .bind(&work_id)
        .bind(next)
        .bind(env_id)
        .bind(data_type)
        .bind(&data_id)
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
