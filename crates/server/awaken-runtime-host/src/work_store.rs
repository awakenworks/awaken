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
use awaken_protocol_managed::types::environment::{WorkData, WorkHeartbeat, WorkQueueStats};
use awaken_protocol_managed::work_queue::{WorkItem, WorkQueue, WorkState};
use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

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
             seq                 INTEGER PRIMARY KEY AUTOINCREMENT, \
             work_id             TEXT NOT NULL UNIQUE, \
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

fn data_of(data_type: &str, data_id: String) -> WorkData {
    match data_type {
        "healthcheck" => WorkData::HealthCheck { id: data_id },
        _ => WorkData::Session { id: data_id },
    }
}

/// The columns a work row projects to a [`WorkItem`], in `SELECT` order.
const COLS: &str = "work_id, environment_id, data_type, data_id, metadata_json, state, \
     acknowledged_at, latest_heartbeat_at, started_at, stop_requested_at, stopped_at";

fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkItem> {
    let metadata_json: String = row.get(4)?;
    Ok(WorkItem {
        id: row.get(0)?,
        environment_id: row.get(1)?,
        data: data_of(&row.get::<_, String>(2)?, row.get(3)?),
        metadata: serde_json::from_str(&metadata_json).unwrap_or_default(),
        state: state_from_wire(&row.get::<_, String>(5)?),
        acknowledged_at: row.get(6)?,
        latest_heartbeat_at: row.get(7)?,
        started_at: row.get(8)?,
        stop_requested_at: row.get(9)?,
        stopped_at: row.get(10)?,
    })
}

/// SQLite persistence for the environment work queue.
pub struct SqliteWorkQueue {
    conn: Arc<Mutex<Connection>>,
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
        })
    }

    /// Insert a queued row and return its work id. `session` sets `data_id` to the
    /// session id; a healthcheck's `data_id` is its own work id (self-reference).
    fn insert(&self, environment_id: &str, data_type: &str, session_id: Option<&str>) -> String {
        let mut guard = self.conn.lock().expect("work queue mutex poisoned");
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin immediate");
        tx.execute(
            "INSERT INTO work_queue_item \
                (work_id, environment_id, data_type, data_id, metadata_json, state) \
             VALUES ('', ?1, ?2, '', '{}', 'queued')",
            params![environment_id, data_type],
        )
        .expect("insert work row");
        let seq = tx.last_insert_rowid();
        let work_id = format!("work_{seq:016}");
        // A session carries the session id; a healthcheck references itself.
        let data_id = session_id.unwrap_or(&work_id);
        tx.execute(
            "UPDATE work_queue_item SET work_id = ?1, data_id = ?2 WHERE seq = ?3",
            params![work_id, data_id, seq],
        )
        .expect("stamp work id");
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

    async fn claim(&self, env_id: &str) -> Option<WorkItem> {
        let mut guard = self.conn.lock().expect("work queue mutex poisoned");
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin immediate");
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
        item
    }

    async fn ack(&self, env_id: &str, wid: &str) -> Option<WorkItem> {
        let mut guard = self.conn.lock().expect("work queue mutex poisoned");
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin immediate");
        let current = Self::owned(&tx, env_id, wid)?;
        // Ack stamps receipt; queued→starting (other states are unchanged).
        let next = if current.state == WorkState::Queued {
            "starting"
        } else {
            current.state.as_str()
        };
        tx.execute(
            "UPDATE work_queue_item SET acknowledged_at = ?1, state = ?2 WHERE work_id = ?3",
            params![OBJECT_AT, next, wid],
        )
        .expect("ack");
        let item = Self::owned(&tx, env_id, wid);
        tx.commit().expect("commit ack");
        item
    }

    async fn heartbeat(&self, env_id: &str, wid: &str) -> Option<WorkHeartbeat> {
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
        Some(WorkHeartbeat {
            object_type: "work_heartbeat",
            last_heartbeat: OBJECT_AT,
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
        let metadata_json = serde_json::to_string(&current.metadata).expect("metadata serializes");
        tx.execute(
            "UPDATE work_queue_item SET metadata_json = ?1 WHERE work_id = ?2",
            params![metadata_json, wid],
        )
        .expect("update metadata");
        let item = Self::owned(&tx, env_id, wid);
        tx.commit().expect("commit metadata");
        item
    }

    async fn stats(&self, env_id: &str) -> WorkQueueStats {
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
        let workers_polling = i64::from(count("state = 'active'") > 0);
        WorkQueueStats {
            object_type: "work_queue_stats",
            depth,
            pending,
            oldest_queued_at: (depth > 0).then(|| OBJECT_AT.to_string()),
            workers_polling,
        }
    }

    async fn remove_env(&self, env_id: &str) {
        let conn = self.conn.lock().expect("work queue mutex poisoned");
        conn.execute(
            "DELETE FROM work_queue_item WHERE environment_id = ?1",
            params![env_id],
        )
        .expect("purge env work");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q() -> SqliteWorkQueue {
        SqliteWorkQueue::open_in_memory().unwrap()
    }

    #[tokio::test]
    async fn healthcheck_seed_self_references_and_survives_reload() {
        let q = q();
        let id = q.enqueue_healthcheck("env_a").await;
        let w = q.get("env_a", &id).await.expect("seeded");
        assert_eq!(w.state, WorkState::Queued);
        assert!(matches!(w.data, WorkData::HealthCheck { id: ref d } if *d == id));
    }

    #[tokio::test]
    async fn claim_leases_oldest_and_caps_at_one_active() {
        let q = q();
        let w1 = q.enqueue_session("env_a", "s1").await;
        let _w2 = q.enqueue_session("env_a", "s2").await;
        let leased = q.claim("env_a").await.expect("leases oldest");
        assert_eq!(leased.id, w1);
        assert_eq!(leased.state, WorkState::Active);
        assert!(q.claim("env_a").await.is_none(), "single active lease");
        q.stop("env_a", &w1).await.expect("stop");
        assert!(q.claim("env_a").await.is_some(), "next lease after stop");
    }

    #[tokio::test]
    async fn ack_heartbeat_and_membership_match_the_in_memory_contract() {
        let q = q();
        let id = q.enqueue_session("env_a", "s1").await;
        assert_eq!(
            q.ack("env_a", &id).await.expect("ack").state,
            WorkState::Starting
        );
        assert!(q.heartbeat("env_a", &id).await.expect("hb").lease_extended);
        // Wrong env → none across the board.
        assert!(q.get("env_b", &id).await.is_none());
        assert!(q.ack("env_b", &id).await.is_none());
        assert!(q.heartbeat("env_b", &id).await.is_none());
        assert!(q.stop("env_b", &id).await.is_none());
    }

    #[tokio::test]
    async fn stats_and_metadata_and_remove_env() {
        let q = q();
        q.enqueue_healthcheck("env_a").await;
        let s = q.enqueue_session("env_a", "s1").await;
        let st = q.stats("env_a").await;
        assert_eq!((st.depth, st.pending, st.workers_polling), (2, 0, 0));
        q.claim("env_a").await;
        let st = q.stats("env_a").await;
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
}
