//! Durable [`WorkQueue`] backend: the self-hosted environment work queue over a
//! store, so a session dispatched to a self-hosted environment survives a restart
//! and can be claimed by a worker on any node. SQLite (embedded, single machine)
//! lands here; the Postgres backend (distributed) shares the one portable bundle,
//! exactly like the session/config/catalog stores.
//!
//! Its own scope (`work_queue_item` table + `work_queue_schema_migrations` ledger),
//! a distinct aggregate from the managed session config. The lease is the store's
//! own transaction: SQLite claims under a `BEGIN IMMEDIATE` write lock, so a run is
//! owned by one worker at a time without `FOR UPDATE SKIP LOCKED` (Postgres uses
//! that in its backend). No secret is minted; `secret` stays `null` on the wire.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};
use awaken_session_contract::work_queue::{
    LeaseBook, LeaseReceipt, QueueStats, WorkItem, WorkPayload, WorkQueue, WorkState,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sqlx::Row;
use sqlx::postgres::{PgPool, PgRow};

/// The frozen presence timestamp the managed wire uses (parity with the in-memory
/// queue — timestamps carry presence, not wall time, in the open-tier projection).
const OBJECT_AT: &str = "2026-01-01T00:00:00Z";
/// The lease TTL a heartbeat reports.
const HEARTBEAT_TTL_SECONDS: u64 = 60;
/// The store's table namespace / bundle prefix (`work_queue_item`,
/// `work_queue_schema_migrations`).
const NS: &str = "work_queue";

/// The versioned schema bundle. One migration: the work item row. Columns are
/// portable — the two adapters read/write identical rows, so SQLite and Postgres
/// stay at parity.
fn work_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        "awaken.work_queue",
        vec![Migration::new(
            1,
            "self-hosted environment work queue: one row per work item",
            "CREATE TABLE {prefix}_item (\
             work_id             TEXT PRIMARY KEY, \
             seq                 BIGINT NOT NULL, \
             environment_id      TEXT NOT NULL, \
             data_type           TEXT NOT NULL, \
             data_id             TEXT NOT NULL, \
             metadata_json       TEXT NOT NULL, \
             state               TEXT NOT NULL, \
             acknowledged_at     TEXT, \
             latest_heartbeat_at TEXT, \
             started_at          TEXT, \
             stop_requested_at   TEXT, \
             stopped_at          TEXT)",
        )?],
    )
}

fn state_from_wire(s: &str) -> WorkState {
    match s {
        "starting" => WorkState::Starting,
        "active" => WorkState::Active,
        "stopping" => WorkState::Stopping,
        "stopped" => WorkState::Stopped,
        _ => WorkState::Queued,
    }
}

fn data_of(data_type: &str, data_id: String) -> WorkPayload {
    match data_type {
        "healthcheck" => WorkPayload::HealthCheck { id: data_id },
        _ => WorkPayload::Session { id: data_id },
    }
}

/// The columns a work row projects to a [`WorkItem`], in `SELECT` order.
const COLS: &str = "work_id, environment_id, data_type, data_id, metadata_json, state, \
     acknowledged_at, latest_heartbeat_at, started_at, stop_requested_at, stopped_at";

fn metadata_str(m: &BTreeMap<String, String>) -> String {
    serde_json::to_string(m).expect("work metadata serializes")
}

/// Ack stamps receipt: `queued` → `starting`; any other state is unchanged.
fn ack_next_state(current: &WorkItem) -> &'static str {
    if current.state == WorkState::Queued {
        "starting"
    } else {
        current.state.as_str()
    }
}

/// Assemble a [`WorkItem`] from already-extracted scalars — the one place the
/// row shape is decoded, so the two adapters (rusqlite/sqlx) can't drift.
#[allow(clippy::too_many_arguments)]
fn build_item(
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
) -> WorkItem {
    WorkItem {
        id,
        environment_id,
        data: data_of(data_type, data_id),
        metadata: serde_json::from_str(metadata_json).unwrap_or_default(),
        state: state_from_wire(state),
        acknowledged_at,
        latest_heartbeat_at,
        started_at,
        stop_requested_at,
        stopped_at,
    }
}

fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkItem> {
    let metadata_json: String = row.get(4)?;
    Ok(build_item(
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
    ))
}

/// SQLite persistence for the environment work queue. The durable rows hold the
/// work items; lease expiry + poll liveness are ephemeral (meaningless after a
/// restart) so they live in the in-process [`LeaseBook`] shared with every backend.
pub struct SqliteWorkQueue {
    conn: Arc<Mutex<Connection>>,
    book: LeaseBook,
}

impl SqliteWorkQueue {
    /// Open (or create) the work-queue database at `path` and apply migrations.
    pub fn open(path: &str) -> Result<Self, String> {
        Self::from_connection(Connection::open(path).map_err(|e| e.to_string())?)
    }

    /// An in-memory database (tests).
    pub fn open_in_memory() -> Result<Self, String> {
        Self::from_connection(Connection::open_in_memory().map_err(|e| e.to_string())?)
    }

    fn from_connection(conn: Connection) -> Result<Self, String> {
        let bundle = work_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|e| e.to_string())?
            .run_bundle(&conn, &bundle)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            book: LeaseBook::default(),
        })
    }

    /// Reclaim `env_id`'s `active` rows whose lease has lapsed (worker gone) back to
    /// `queued`, inside `tx`, so the next claim can re-lease them. Returns nothing;
    /// the caller re-counts active afterward.
    fn reclaim_lapsed(&self, tx: &Transaction<'_>, env_id: &str, now_ms: u64) {
        let mut stmt = tx
            .prepare(
                "SELECT work_id FROM work_queue_item WHERE environment_id = ?1 AND state = 'active'",
            )
            .expect("prepare active");
        let active_ids: Vec<String> = stmt
            .query_map(params![env_id], |r| r.get::<_, String>(0))
            .expect("query active")
            .map(|r| r.expect("row"))
            .collect();
        drop(stmt);
        for wid in active_ids {
            if !self.book.is_leased(&wid, now_ms) {
                tx.execute(
                    "UPDATE work_queue_item SET state = 'queued' WHERE work_id = ?1",
                    params![wid],
                )
                .expect("reclaim");
                self.book.release(&wid);
            }
        }
    }

    /// Insert a queued row and return its work id. `session` sets `data_id` to the
    /// session id; a healthcheck's `data_id` is its own work id (self-reference).
    fn insert(&self, environment_id: &str, data_type: &str, session_id: Option<&str>) -> String {
        let mut guard = self.conn.lock().expect("work queue mutex poisoned");
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin immediate");
        // A portable monotonic order key (no backend-specific autoincrement): the
        // next seq under the write lock, so ids are enqueue-ordered on both stores.
        let next: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), -1) + 1 FROM work_queue_item",
                [],
                |r| r.get(0),
            )
            .expect("next seq");
        let work_id = format!("work_{next:016}");
        // A session carries the session id; a healthcheck references itself.
        let data_id = session_id.unwrap_or(&work_id);
        tx.execute(
            "INSERT INTO work_queue_item \
                (work_id, seq, environment_id, data_type, data_id, metadata_json, state) \
             VALUES (?1, ?2, ?3, ?4, ?5, '{}', 'queued')",
            params![work_id, next, environment_id, data_type, data_id],
        )
        .expect("insert work row");
        tx.commit().expect("commit enqueue");
        work_id
    }

    /// Read the single owned row (belongs to `env_id`) inside `tx`.
    fn owned(tx: &Transaction<'_>, env_id: &str, wid: &str) -> Option<WorkItem> {
        tx.query_row(
            &format!(
                "SELECT {COLS} FROM work_queue_item WHERE work_id = ?1 AND environment_id = ?2"
            ),
            params![wid, env_id],
            row_to_item,
        )
        .optional()
        .expect("read work row")
    }
}

#[async_trait]
impl WorkQueue for SqliteWorkQueue {
    async fn enqueue_session(&self, env_id: &str, session_id: &str) -> String {
        self.insert(env_id, "session", Some(session_id))
    }

    async fn enqueue_healthcheck(&self, env_id: &str) -> String {
        self.insert(env_id, "healthcheck", None)
    }

    async fn list(&self, env_id: &str) -> Vec<WorkItem> {
        let conn = self.conn.lock().expect("work queue mutex poisoned");
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {COLS} FROM work_queue_item WHERE environment_id = ?1 ORDER BY seq ASC"
            ))
            .expect("prepare list");
        let rows = stmt
            .query_map(params![env_id], row_to_item)
            .expect("query list");
        rows.map(|r| r.expect("row")).collect()
    }

    async fn get(&self, env_id: &str, wid: &str) -> Option<WorkItem> {
        let mut guard = self.conn.lock().expect("work queue mutex poisoned");
        let tx = guard.transaction().expect("begin");
        Self::owned(&tx, env_id, wid)
    }

    async fn claim(&self, env_id: &str, worker_id: &str, now_ms: u64) -> Option<WorkItem> {
        self.book.record_poll(env_id, worker_id, now_ms);
        let mut guard = self.conn.lock().expect("work queue mutex poisoned");
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin immediate");
        // Reclaim any lapsed lease first, so a crashed worker doesn't block the env.
        self.reclaim_lapsed(&tx, env_id, now_ms);
        // Single active lease per environment (the open-tier single-worker cap).
        let active: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM work_queue_item WHERE environment_id = ?1 AND state = 'active'",
                params![env_id],
                |r| r.get(0),
            )
            .expect("count active");
        if active > 0 {
            return None;
        }
        // Lease the oldest queued item (ascending seq == enqueue order).
        let wid: Option<String> = tx
            .query_row(
                "SELECT work_id FROM work_queue_item \
                 WHERE environment_id = ?1 AND state = 'queued' ORDER BY seq ASC LIMIT 1",
                params![env_id],
                |r| r.get(0),
            )
            .optional()
            .expect("find queued");
        let wid = wid?;
        tx.execute(
            "UPDATE work_queue_item SET state = 'active', started_at = ?1 WHERE work_id = ?2",
            params![OBJECT_AT, wid],
        )
        .expect("lease");
        let item = Self::owned(&tx, env_id, &wid);
        tx.commit().expect("commit claim");
        self.book.lease(&wid, now_ms);
        item
    }

    async fn ack(&self, env_id: &str, wid: &str) -> Option<WorkItem> {
        let mut guard = self.conn.lock().expect("work queue mutex poisoned");
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin immediate");
        let current = Self::owned(&tx, env_id, wid)?;
        let next = ack_next_state(&current);
        tx.execute(
            "UPDATE work_queue_item SET acknowledged_at = ?1, state = ?2 WHERE work_id = ?3",
            params![OBJECT_AT, next, wid],
        )
        .expect("ack");
        let item = Self::owned(&tx, env_id, wid);
        tx.commit().expect("commit ack");
        item
    }

    async fn heartbeat(&self, env_id: &str, wid: &str, now_ms: u64) -> Option<LeaseReceipt> {
        let mut guard = self.conn.lock().expect("work queue mutex poisoned");
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin immediate");
        let current = Self::owned(&tx, env_id, wid)?;
        tx.execute(
            "UPDATE work_queue_item SET latest_heartbeat_at = ?1 WHERE work_id = ?2",
            params![OBJECT_AT, wid],
        )
        .expect("heartbeat");
        tx.commit().expect("commit heartbeat");
        self.book.lease(wid, now_ms); // extend the lease
        Some(LeaseReceipt {
            lease_extended: true,
            state: current.state.as_str(),
            ttl_seconds: HEARTBEAT_TTL_SECONDS,
        })
    }

    async fn stop(&self, env_id: &str, wid: &str) -> Option<WorkItem> {
        let mut guard = self.conn.lock().expect("work queue mutex poisoned");
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin immediate");
        Self::owned(&tx, env_id, wid)?;
        tx.execute(
            "UPDATE work_queue_item SET stop_requested_at = ?1, stopped_at = ?1, state = 'stopped' \
             WHERE work_id = ?2",
            params![OBJECT_AT, wid],
        )
        .expect("stop");
        let item = Self::owned(&tx, env_id, wid);
        tx.commit().expect("commit stop");
        self.book.release(wid);
        item
    }

    async fn update_metadata(
        &self,
        env_id: &str,
        wid: &str,
        patch: BTreeMap<String, String>,
    ) -> Option<WorkItem> {
        let mut guard = self.conn.lock().expect("work queue mutex poisoned");
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin immediate");
        let mut current = Self::owned(&tx, env_id, wid)?;
        current.metadata.extend(patch);
        let metadata_json = metadata_str(&current.metadata);
        tx.execute(
            "UPDATE work_queue_item SET metadata_json = ?1 WHERE work_id = ?2",
            params![metadata_json, wid],
        )
        .expect("update metadata");
        let item = Self::owned(&tx, env_id, wid);
        tx.commit().expect("commit metadata");
        item
    }

    async fn stats(&self, env_id: &str, now_ms: u64) -> QueueStats {
        let conn = self.conn.lock().expect("work queue mutex poisoned");
        let count = |state_clause: &str| -> usize {
            conn.query_row(
                &format!(
                    "SELECT COUNT(*) FROM work_queue_item WHERE environment_id = ?1 AND {state_clause}"
                ),
                params![env_id],
                |r| r.get::<_, i64>(0),
            )
            .expect("count") as usize
        };
        let depth = count("state = 'queued'");
        let pending = count("state IN ('starting', 'active', 'stopping')");
        // Parity with the in-memory queue: oldest stays set while an item is still
        // processing (queued OR pending), and pollers are counted from the liveness
        // book, not proxied from the active count.
        let has_unfinished = depth > 0 || pending > 0;
        QueueStats {
            depth,
            pending,
            oldest_queued_at: has_unfinished.then(|| OBJECT_AT.to_string()),
            workers_polling: self.book.workers_polling(env_id, now_ms),
        }
    }

    async fn remove_env(&self, env_id: &str) {
        let ids: Vec<String> = self.list(env_id).await.into_iter().map(|w| w.id).collect();
        let conn = self.conn.lock().expect("work queue mutex poisoned");
        conn.execute(
            "DELETE FROM work_queue_item WHERE environment_id = ?1",
            params![env_id],
        )
        .expect("purge env work");
        drop(conn);
        self.book.forget_env(env_id, &ids);
    }
}

fn pg_row_to_item(row: &PgRow) -> WorkItem {
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
    )
}

/// A Postgres-backed [`WorkQueue`] — the network-DB sibling over the same
/// `work_queue` migration scope, for distributed deployments. Claims run in a
/// transaction that mirrors the SQLite semantics (the single-active-per-env cap);
/// multi-worker fan-out (`FOR UPDATE SKIP LOCKED`, no cap) is the later step.
pub struct PostgresWorkQueue {
    pool: PgPool,
    book: LeaseBook,
}

impl PostgresWorkQueue {
    /// Connect and apply the work-queue migrations under the `work_queue` namespace.
    pub async fn connect(url: &str) -> Result<Self, String> {
        let pool = PgPool::connect(url).await.map_err(|e| e.to_string())?;
        Self::with_pool(pool).await
    }

    /// Build from an existing pool: apply the work-queue migrations.
    pub async fn with_pool(pool: PgPool) -> Result<Self, String> {
        let bundle = work_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|e| e.to_string())?
            .run_bundle(&bundle)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self {
            pool,
            book: LeaseBook::default(),
        })
    }

    async fn insert(&self, env_id: &str, data_type: &str, session_id: Option<&str>) -> String {
        let mut tx = self.pool.begin().await.expect("begin");
        let next: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(seq), -1) + 1 FROM work_queue_item")
                .fetch_one(&mut *tx)
                .await
                .expect("next seq");
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
        .expect("insert work row");
        tx.commit().await.expect("commit enqueue");
        work_id
    }

    async fn fetch_owned(&self, env_id: &str, wid: &str) -> Option<WorkItem> {
        sqlx::query(&format!(
            "SELECT {COLS} FROM work_queue_item WHERE work_id = $1 AND environment_id = $2"
        ))
        .bind(wid)
        .bind(env_id)
        .fetch_optional(&self.pool)
        .await
        .expect("read work row")
        .map(|r| pg_row_to_item(&r))
    }
}

#[async_trait]
impl WorkQueue for PostgresWorkQueue {
    async fn enqueue_session(&self, env_id: &str, session_id: &str) -> String {
        self.insert(env_id, "session", Some(session_id)).await
    }

    async fn enqueue_healthcheck(&self, env_id: &str) -> String {
        self.insert(env_id, "healthcheck", None).await
    }

    async fn list(&self, env_id: &str) -> Vec<WorkItem> {
        sqlx::query(&format!(
            "SELECT {COLS} FROM work_queue_item WHERE environment_id = $1 ORDER BY seq ASC"
        ))
        .bind(env_id)
        .fetch_all(&self.pool)
        .await
        .expect("query list")
        .iter()
        .map(pg_row_to_item)
        .collect()
    }

    async fn get(&self, env_id: &str, wid: &str) -> Option<WorkItem> {
        self.fetch_owned(env_id, wid).await
    }

    async fn claim(&self, env_id: &str, worker_id: &str, now_ms: u64) -> Option<WorkItem> {
        self.book.record_poll(env_id, worker_id, now_ms);
        let mut tx = self.pool.begin().await.expect("begin");
        // Reclaim lapsed leases first (a crashed worker must not block the env): any
        // `active` row whose in-process lease has expired returns to `queued`.
        let active_ids: Vec<String> = sqlx::query_scalar(
            "SELECT work_id FROM work_queue_item WHERE environment_id = $1 AND state = 'active'",
        )
        .bind(env_id)
        .fetch_all(&mut *tx)
        .await
        .expect("active ids");
        for wid in &active_ids {
            if !self.book.is_leased(wid, now_ms) {
                sqlx::query("UPDATE work_queue_item SET state = 'queued' WHERE work_id = $1")
                    .bind(wid)
                    .execute(&mut *tx)
                    .await
                    .expect("reclaim");
                self.book.release(wid);
            }
        }
        // Single active lease per environment (the open-tier single-worker cap).
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM work_queue_item WHERE environment_id = $1 AND state = 'active'",
        )
        .bind(env_id)
        .fetch_one(&mut *tx)
        .await
        .expect("count active");
        if active > 0 {
            return None;
        }
        // Lease the oldest queued item, locking the chosen row.
        let wid: Option<String> = sqlx::query_scalar(
            "SELECT work_id FROM work_queue_item \
             WHERE environment_id = $1 AND state = 'queued' \
             ORDER BY seq ASC LIMIT 1 FOR UPDATE SKIP LOCKED",
        )
        .bind(env_id)
        .fetch_optional(&mut *tx)
        .await
        .expect("find queued");
        let wid = wid?;
        sqlx::query(
            "UPDATE work_queue_item SET state = 'active', started_at = $1 WHERE work_id = $2",
        )
        .bind(OBJECT_AT)
        .bind(&wid)
        .execute(&mut *tx)
        .await
        .expect("lease");
        tx.commit().await.expect("commit claim");
        self.book.lease(&wid, now_ms);
        self.fetch_owned(env_id, &wid).await
    }

    async fn ack(&self, env_id: &str, wid: &str) -> Option<WorkItem> {
        let current = self.fetch_owned(env_id, wid).await?;
        let next = ack_next_state(&current);
        sqlx::query(
            "UPDATE work_queue_item SET acknowledged_at = $1, state = $2 \
             WHERE work_id = $3 AND environment_id = $4",
        )
        .bind(OBJECT_AT)
        .bind(next)
        .bind(wid)
        .bind(env_id)
        .execute(&self.pool)
        .await
        .expect("ack");
        self.fetch_owned(env_id, wid).await
    }

    async fn heartbeat(&self, env_id: &str, wid: &str, now_ms: u64) -> Option<LeaseReceipt> {
        let current = self.fetch_owned(env_id, wid).await?;
        sqlx::query(
            "UPDATE work_queue_item SET latest_heartbeat_at = $1 \
             WHERE work_id = $2 AND environment_id = $3",
        )
        .bind(OBJECT_AT)
        .bind(wid)
        .bind(env_id)
        .execute(&self.pool)
        .await
        .expect("heartbeat");
        self.book.lease(wid, now_ms); // extend the lease
        Some(LeaseReceipt {
            lease_extended: true,
            state: current.state.as_str(),
            ttl_seconds: HEARTBEAT_TTL_SECONDS,
        })
    }

    async fn stop(&self, env_id: &str, wid: &str) -> Option<WorkItem> {
        self.fetch_owned(env_id, wid).await?;
        sqlx::query(
            "UPDATE work_queue_item SET stop_requested_at = $1, stopped_at = $1, state = 'stopped' \
             WHERE work_id = $2 AND environment_id = $3",
        )
        .bind(OBJECT_AT)
        .bind(wid)
        .bind(env_id)
        .execute(&self.pool)
        .await
        .expect("stop");
        self.book.release(wid);
        self.fetch_owned(env_id, wid).await
    }

    async fn update_metadata(
        &self,
        env_id: &str,
        wid: &str,
        patch: BTreeMap<String, String>,
    ) -> Option<WorkItem> {
        let mut current = self.fetch_owned(env_id, wid).await?;
        current.metadata.extend(patch);
        let metadata_json = metadata_str(&current.metadata);
        sqlx::query(
            "UPDATE work_queue_item SET metadata_json = $1 WHERE work_id = $2 AND environment_id = $3",
        )
        .bind(metadata_json)
        .bind(wid)
        .bind(env_id)
        .execute(&self.pool)
        .await
        .expect("update metadata");
        self.fetch_owned(env_id, wid).await
    }

    async fn stats(&self, env_id: &str, now_ms: u64) -> QueueStats {
        let count = |clause: &'static str| {
            let pool = self.pool.clone();
            let env = env_id.to_string();
            async move {
                sqlx::query_scalar::<_, i64>(&format!(
                    "SELECT COUNT(*) FROM work_queue_item WHERE environment_id = $1 AND {clause}"
                ))
                .bind(env)
                .fetch_one(&pool)
                .await
                .expect("count") as usize
            }
        };
        let depth = count("state = 'queued'").await;
        let pending = count("state IN ('starting', 'active', 'stopping')").await;
        // Parity with the in-memory queue: oldest persists while processing, and
        // pollers come from the liveness book (not the active-count proxy).
        let has_unfinished = depth > 0 || pending > 0;
        QueueStats {
            depth,
            pending,
            oldest_queued_at: has_unfinished.then(|| OBJECT_AT.to_string()),
            workers_polling: self.book.workers_polling(env_id, now_ms),
        }
    }

    async fn remove_env(&self, env_id: &str) {
        let ids: Vec<String> = self.list(env_id).await.into_iter().map(|w| w.id).collect();
        sqlx::query("DELETE FROM work_queue_item WHERE environment_id = $1")
            .bind(env_id)
            .execute(&self.pool)
            .await
            .expect("purge env work");
        self.book.forget_env(env_id, &ids);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_session_contract::work_queue::LEASE_TTL_MS;

    fn q() -> SqliteWorkQueue {
        SqliteWorkQueue::open_in_memory().unwrap()
    }

    #[tokio::test]
    async fn healthcheck_seed_self_references_and_survives_reload() {
        let q = q();
        let id = q.enqueue_healthcheck("env_a").await;
        let w = q.get("env_a", &id).await.expect("seeded");
        assert_eq!(w.state, WorkState::Queued);
        assert!(matches!(w.data, WorkPayload::HealthCheck { id: ref d } if *d == id));
    }

    #[tokio::test]
    async fn claim_leases_oldest_and_caps_at_one_active() {
        let q = q();
        let w1 = q.enqueue_session("env_a", "s1").await;
        let _w2 = q.enqueue_session("env_a", "s2").await;
        let leased = q.claim("env_a", "w", 0).await.expect("leases oldest");
        assert_eq!(leased.id, w1);
        assert_eq!(leased.state, WorkState::Active);
        assert!(
            q.claim("env_a", "w", 0).await.is_none(),
            "single active lease"
        );
        q.stop("env_a", &w1).await.expect("stop");
        assert!(
            q.claim("env_a", "w", 0).await.is_some(),
            "next lease after stop"
        );
    }

    #[tokio::test]
    async fn durable_reclaims_an_expired_lease_on_the_next_poll() {
        let q = q();
        let w1 = q.enqueue_session("env_a", "s1").await;
        assert_eq!(q.claim("env_a", "a", 0).await.expect("lease").id, w1);
        // Live lease caps; a lapsed lease is reclaimed and re-leased.
        assert!(
            q.claim("env_a", "b", 1_000).await.is_none(),
            "live lease caps"
        );
        let reclaimed = q
            .claim("env_a", "b", LEASE_TTL_MS + 1)
            .await
            .expect("expired lease reclaimed");
        assert_eq!(reclaimed.id, w1);
        assert_eq!(reclaimed.state, WorkState::Active);
    }

    #[tokio::test]
    async fn ack_heartbeat_and_membership_match_the_in_memory_contract() {
        let q = q();
        let id = q.enqueue_session("env_a", "s1").await;
        assert_eq!(
            q.ack("env_a", &id).await.expect("ack").state,
            WorkState::Starting
        );
        assert!(
            q.heartbeat("env_a", &id, 0)
                .await
                .expect("hb")
                .lease_extended
        );
        // Wrong env → none across the board.
        assert!(q.get("env_b", &id).await.is_none());
        assert!(q.ack("env_b", &id).await.is_none());
        assert!(q.heartbeat("env_b", &id, 0).await.is_none());
        assert!(q.stop("env_b", &id).await.is_none());
    }

    #[tokio::test]
    async fn stats_and_metadata_and_remove_env() {
        let q = q();
        q.enqueue_healthcheck("env_a").await;
        let s = q.enqueue_session("env_a", "s1").await;
        let st = q.stats("env_a", 0).await;
        assert_eq!((st.depth, st.pending, st.workers_polling), (2, 0, 0));
        q.claim("env_a", "w1", 0).await;
        let st = q.stats("env_a", 0).await;
        assert_eq!((st.depth, st.pending, st.workers_polling), (1, 1, 1));
        let patch = BTreeMap::from([("k".to_string(), "v".to_string())]);
        let up = q.update_metadata("env_a", &s, patch).await.expect("patch");
        assert_eq!(up.metadata.get("k").map(String::as_str), Some("v"));
        q.remove_env("env_a").await;
        assert!(q.list("env_a").await.is_empty(), "env delete purges work");
    }

    #[tokio::test]
    async fn list_is_enqueue_ordered() {
        let q = q();
        let a = q.enqueue_session("e", "s1").await;
        let b = q.enqueue_session("e", "s2").await;
        let ids: Vec<String> = q.list("e").await.into_iter().map(|w| w.id).collect();
        assert_eq!(ids, vec![a, b]);
    }

    #[tokio::test]
    async fn file_open_and_pg_connect_entry_points() {
        // The file-backed `open` (not just `open_in_memory`).
        let dir = std::env::temp_dir().join(format!("wq-cov-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wq.db");
        let q = SqliteWorkQueue::open(path.to_str().unwrap()).expect("open file db");
        let id = q.enqueue_session("e", "s").await;
        assert!(q.get("e", &id).await.is_some());
        std::fs::remove_dir_all(&dir).ok();
        // The `connect(url)` path over a live Postgres (skips when unreachable).
        if let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL")
            && let Ok(q) = PostgresWorkQueue::connect(&url).await
        {
            let id = q.enqueue_healthcheck("cov_env").await;
            assert!(q.get("cov_env", &id).await.is_some());
            q.remove_env("cov_env").await;
        }
    }

    /// Live Postgres parity over the same portable bundle. Skips when no Postgres is
    /// reachable (`AWAKEN_TEST_DATABASE_URL`), isolated in its own schema.
    #[tokio::test]
    async fn postgres_parity_over_a_live_db() {
        use sqlx::Executor;
        use sqlx::postgres::{PgPool, PgPoolOptions};

        let url = std::env::var("AWAKEN_TEST_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://postgres:pw@127.0.0.1:5455/cov".to_string());
        let Ok(admin) = PgPool::connect(&url).await else {
            println!("[skip] no Postgres reachable");
            return;
        };
        let _ = admin
            .execute("DROP SCHEMA IF EXISTS t_work_queue CASCADE")
            .await;
        admin
            .execute("CREATE SCHEMA t_work_queue")
            .await
            .expect("schema");
        admin.close().await;
        let pool = PgPoolOptions::new()
            .after_connect(|conn, _| {
                Box::pin(async move {
                    conn.execute("SET search_path = t_work_queue").await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .expect("schema pool");
        let q = PostgresWorkQueue::with_pool(pool).await.expect("store");

        let hc = q.enqueue_healthcheck("env_a").await;
        let w1 = q.enqueue_session("env_a", "s1").await;
        let w2 = q.enqueue_session("env_a", "s2").await;
        // list is enqueue-ordered
        let ids: Vec<String> = q.list("env_a").await.into_iter().map(|w| w.id).collect();
        assert_eq!(ids, vec![hc.clone(), w1.clone(), w2]);
        // claim leases the oldest + single-active cap
        assert_eq!(q.claim("env_a", "w", 0).await.expect("lease").id, hc);
        assert!(
            q.claim("env_a", "w", 0).await.is_none(),
            "single active lease"
        );
        // ack + heartbeat + membership
        assert_eq!(
            q.ack("env_a", &w1).await.expect("ack").state,
            WorkState::Starting
        );
        assert!(
            q.heartbeat("env_a", &w1, 0)
                .await
                .expect("hb")
                .lease_extended
        );
        assert!(q.get("env_b", &w1).await.is_none(), "wrong env → none");
        assert!(q.heartbeat("env_b", &w1, 0).await.is_none());
        // stats reflect the active healthcheck + starting w1
        let st = q.stats("env_a", 0).await;
        assert!(st.pending >= 1 && st.workers_polling == 1);
        // metadata + stop + remove_env
        let patch = BTreeMap::from([("k".to_string(), "v".to_string())]);
        assert_eq!(
            q.update_metadata("env_a", &w1, patch)
                .await
                .expect("md")
                .metadata
                .get("k")
                .map(String::as_str),
            Some("v")
        );
        assert!(q.stop("env_a", &hc).await.is_some());
        q.remove_env("env_a").await;
        assert!(q.list("env_a").await.is_empty(), "remove_env purges");
    }
}
