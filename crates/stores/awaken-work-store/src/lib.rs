//! Durable [`WorkQueue`] backend: the self-hosted environment work queue over a
//! store, so a session dispatched to a self-hosted environment survives a restart
//! and can be claimed by a worker on any node. SQLite (embedded, single machine)
//! lands here; the Postgres backend (distributed) shares the one portable bundle,
//! exactly like the session/config/catalog stores.
//!
//! Its own scope (`work_queue_item` table + `work_queue_schema_migrations` ledger),
//! a distinct aggregate from the managed session config. The lease is the store's
//! own transaction: SQLite claims under a `BEGIN IMMEDIATE` write lock, so a run is
//! owned by one worker at a time. PostgreSQL locks an environment's work rows so
//! its single-active decision is atomic across processes. No secret is minted;
//! `secret` stays `null` on the wire.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_session_contract::work_queue::{
    HeartbeatResult, LeaseHeartbeat, LeaseReceipt, QueueStats, SessionWorkLease, WorkItem,
    WorkMutationResult, WorkQueue, WorkQueueError,
};

fn storage(error: impl std::fmt::Display) -> WorkQueueError {
    WorkQueueError::Storage(error.to_string())
}

fn lease_epoch(current: i64, advance: bool) -> Result<(i64, u64), WorkQueueError> {
    let current = u64::try_from(current).map_err(storage)?;
    let next = if advance {
        current
            .checked_add(1)
            .ok_or_else(|| storage("work lease epoch exhausted"))?
    } else {
        current
    };
    Ok((i64::try_from(next).map_err(storage)?, next))
}
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sqlx::Row;
use sqlx::postgres::{PgPool, PgRow};

// Poll-liveness bookkeeping is shared by the durable stores. Lease authority remains
// in their rows. The volatile executable specification is never part of a default
// product build.
mod lease_book;
pub use lease_book::{LEASE_TTL_MS, LeaseBook, POLLER_WINDOW_MS};
#[cfg(any(test, feature = "test-support"))]
mod inmem;
#[cfg(any(test, feature = "test-support"))]
pub use inmem::InMemoryWorkQueue;
mod schema;
use schema::*;

#[cfg(test)]
mod product_readiness_tests {
    #[test]
    fn volatile_queue_is_opt_in_and_its_export_is_feature_gated() {
        // Cause/effect graph: C1 the product uses default features; C2 test-support
        // is explicitly enabled. Effects: E1 the volatile backend is unreachable;
        // E2 the reference backend is available to conformance tests. Constraint:
        // C1 and C2 are mutually exclusive build selections for this boundary.
        //
        // | Rule | default product | test-support | InMemoryWorkQueue export |
        // | T1   | yes             | no           | absent                   |
        // | T2   | no              | yes          | present                  |
        //
        // T1 is completed by the default-feature `cargo check`; T2 is completed by
        // the all-features conformance suite. This source-level fitness assertion
        // prevents either selector from being silently removed or made default.
        let manifest = include_str!("../Cargo.toml");
        let source = include_str!("lib.rs");
        assert!(manifest.contains("test-support = []"), "T2 selector");
        assert!(
            !manifest.contains("default = [\"test-support\"]"),
            "T1 must remain the default"
        );
        assert!(
            source.contains("#[cfg(any(test, feature = \"test-support\"))]\nmod inmem;")
                && source.contains(
                    "#[cfg(any(test, feature = \"test-support\"))]\npub use inmem::InMemoryWorkQueue;"
                ),
            "T1/T2 export gate"
        );
    }
}

/// SQLite persistence for the environment work queue. Ownership, epoch and expiry
/// are durable because they are safety authority; only poller liveness is ephemeral.
pub struct SqliteWorkQueue {
    conn: Arc<Mutex<Connection>>,
    book: LeaseBook,
}

impl SqliteWorkQueue {
    /// Open (or create) the work-queue database at `path` and apply migrations.
    pub fn open(path: &str) -> Result<Self, String> {
        Self::from_connection(Connection::open(path).map_err(|e| e.to_string())?)
    }

    /// A private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
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

    /// Reclaim `env_id`'s `active` rows whose durable lease has lapsed.
    fn reclaim_lapsed(
        &self,
        tx: &Transaction<'_>,
        env_id: &str,
        now_ms: u64,
    ) -> Result<(), WorkQueueError> {
        tx.execute(
            "UPDATE work_queue_item \
             SET state = 'queued', lease_owner = NULL, lease_expires_ms = NULL, \
                 lease_refreshed_ms = NULL, latest_heartbeat_at = NULL \
             WHERE environment_id = ?1 AND state = 'active' \
               AND (lease_expires_ms IS NULL OR lease_expires_ms <= ?2)",
            params![env_id, db_millis(now_ms)],
        )
        .map_err(storage)?;
        Ok(())
    }

    /// Insert a queued row and return its work id. `session` sets `data_id` to the
    /// session id; a healthcheck's `data_id` is its own work id (self-reference).
    fn insert(
        &self,
        environment_id: &str,
        data_type: &str,
        session_id: Option<&str>,
    ) -> Result<String, WorkQueueError> {
        let mut guard = self.conn.lock().map_err(storage)?;
        let tx = guard
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
        // A portable monotonic order key (no backend-specific autoincrement): the
        // next seq under the write lock, so ids are enqueue-ordered on both stores.
        let next: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), -1) + 1 FROM work_queue_item",
                [],
                |r| r.get(0),
            )
            .map_err(storage)?;
        let work_id = format!("work_{next:016}");
        // A session carries the session id; a healthcheck references itself.
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

    /// Read the single owned row (belongs to `env_id`) inside `tx`.
    fn owned(
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

#[async_trait]
impl WorkQueue for SqliteWorkQueue {
    async fn enqueue_session(
        &self,
        env_id: &str,
        session_id: &str,
    ) -> Result<String, WorkQueueError> {
        self.insert(env_id, "session", Some(session_id))
    }

    async fn wake_session(&self, env_id: &str, session_id: &str) -> Result<String, WorkQueueError> {
        let work_id = self.insert(env_id, "session", Some(session_id))?;
        let mut guard = self.conn.lock().map_err(storage)?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        tx.execute(
            "UPDATE work_queue_item SET state = 'queued', acknowledged_at = NULL, \
             latest_heartbeat_at = NULL, started_at = NULL, stop_requested_at = NULL, \
             stopped_at = NULL, lease_owner = NULL, lease_expires_ms = NULL, \
             lease_refreshed_ms = NULL WHERE work_id = ?1 AND environment_id = ?2 \
             AND data_type = 'session' AND data_id = ?3 AND state = 'stopped'",
            params![work_id, env_id, session_id],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(work_id)
    }

    async fn enqueue_healthcheck(&self, env_id: &str) -> Result<String, WorkQueueError> {
        self.insert(env_id, "healthcheck", None)
    }

    async fn ensure_healthcheck(&self, env_id: &str) -> Result<String, WorkQueueError> {
        let mut guard = self.conn.lock().map_err(storage)?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        if let Some(id) = tx
            .query_row(
                "SELECT work_id FROM work_queue_item WHERE environment_id = ?1 AND data_type = 'healthcheck' ORDER BY seq ASC LIMIT 1",
                params![env_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?
        {
            tx.commit().map_err(storage)?;
            return Ok(id);
        }
        let next: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), -1) + 1 FROM work_queue_item",
                [],
                |row| row.get(0),
            )
            .map_err(storage)?;
        let work_id = format!("work_{next:016}");
        tx.execute(
            "INSERT INTO work_queue_item (work_id, seq, environment_id, data_type, data_id, metadata_json, state) VALUES (?1, ?2, ?3, 'healthcheck', ?1, '{}', 'queued')",
            params![work_id, next, env_id],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(work_id)
    }

    async fn list(&self, env_id: &str) -> Result<Vec<WorkItem>, WorkQueueError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {COLS} FROM work_queue_item WHERE environment_id = ?1 ORDER BY seq ASC"
            ))
            .map_err(storage)?;
        let rows = stmt
            .query_map(params![env_id], row_to_item)
            .map_err(storage)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(storage)
    }

    async fn get(&self, env_id: &str, wid: &str) -> Result<Option<WorkItem>, WorkQueueError> {
        let mut guard = self.conn.lock().map_err(storage)?;
        let tx = guard.transaction().map_err(storage)?;
        Self::owned(&tx, env_id, wid)
    }

    async fn claim(
        &self,
        env_id: &str,
        worker_id: &str,
        now_ms: u64,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        self.claim_with_reclaim(env_id, worker_id, worker_id, now_ms, None)
            .await
    }

    async fn claim_with_reclaim(
        &self,
        env_id: &str,
        lease_owner: &str,
        poller_id: &str,
        now_ms: u64,
        age_ms: Option<u64>,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        self.book.record_poll(env_id, poller_id, now_ms);
        if let Some(age) = age_ms.filter(|age| *age <= now_ms) {
            let mut guard = self.conn.lock().map_err(storage)?;
            let tx = guard
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let cutoff = db_millis(now_ms - age);
            tx.execute(
                "UPDATE work_queue_item SET state = 'queued', lease_owner = NULL, lease_expires_ms = NULL, lease_refreshed_ms = NULL, latest_heartbeat_at = NULL \
                 WHERE environment_id = ?1 AND state = 'active' AND lease_refreshed_ms IS NOT NULL AND lease_refreshed_ms <= ?2",
                params![env_id, cutoff],
            ).map_err(storage)?;
            tx.commit().map_err(storage)?;
        }
        let mut guard = self.conn.lock().map_err(storage)?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        // Reclaim any lapsed lease first, so a crashed worker doesn't block the env.
        self.reclaim_lapsed(&tx, env_id, now_ms)?;
        // Single active lease per environment (the open-tier single-worker cap).
        let active: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM work_queue_item WHERE environment_id = ?1 AND state = 'active'",
                params![env_id],
                |r| r.get(0),
            )
            .map_err(storage)?;
        if active > 0 {
            return Ok(None);
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
            .map_err(storage)?;
        let Some(wid) = wid else {
            return Ok(None);
        };
        tx.execute(
            "UPDATE work_queue_item \
             SET state = 'active', started_at = ?1, lease_owner = ?2, \
                 lease_epoch = lease_epoch + 1, lease_expires_ms = ?3, \
                 lease_refreshed_ms = ?4, latest_heartbeat_at = NULL \
             WHERE work_id = ?5",
            params![
                OBJECT_AT,
                lease_owner,
                lease_expiry(now_ms, HEARTBEAT_TTL_SECONDS),
                db_millis(now_ms),
                wid,
            ],
        )
        .map_err(storage)?;
        let item = Self::owned(&tx, env_id, &wid)?;
        tx.commit().map_err(storage)?;
        Ok(item)
    }

    async fn ack(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        let mut guard = self.conn.lock().map_err(storage)?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let Some(current) = Self::owned(&tx, env_id, wid)? else {
            return Ok(WorkMutationResult::NotFound);
        };
        let owner: Option<String> = tx
            .query_row(
                "SELECT lease_owner FROM work_queue_item WHERE work_id = ?1 AND environment_id = ?2",
                params![wid, env_id],
                |row| row.get(0),
            )
            .map_err(storage)?;
        if owner.as_deref() != Some(worker_id) {
            return Ok(WorkMutationResult::PreconditionFailed);
        }
        let next = ack_next_state(&current);
        tx.execute(
            "UPDATE work_queue_item SET acknowledged_at = ?1, state = ?2 WHERE work_id = ?3",
            params![OBJECT_AT, next, wid],
        )
        .map_err(storage)?;
        let item = Self::owned(&tx, env_id, wid)?;
        tx.commit().map_err(storage)?;
        Ok(item
            .map(WorkMutationResult::accepted)
            .unwrap_or(WorkMutationResult::NotFound))
    }

    async fn heartbeat(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        now_ms: u64,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, WorkQueueError> {
        let mut guard = self.conn.lock().map_err(storage)?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let Some(current) = Self::owned(&tx, env_id, wid)? else {
            return Ok(HeartbeatResult::NotFound);
        };
        let owner: Option<String> = tx
            .query_row(
                "SELECT lease_owner FROM work_queue_item \
                 WHERE work_id = ?1 AND environment_id = ?2",
                params![wid, env_id],
                |row| row.get(0),
            )
            .map_err(storage)?;
        if owner.as_deref() != Some(worker_id) {
            return Ok(HeartbeatResult::PreconditionFailed);
        }
        if !heartbeat
            .condition
            .permits(current.latest_heartbeat_at.as_deref())
        {
            return Ok(HeartbeatResult::PreconditionFailed);
        }
        let extended = current.state.can_extend_lease();
        let ttl_seconds = effective_ttl_seconds(heartbeat.desired_ttl_seconds);
        let last_heartbeat = heartbeat_at(now_ms, current.latest_heartbeat_at.as_deref());
        if extended {
            tx.execute(
                "UPDATE work_queue_item \
                 SET latest_heartbeat_at = ?1, lease_expires_ms = ?2, lease_refreshed_ms = ?3 \
                 WHERE work_id = ?4 AND environment_id = ?5 AND state = 'active' \
                   AND lease_owner = ?6",
                params![
                    &last_heartbeat,
                    lease_expiry(now_ms, ttl_seconds),
                    db_millis(now_ms),
                    wid,
                    env_id,
                    worker_id
                ],
            )
            .map_err(storage)?;
        }
        tx.commit().map_err(storage)?;
        Ok(HeartbeatResult::Accepted(LeaseReceipt {
            last_heartbeat,
            lease_extended: extended,
            state: current.state.as_str(),
            ttl_seconds,
        }))
    }

    async fn stop(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        let mut guard = self.conn.lock().map_err(storage)?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        if Self::owned(&tx, env_id, wid)?.is_none() {
            return Ok(WorkMutationResult::NotFound);
        }
        let owner: Option<String> = tx
            .query_row(
                "SELECT lease_owner FROM work_queue_item WHERE work_id = ?1 AND environment_id = ?2",
                params![wid, env_id],
                |row| row.get(0),
            )
            .map_err(storage)?;
        if owner.as_deref() != Some(worker_id) {
            return Ok(WorkMutationResult::PreconditionFailed);
        }
        tx.execute(
            "UPDATE work_queue_item SET stop_requested_at = ?1, stopped_at = ?1, \
             state = 'stopped', lease_owner = NULL, lease_expires_ms = NULL, \
             lease_refreshed_ms = NULL \
             WHERE work_id = ?2",
            params![OBJECT_AT, wid],
        )
        .map_err(storage)?;
        let item = Self::owned(&tx, env_id, wid)?;
        tx.commit().map_err(storage)?;
        Ok(item
            .map(WorkMutationResult::accepted)
            .unwrap_or(WorkMutationResult::NotFound))
    }

    async fn release_owner(&self, worker_owner: &str) -> Result<usize, WorkQueueError> {
        let guard = self.conn.lock().map_err(storage)?;
        guard
            .execute(
                "UPDATE work_queue_item SET stop_requested_at = ?1, stopped_at = ?1, \
                 state = 'stopped', lease_owner = NULL, lease_expires_ms = NULL, \
                 lease_refreshed_ms = NULL WHERE data_type = 'session' \
                 AND state = 'active' AND lease_owner = ?2",
                params![OBJECT_AT, worker_owner],
            )
            .map_err(storage)
    }

    async fn retire_session(
        &self,
        env_id: &str,
        session_id: &str,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        let mut guard = self.conn.lock().map_err(storage)?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let work_id: Option<String> = tx
            .query_row(
                "SELECT work_id FROM work_queue_item WHERE environment_id = ?1 \
                 AND data_type = 'session' AND data_id = ?2 ORDER BY seq ASC LIMIT 1",
                params![env_id, session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        let Some(work_id) = work_id else {
            return Ok(None);
        };
        tx.execute(
            "UPDATE work_queue_item SET stop_requested_at = ?1, stopped_at = ?1, \
             state = 'stopped', lease_owner = NULL, lease_expires_ms = NULL, \
             lease_refreshed_ms = NULL WHERE work_id = ?2",
            params![OBJECT_AT, work_id],
        )
        .map_err(storage)?;
        let item = Self::owned(&tx, env_id, &work_id)?;
        tx.commit().map_err(storage)?;
        Ok(item)
    }

    async fn acquire_session(
        &self,
        env_id: &str,
        session_id: &str,
        worker_owner: &str,
        now_ms: u64,
    ) -> Result<Option<SessionWorkLease>, WorkQueueError> {
        let work_id = self.insert(env_id, "session", Some(session_id))?;
        let mut guard = self.conn.lock().map_err(storage)?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        self.reclaim_lapsed(&tx, env_id, now_ms)?;
        let current: (String, Option<String>, i64) = tx
            .query_row(
                "SELECT state, lease_owner, lease_epoch FROM work_queue_item \
                 WHERE work_id = ?1 AND environment_id = ?2",
                params![work_id, env_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(storage)?;
        let epoch = if current.0 == "active" && current.1.as_deref() == Some(worker_owner) {
            let (_, epoch) = lease_epoch(current.2, false)?;
            tx.execute(
                "UPDATE work_queue_item SET lease_expires_ms = ?1, lease_refreshed_ms = ?2 \
                 WHERE work_id = ?3 AND environment_id = ?4 AND lease_owner = ?5",
                params![
                    lease_expiry(now_ms, HEARTBEAT_TTL_SECONDS),
                    db_millis(now_ms),
                    work_id,
                    env_id,
                    worker_owner
                ],
            )
            .map_err(storage)?;
            epoch
        } else {
            let active: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM work_queue_item WHERE environment_id = ?1 AND state = 'active'",
                    params![env_id],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            if active > 0 || current.0 != "queued" {
                return Ok(None);
            }
            let (epoch_db, epoch) = lease_epoch(current.2, true)?;
            tx.execute(
                "UPDATE work_queue_item SET state = 'active', started_at = ?1, \
                 lease_owner = ?2, lease_epoch = ?3, lease_expires_ms = ?4, \
                 lease_refreshed_ms = ?5, latest_heartbeat_at = NULL WHERE work_id = ?6",
                params![
                    OBJECT_AT,
                    worker_owner,
                    epoch_db,
                    lease_expiry(now_ms, HEARTBEAT_TTL_SECONDS),
                    db_millis(now_ms),
                    work_id
                ],
            )
            .map_err(storage)?;
            epoch
        };
        tx.commit().map_err(storage)?;
        Ok(Some(SessionWorkLease {
            work_id,
            environment_id: env_id.to_string(),
            session_id: session_id.to_string(),
            owner: worker_owner.to_string(),
            epoch,
            expires_at_unix_ms: now_ms.saturating_add(HEARTBEAT_TTL_SECONDS * 1_000),
        }))
    }

    async fn update_metadata(
        &self,
        env_id: &str,
        wid: &str,
        patch: BTreeMap<String, String>,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        let mut guard = self.conn.lock().map_err(storage)?;
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let Some(mut current) = Self::owned(&tx, env_id, wid)? else {
            return Ok(None);
        };
        current.metadata.extend(patch);
        let metadata_json = metadata_str(&current.metadata);
        tx.execute(
            "UPDATE work_queue_item SET metadata_json = ?1 WHERE work_id = ?2",
            params![metadata_json, wid],
        )
        .map_err(storage)?;
        let item = Self::owned(&tx, env_id, wid)?;
        tx.commit().map_err(storage)?;
        Ok(item)
    }

    async fn stats(&self, env_id: &str, now_ms: u64) -> Result<QueueStats, WorkQueueError> {
        let conn = self.conn.lock().map_err(storage)?;
        let count = |state_clause: &str| -> Result<usize, WorkQueueError> {
            conn.query_row(
                &format!(
                    "SELECT COUNT(*) FROM work_queue_item WHERE environment_id = ?1 AND {state_clause}"
                ),
                params![env_id],
                |r| r.get::<_, i64>(0),
            )
            .map(|count| count as usize)
            .map_err(storage)
        };
        let depth = count("state = 'queued'")?;
        let pending = count("state IN ('starting', 'active', 'stopping')")?;
        // Parity with the in-memory queue: oldest stays set while an item is still
        // processing (queued OR pending), and pollers are counted from the liveness
        // book, not proxied from the active count.
        let has_unfinished = depth > 0 || pending > 0;
        Ok(QueueStats {
            depth,
            pending,
            oldest_queued_at: has_unfinished.then(|| OBJECT_AT.to_string()),
            workers_polling: self.book.workers_polling(env_id, now_ms),
        })
    }

    async fn remove_env(&self, env_id: &str) -> Result<(), WorkQueueError> {
        let mut conn = self.conn.lock().map_err(storage)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let mut stmt = tx
            .prepare("SELECT work_id FROM work_queue_item WHERE environment_id = ?1")
            .map_err(storage)?;
        let ids = stmt
            .query_map(params![env_id], |row| row.get::<_, String>(0))
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;
        drop(stmt);
        tx.execute(
            "DELETE FROM work_queue_item WHERE environment_id = ?1",
            params![env_id],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)?;
        self.book.forget_env(env_id, &ids);
        Ok(())
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
/// the transaction locks the environment rows before the single-active decision.
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

    pub async fn connect_existing(url: &str) -> Result<Self, String> {
        let pool = PgPool::connect(url).await.map_err(|e| e.to_string())?;
        let bundle = work_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|e| e.to_string())?
            .verify_bundle(&bundle)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self {
            pool,
            book: LeaseBook::default(),
        })
    }

    async fn insert(
        &self,
        env_id: &str,
        data_type: &str,
        session_id: Option<&str>,
    ) -> Result<String, WorkQueueError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        // Serialize the portable MAX(seq)+1 allocator. This is infrequent control
        // plane work and avoids a backend-specific sequence while remaining safe
        // across processes.
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

    async fn fetch_owned(
        &self,
        env_id: &str,
        wid: &str,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        Ok(sqlx::query(&format!(
            "SELECT {COLS} FROM work_queue_item WHERE work_id = $1 AND environment_id = $2"
        ))
        .bind(wid)
        .bind(env_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?
        .map(|r| pg_row_to_item(&r)))
    }
}

#[async_trait]
impl WorkQueue for PostgresWorkQueue {
    async fn enqueue_session(
        &self,
        env_id: &str,
        session_id: &str,
    ) -> Result<String, WorkQueueError> {
        self.insert(env_id, "session", Some(session_id)).await
    }

    async fn wake_session(&self, env_id: &str, session_id: &str) -> Result<String, WorkQueueError> {
        let work_id = self.insert(env_id, "session", Some(session_id)).await?;
        sqlx::query(
            "UPDATE work_queue_item SET state = 'queued', acknowledged_at = NULL, \
             latest_heartbeat_at = NULL, started_at = NULL, stop_requested_at = NULL, \
             stopped_at = NULL, lease_owner = NULL, lease_expires_ms = NULL, \
             lease_refreshed_ms = NULL WHERE work_id = $1 AND environment_id = $2 \
             AND data_type = 'session' AND data_id = $3 AND state = 'stopped'",
        )
        .bind(&work_id)
        .bind(env_id)
        .bind(session_id)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(work_id)
    }

    async fn enqueue_healthcheck(&self, env_id: &str) -> Result<String, WorkQueueError> {
        self.insert(env_id, "healthcheck", None).await
    }

    async fn ensure_healthcheck(&self, env_id: &str) -> Result<String, WorkQueueError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        sqlx::query("LOCK TABLE work_queue_item IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        if let Some(id) = sqlx::query_scalar::<_, String>(
            "SELECT work_id FROM work_queue_item WHERE environment_id = $1 AND data_type = 'healthcheck' ORDER BY seq ASC LIMIT 1",
        )
        .bind(env_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        {
            tx.commit().await.map_err(storage)?;
            return Ok(id);
        }
        let next: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(seq), -1) + 1 FROM work_queue_item")
                .fetch_one(&mut *tx)
                .await
                .map_err(storage)?;
        let work_id = format!("work_{next:016}");
        sqlx::query("INSERT INTO work_queue_item (work_id, seq, environment_id, data_type, data_id, metadata_json, state) VALUES ($1, $2, $3, 'healthcheck', $1, '{}', 'queued')")
            .bind(&work_id).bind(next).bind(env_id)
            .execute(&mut *tx).await.map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(work_id)
    }

    async fn list(&self, env_id: &str) -> Result<Vec<WorkItem>, WorkQueueError> {
        Ok(sqlx::query(&format!(
            "SELECT {COLS} FROM work_queue_item WHERE environment_id = $1 ORDER BY seq ASC"
        ))
        .bind(env_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?
        .iter()
        .map(pg_row_to_item)
        .collect())
    }

    async fn get(&self, env_id: &str, wid: &str) -> Result<Option<WorkItem>, WorkQueueError> {
        self.fetch_owned(env_id, wid).await
    }

    async fn claim(
        &self,
        env_id: &str,
        worker_id: &str,
        now_ms: u64,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        self.claim_with_reclaim(env_id, worker_id, worker_id, now_ms, None)
            .await
    }

    async fn claim_with_reclaim(
        &self,
        env_id: &str,
        lease_owner: &str,
        poller_id: &str,
        now_ms: u64,
        age_ms: Option<u64>,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        self.book.record_poll(env_id, poller_id, now_ms);
        if let Some(age) = age_ms.filter(|age| *age <= now_ms) {
            let cutoff = db_millis(now_ms - age);
            sqlx::query(
                "UPDATE work_queue_item SET state = 'queued', lease_owner = NULL, lease_expires_ms = NULL, lease_refreshed_ms = NULL, latest_heartbeat_at = NULL \
                 WHERE environment_id = $1 AND state = 'active' AND lease_refreshed_ms IS NOT NULL AND lease_refreshed_ms <= $2",
            )
            .bind(env_id)
            .bind(cutoff)
            .execute(&self.pool)
            .await
            .map_err(storage)?;
        }
        let mut tx = self.pool.begin().await.map_err(storage)?;
        // Lock the environment's rows before count/select so concurrent pollers
        // cannot both observe zero active work and lease different rows.
        let _: Vec<String> = sqlx::query_scalar(
            "SELECT work_id FROM work_queue_item WHERE environment_id = $1 FOR UPDATE",
        )
        .bind(env_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
        sqlx::query(
            "UPDATE work_queue_item \
             SET state = 'queued', lease_owner = NULL, lease_expires_ms = NULL, \
                 lease_refreshed_ms = NULL, latest_heartbeat_at = NULL \
             WHERE environment_id = $1 AND state = 'active' \
               AND (lease_expires_ms IS NULL OR lease_expires_ms <= $2)",
        )
        .bind(env_id)
        .bind(db_millis(now_ms))
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        // Single active lease per environment (the open-tier single-worker cap).
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM work_queue_item WHERE environment_id = $1 AND state = 'active'",
        )
        .bind(env_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage)?;
        if active > 0 {
            return Ok(None);
        }
        // Lease the oldest queued item, locking the chosen row.
        let wid: Option<String> = sqlx::query_scalar(
            "SELECT work_id FROM work_queue_item \
             WHERE environment_id = $1 AND state = 'queued' \
             ORDER BY seq ASC LIMIT 1",
        )
        .bind(env_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some(wid) = wid else {
            return Ok(None);
        };
        sqlx::query(
            "UPDATE work_queue_item \
             SET state = 'active', started_at = $1, lease_owner = $2, \
                 lease_epoch = lease_epoch + 1, lease_expires_ms = $3, \
                 lease_refreshed_ms = $4, latest_heartbeat_at = NULL \
             WHERE work_id = $5",
        )
        .bind(OBJECT_AT)
        .bind(lease_owner)
        .bind(lease_expiry(now_ms, HEARTBEAT_TTL_SECONDS))
        .bind(db_millis(now_ms))
        .bind(&wid)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        self.fetch_owned(env_id, &wid).await
    }

    async fn ack(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let current: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT state, lease_owner FROM work_queue_item WHERE work_id = $1 \
             AND environment_id = $2 FOR UPDATE",
        )
        .bind(wid)
        .bind(env_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some((state, owner)) = current else {
            return Ok(WorkMutationResult::NotFound);
        };
        if owner.as_deref() != Some(worker_id) {
            return Ok(WorkMutationResult::PreconditionFailed);
        }
        let next = awaken_session_contract::work_queue::WorkState::from_wire(&state)
            .unwrap_or(awaken_session_contract::work_queue::WorkState::Stopped)
            .after_ack()
            .as_str();
        sqlx::query(
            "UPDATE work_queue_item SET acknowledged_at = $1, state = $2 \
             WHERE work_id = $3 AND environment_id = $4",
        )
        .bind(OBJECT_AT)
        .bind(next)
        .bind(wid)
        .bind(env_id)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(self
            .fetch_owned(env_id, wid)
            .await?
            .map(WorkMutationResult::accepted)
            .unwrap_or(WorkMutationResult::NotFound))
    }

    async fn heartbeat(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        now_ms: u64,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, WorkQueueError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let current = sqlx::query(&format!(
            "SELECT {COLS} FROM work_queue_item \
             WHERE work_id = $1 AND environment_id = $2 FOR UPDATE"
        ))
        .bind(wid)
        .bind(env_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| pg_row_to_item(&row));
        let Some(current) = current else {
            return Ok(HeartbeatResult::NotFound);
        };
        let owner: Option<String> = sqlx::query_scalar(
            "SELECT lease_owner FROM work_queue_item \
             WHERE work_id = $1 AND environment_id = $2",
        )
        .bind(wid)
        .bind(env_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage)?;
        if owner.as_deref() != Some(worker_id) {
            return Ok(HeartbeatResult::PreconditionFailed);
        }
        if !heartbeat
            .condition
            .permits(current.latest_heartbeat_at.as_deref())
        {
            return Ok(HeartbeatResult::PreconditionFailed);
        }
        let extended = current.state.can_extend_lease();
        let ttl_seconds = effective_ttl_seconds(heartbeat.desired_ttl_seconds);
        let last_heartbeat = heartbeat_at(now_ms, current.latest_heartbeat_at.as_deref());
        if extended {
            sqlx::query(
                "UPDATE work_queue_item SET latest_heartbeat_at = $1, lease_expires_ms = $2, lease_refreshed_ms = $3 \
                 WHERE work_id = $4 AND environment_id = $5 AND state = 'active' \
                   AND lease_owner = $6",
            )
            .bind(&last_heartbeat)
            .bind(lease_expiry(now_ms, ttl_seconds))
            .bind(db_millis(now_ms))
            .bind(wid)
            .bind(env_id)
            .bind(worker_id)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        }
        tx.commit().await.map_err(storage)?;
        Ok(HeartbeatResult::Accepted(LeaseReceipt {
            last_heartbeat,
            lease_extended: extended,
            state: current.state.as_str(),
            ttl_seconds,
        }))
    }

    async fn stop(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let owner: Option<Option<String>> = sqlx::query_scalar(
            "SELECT lease_owner FROM work_queue_item WHERE work_id = $1 \
             AND environment_id = $2 FOR UPDATE",
        )
        .bind(wid)
        .bind(env_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some(owner) = owner else {
            return Ok(WorkMutationResult::NotFound);
        };
        if owner.as_deref() != Some(worker_id) {
            return Ok(WorkMutationResult::PreconditionFailed);
        }
        sqlx::query(
            "UPDATE work_queue_item SET stop_requested_at = $1, stopped_at = $1, \
             state = 'stopped', lease_owner = NULL, lease_expires_ms = NULL, \
             lease_refreshed_ms = NULL \
             WHERE work_id = $2 AND environment_id = $3",
        )
        .bind(OBJECT_AT)
        .bind(wid)
        .bind(env_id)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(self
            .fetch_owned(env_id, wid)
            .await?
            .map(WorkMutationResult::accepted)
            .unwrap_or(WorkMutationResult::NotFound))
    }

    async fn release_owner(&self, worker_owner: &str) -> Result<usize, WorkQueueError> {
        let released = sqlx::query(
            "UPDATE work_queue_item SET stop_requested_at = $1, stopped_at = $1, \
             state = 'stopped', lease_owner = NULL, lease_expires_ms = NULL, \
             lease_refreshed_ms = NULL WHERE data_type = 'session' \
             AND state = 'active' AND lease_owner = $2",
        )
        .bind(OBJECT_AT)
        .bind(worker_owner)
        .execute(&self.pool)
        .await
        .map_err(storage)?
        .rows_affected();
        usize::try_from(released)
            .map_err(|_| WorkQueueError::Storage("released row count overflow".into()))
    }

    async fn retire_session(
        &self,
        env_id: &str,
        session_id: &str,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        let work_id: Option<String> = sqlx::query_scalar(
            "SELECT work_id FROM work_queue_item WHERE environment_id = $1 \
             AND data_type = 'session' AND data_id = $2 ORDER BY seq ASC LIMIT 1",
        )
        .bind(env_id)
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        let Some(work_id) = work_id else {
            return Ok(None);
        };
        sqlx::query(
            "UPDATE work_queue_item SET stop_requested_at = $1, stopped_at = $1, \
             state = 'stopped', lease_owner = NULL, lease_expires_ms = NULL, \
             lease_refreshed_ms = NULL WHERE work_id = $2 AND environment_id = $3",
        )
        .bind(OBJECT_AT)
        .bind(&work_id)
        .bind(env_id)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        self.fetch_owned(env_id, &work_id).await
    }

    async fn acquire_session(
        &self,
        env_id: &str,
        session_id: &str,
        worker_owner: &str,
        now_ms: u64,
    ) -> Result<Option<SessionWorkLease>, WorkQueueError> {
        let work_id = self.insert(env_id, "session", Some(session_id)).await?;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let _: Vec<String> = sqlx::query_scalar(
            "SELECT work_id FROM work_queue_item WHERE environment_id = $1 FOR UPDATE",
        )
        .bind(env_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
        sqlx::query(
            "UPDATE work_queue_item SET state = 'queued', lease_owner = NULL, \
             lease_expires_ms = NULL, lease_refreshed_ms = NULL, latest_heartbeat_at = NULL \
             WHERE environment_id = $1 AND state = 'active' \
             AND (lease_expires_ms IS NULL OR lease_expires_ms <= $2)",
        )
        .bind(env_id)
        .bind(db_millis(now_ms))
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        let current: (String, Option<String>, i64) = sqlx::query_as(
            "SELECT state, lease_owner, lease_epoch FROM work_queue_item \
             WHERE work_id = $1 AND environment_id = $2",
        )
        .bind(&work_id)
        .bind(env_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage)?;
        let epoch = if current.0 == "active" && current.1.as_deref() == Some(worker_owner) {
            let (_, epoch) = lease_epoch(current.2, false)?;
            sqlx::query(
                "UPDATE work_queue_item SET lease_expires_ms = $1, lease_refreshed_ms = $2 \
                 WHERE work_id = $3 AND environment_id = $4 AND lease_owner = $5",
            )
            .bind(lease_expiry(now_ms, HEARTBEAT_TTL_SECONDS))
            .bind(db_millis(now_ms))
            .bind(&work_id)
            .bind(env_id)
            .bind(worker_owner)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
            epoch
        } else {
            let active: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM work_queue_item WHERE environment_id = $1 AND state = 'active'",
            )
            .bind(env_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(storage)?;
            if active > 0 || current.0 != "queued" {
                return Ok(None);
            }
            let (epoch_db, epoch) = lease_epoch(current.2, true)?;
            sqlx::query(
                "UPDATE work_queue_item SET state = 'active', started_at = $1, \
                 lease_owner = $2, lease_epoch = $3, lease_expires_ms = $4, \
                 lease_refreshed_ms = $5, latest_heartbeat_at = NULL WHERE work_id = $6",
            )
            .bind(OBJECT_AT)
            .bind(worker_owner)
            .bind(epoch_db)
            .bind(lease_expiry(now_ms, HEARTBEAT_TTL_SECONDS))
            .bind(db_millis(now_ms))
            .bind(&work_id)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
            epoch
        };
        tx.commit().await.map_err(storage)?;
        Ok(Some(SessionWorkLease {
            work_id,
            environment_id: env_id.to_string(),
            session_id: session_id.to_string(),
            owner: worker_owner.to_string(),
            epoch,
            expires_at_unix_ms: now_ms.saturating_add(HEARTBEAT_TTL_SECONDS * 1_000),
        }))
    }

    async fn update_metadata(
        &self,
        env_id: &str,
        wid: &str,
        patch: BTreeMap<String, String>,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        let Some(mut current) = self.fetch_owned(env_id, wid).await? else {
            return Ok(None);
        };
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
        .map_err(storage)?;
        self.fetch_owned(env_id, wid).await
    }

    async fn stats(&self, env_id: &str, now_ms: u64) -> Result<QueueStats, WorkQueueError> {
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
                .map_err(storage)
                .map(|count| count as usize)
            }
        };
        let depth = count("state = 'queued'").await?;
        let pending = count("state IN ('starting', 'active', 'stopping')").await?;
        // Parity with the in-memory queue: oldest persists while processing, and
        // pollers come from the liveness book (not the active-count proxy).
        let has_unfinished = depth > 0 || pending > 0;
        Ok(QueueStats {
            depth,
            pending,
            oldest_queued_at: has_unfinished.then(|| OBJECT_AT.to_string()),
            workers_polling: self.book.workers_polling(env_id, now_ms),
        })
    }

    async fn remove_env(&self, env_id: &str) -> Result<(), WorkQueueError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        sqlx::query("LOCK TABLE work_queue_item IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        let ids = sqlx::query_scalar::<_, String>(
            "SELECT work_id FROM work_queue_item WHERE environment_id = $1",
        )
        .bind(env_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
        sqlx::query("DELETE FROM work_queue_item WHERE environment_id = $1")
            .bind(env_id)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        self.book.forget_env(env_id, &ids);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::LEASE_TTL_MS;
    use super::*;
    use awaken_session_contract::work_queue::{WorkPayload, WorkState};

    fn q() -> SqliteWorkQueue {
        SqliteWorkQueue::open_in_memory().unwrap()
    }

    #[test]
    fn lease_epoch_conversion_fails_closed_at_corrupt_and_exhausted_boundaries() {
        // Cause/effect graph: C1 persisted epoch is negative/corrupt; C2 epoch
        // is the largest SQL BIGINT and a new owner requires advancement; C3
        // the same largest epoch is only being read/renewed. Effects: E1/C1 and
        // E1/C2 return typed Storage errors without zeroing or reusing a fence;
        // E2/C3 preserves the current fence. Decision rows: X1=C1->E1,
        // X2=C2->E1, X3=C3->E2.
        assert!(
            matches!(lease_epoch(-1, false), Err(WorkQueueError::Storage(_))),
            "X1"
        );
        assert!(
            matches!(lease_epoch(i64::MAX, true), Err(WorkQueueError::Storage(_))),
            "X2"
        );
        assert_eq!(
            lease_epoch(i64::MAX, false).expect("X3"),
            (i64::MAX, i64::MAX as u64),
            "X3"
        );
    }

    #[tokio::test]
    async fn sqlite_storage_outage_is_typed_for_every_queue_operation() {
        // Cause/effect graph: C1 the queue schema is available; C2 the backing
        // schema becomes unavailable; C3 an operation would otherwise return
        // None/empty. Effects: E1 C1 preserves ordinary success/not-found
        // semantics; E2 C2 returns WorkQueueError::Storage for every read and
        // mutation; E3 no C2 path panics or degrades into C3.
        //
        // | Rule | storage | logical item | operation result |
        // | S1 | healthy | present/absent | success/None |
        // | S2 | unavailable | any | Storage error |
        // The ordinary backend and conformance tests own S1. This test owns S2
        // across the complete port after invalidating the SQLite schema.
        let q = q();
        q.conn
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE work_queue_item")
            .unwrap();
        let is_storage = |result: Result<(), WorkQueueError>| {
            assert!(matches!(result, Err(WorkQueueError::Storage(_))));
        };

        is_storage(q.enqueue_session("env", "session").await.map(|_| ()));
        is_storage(q.enqueue_healthcheck("env").await.map(|_| ()));
        is_storage(q.ensure_healthcheck("env").await.map(|_| ()));
        is_storage(q.list("env").await.map(|_| ()));
        is_storage(q.get("env", "work").await.map(|_| ()));
        is_storage(q.claim("env", "worker", 0).await.map(|_| ()));
        is_storage(
            q.claim_with_reclaim("env", "worker", "worker", 1, Some(1))
                .await
                .map(|_| ()),
        );
        is_storage(q.ack("env", "work", "worker").await.map(|_| ()));
        is_storage(
            q.heartbeat("env", "work", "worker", 0, LeaseHeartbeat::unconditional())
                .await
                .map(|_| ()),
        );
        is_storage(q.stop("env", "work", "worker").await.map(|_| ()));
        is_storage(
            q.update_metadata("env", "work", BTreeMap::new())
                .await
                .map(|_| ()),
        );
        is_storage(q.stats("env", 0).await.map(|_| ()));
        is_storage(q.remove_env("env").await);
    }

    #[tokio::test]
    async fn healthcheck_seed_self_references_and_survives_reload() {
        let q = q();
        let id = q.enqueue_healthcheck("env_a").await.expect("enqueue");
        let w = q.get("env_a", &id).await.expect("get").expect("seeded");
        assert_eq!(w.state, WorkState::Queued);
        assert!(matches!(w.data, WorkPayload::HealthCheck { id: ref d } if *d == id));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_session_dispatch_replay_has_one_sqlite_row() {
        // Cause/effect graph: C1 one durable queue; C2 identical Environment and
        // Session coordinates; C3 concurrent replay. Effects: E1 every caller
        // observes the same canonical id and E2 exactly one row exists. The
        // differing-coordinate rules are covered by the backend conformance table.
        //
        // | Rule | queue | coordinates | concurrency | ids | rows |
        // | C1 | shared | identical | 32 callers | one | one |
        let queue = Arc::new(q());
        let mut tasks = Vec::new();
        for _ in 0..32 {
            let queue = queue.clone();
            tasks.push(tokio::spawn(async move {
                queue
                    .enqueue_session("env", "session")
                    .await
                    .expect("C1 enqueue")
            }));
        }
        let mut ids = Vec::new();
        for task in tasks {
            ids.push(task.await.expect("C1 join"));
        }
        assert!(ids.iter().all(|id| id == &ids[0]), "C1/E1");
        assert_eq!(queue.list("env").await.unwrap().len(), 1, "C1/E2");
    }

    #[tokio::test]
    async fn claim_leases_oldest_and_caps_at_one_active() {
        let q = q();
        let w1 = q.enqueue_session("env_a", "s1").await.expect("enqueue");
        let _w2 = q.enqueue_session("env_a", "s2").await.expect("enqueue");
        let leased = q
            .claim("env_a", "w", 0)
            .await
            .expect("claim query")
            .expect("leases oldest");
        assert_eq!(leased.id, w1);
        assert_eq!(leased.state, WorkState::Active);
        assert!(
            q.claim("env_a", "w", 0).await.expect("claim").is_none(),
            "single active lease"
        );
        q.stop("env_a", &w1, "w")
            .await
            .expect("stop query")
            .into_item()
            .expect("stop");
        assert!(
            q.claim("env_a", "w", 0).await.expect("claim").is_some(),
            "next lease after stop"
        );
    }

    #[tokio::test]
    async fn durable_reclaims_an_expired_lease_on_the_next_poll() {
        let q = q();
        let w1 = q.enqueue_session("env_a", "s1").await.expect("enqueue");
        assert_eq!(
            q.claim("env_a", "a", 0)
                .await
                .expect("claim query")
                .expect("lease")
                .id,
            w1
        );
        // Live lease caps; a lapsed lease is reclaimed and re-leased.
        assert!(
            q.claim("env_a", "b", 1_000).await.expect("claim").is_none(),
            "live lease caps"
        );
        let reclaimed = q
            .claim("env_a", "b", LEASE_TTL_MS + 1)
            .await
            .expect("claim query")
            .expect("expired lease reclaimed");
        assert_eq!(reclaimed.id, w1);
        assert_eq!(reclaimed.state, WorkState::Active);
    }

    #[tokio::test]
    async fn sqlite_honors_requested_reclaim_age_before_lease_expiry() {
        let q = q();
        let w1 = q.enqueue_session("env_a", "s1").await.expect("enqueue");
        assert_eq!(
            q.claim("env_a", "a", 0)
                .await
                .expect("claim query")
                .expect("lease")
                .id,
            w1
        );
        let reclaimed = q
            .claim_with_reclaim("env_a", "b", "b", 1_001, Some(1_000))
            .await
            .expect("claim query")
            .expect("requested reclaim age reclaims the active lease");
        assert_eq!(reclaimed.id, w1);
        assert_eq!(reclaimed.state, WorkState::Active);
    }

    #[tokio::test]
    async fn sqlite_lease_survives_process_store_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "awaken-work-lease-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("queue.db");
        let path = path.to_str().unwrap();
        {
            let q = SqliteWorkQueue::open(path).expect("open first store");
            q.enqueue_session("env", "session").await.expect("enqueue");
            assert!(
                q.claim("env", "worker-a", 0)
                    .await
                    .expect("claim")
                    .is_some()
            );
        }
        {
            let reopened = SqliteWorkQueue::open(path).expect("reopen store");
            assert!(
                reopened
                    .claim("env", "worker-b", LEASE_TTL_MS - 1)
                    .await
                    .expect("claim")
                    .is_none(),
                "a restart must not erase a live lease"
            );
            assert!(
                reopened
                    .claim("env", "worker-b", LEASE_TTL_MS)
                    .await
                    .expect("claim")
                    .is_some(),
                "the durable lease is reclaimable at its exact expiry"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn ack_heartbeat_and_membership_match_the_in_memory_contract() {
        let q = q();
        let id = q.enqueue_session("env_a", "s1").await.expect("enqueue");
        q.claim("env_a", "worker", 0)
            .await
            .expect("claim query")
            .expect("claim");
        assert_eq!(
            q.ack("env_a", &id, "worker")
                .await
                .expect("ack query")
                .into_item()
                .expect("ack")
                .state,
            WorkState::Active
        );
        assert!(
            q.heartbeat("env_a", &id, "worker", 0, LeaseHeartbeat::unconditional())
                .await
                .expect("heartbeat")
                .into_receipt()
                .expect("hb")
                .lease_extended
        );
        // Wrong env → none across the board.
        assert!(q.get("env_b", &id).await.expect("get").is_none());
        assert!(
            q.ack("env_b", &id, "worker")
                .await
                .expect("ack")
                .is_not_found()
        );
        assert!(
            q.heartbeat("env_b", &id, "worker", 0, LeaseHeartbeat::unconditional())
                .await
                .expect("heartbeat")
                .is_not_found()
        );
        assert!(
            q.stop("env_b", &id, "worker")
                .await
                .expect("stop")
                .is_not_found()
        );
    }

    #[tokio::test]
    async fn stats_and_metadata_and_remove_env() {
        let q = q();
        q.enqueue_healthcheck("env_a").await.expect("enqueue");
        let s = q.enqueue_session("env_a", "s1").await.expect("enqueue");
        let st = q.stats("env_a", 0).await.expect("stats");
        assert_eq!((st.depth, st.pending, st.workers_polling), (2, 0, 0));
        q.claim("env_a", "w1", 0).await.expect("claim");
        let st = q.stats("env_a", 0).await.expect("stats");
        assert_eq!((st.depth, st.pending, st.workers_polling), (1, 1, 1));
        let patch = BTreeMap::from([("k".to_string(), "v".to_string())]);
        let up = q
            .update_metadata("env_a", &s, patch)
            .await
            .expect("update query")
            .expect("patch");
        assert_eq!(up.metadata.get("k").map(String::as_str), Some("v"));
        q.remove_env("env_a").await.unwrap();
        assert!(
            q.list("env_a").await.expect("list").is_empty(),
            "env delete purges work"
        );
    }

    #[tokio::test]
    async fn list_is_enqueue_ordered() {
        let q = q();
        let a = q.enqueue_session("e", "s1").await.expect("enqueue");
        let b = q.enqueue_session("e", "s2").await.expect("enqueue");
        let ids: Vec<String> = q
            .list("e")
            .await
            .expect("list")
            .into_iter()
            .map(|w| w.id)
            .collect();
        assert_eq!(ids, vec![a, b]);
    }

    #[tokio::test]
    async fn file_open_and_pg_connect_entry_points() {
        // The file-backed `open` (not just `open_in_memory`).
        let dir = std::env::temp_dir().join(format!("wq-cov-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wq.db");
        let q = SqliteWorkQueue::open(path.to_str().unwrap()).expect("open file db");
        let id = q.enqueue_session("e", "s").await.expect("enqueue");
        assert!(q.get("e", &id).await.expect("get").is_some());
        std::fs::remove_dir_all(&dir).ok();
        // The `connect(url)` path over a live Postgres (skips when unreachable).
        if let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL")
            && let Ok(q) = PostgresWorkQueue::connect(&url).await
        {
            let id = q.enqueue_healthcheck("cov_env").await.expect("enqueue");
            assert!(q.get("cov_env", &id).await.expect("get").is_some());
            q.remove_env("cov_env").await.unwrap();
        }
    }

    /// Live Postgres parity over the same portable bundle. Skips when no Postgres is
    /// reachable (`AWAKEN_TEST_DATABASE_URL`), isolated in its own schema.
    #[tokio::test]
    async fn postgres_parity_over_a_live_db() {
        use sqlx::Executor;
        use sqlx::postgres::{PgPool, PgPoolOptions};

        // Aligned with the memory/session/skill store pg tests' default URL/port
        // (was the divergent `:5455/cov`) so one reachable Postgres runs them all.
        let url = std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
        });
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

        let hc = q.enqueue_healthcheck("env_a").await.expect("enqueue");
        let w1 = q.enqueue_session("env_a", "s1").await.expect("enqueue");
        let w2 = q.enqueue_session("env_a", "s2").await.expect("enqueue");
        // list is enqueue-ordered
        let ids: Vec<String> = q
            .list("env_a")
            .await
            .expect("list")
            .into_iter()
            .map(|w| w.id)
            .collect();
        assert_eq!(ids, vec![hc.clone(), w1.clone(), w2]);
        // claim leases the oldest + single-active cap
        assert_eq!(
            q.claim("env_a", "w", 0)
                .await
                .expect("claim query")
                .expect("lease")
                .id,
            hc
        );
        assert!(
            q.claim("env_a", "w", 0).await.expect("claim").is_none(),
            "single active lease"
        );
        // The active healthcheck owner cannot acknowledge another queued item.
        assert!(matches!(
            q.ack("env_a", &w1, "w").await.expect("ack query"),
            WorkMutationResult::PreconditionFailed
        ));
        assert!(matches!(
            q.heartbeat("env_a", &w1, "w", 0, LeaseHeartbeat::unconditional())
                .await
                .expect("heartbeat"),
            HeartbeatResult::PreconditionFailed
        ));
        assert!(
            q.get("env_b", &w1).await.expect("get").is_none(),
            "wrong env → none"
        );
        assert!(
            q.heartbeat("env_b", &w1, "w", 0, LeaseHeartbeat::unconditional())
                .await
                .expect("heartbeat")
                .is_not_found()
        );
        // stats reflect the active healthcheck and queued Session work.
        let st = q.stats("env_a", 0).await.expect("stats");
        assert!(st.pending >= 1 && st.workers_polling == 1);
        // metadata + stop + remove_env
        let patch = BTreeMap::from([("k".to_string(), "v".to_string())]);
        assert_eq!(
            q.update_metadata("env_a", &w1, patch)
                .await
                .expect("update query")
                .expect("md")
                .metadata
                .get("k")
                .map(String::as_str),
            Some("v")
        );
        assert!(q.stop("env_a", &hc, "w").await.expect("stop").is_accepted());
        q.remove_env("env_a").await.unwrap();
        assert!(
            q.list("env_a").await.expect("list").is_empty(),
            "remove_env purges"
        );
    }

    /// The environment-row lock in pg `claim` serializes the count-and-lease decision:
    /// under real concurrent contention, the single-active-lease cap holds —
    /// with one queued item and N workers claiming at once, exactly ONE wins the lease
    /// and the rest get `None` after they observe the winner's active row.
    /// Distinct pool connections per task make the transactions genuinely concurrent.
    /// Skips when no Postgres is reachable (`AWAKEN_TEST_DATABASE_URL`), own schema.
    #[tokio::test]
    async fn postgres_concurrent_claim_yields_a_single_active_lease() {
        use std::sync::Arc;

        use sqlx::Executor;
        use sqlx::postgres::{PgPool, PgPoolOptions};

        let url = std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
        });
        let Ok(admin) = PgPool::connect(&url).await else {
            println!("[skip] no Postgres reachable");
            return;
        };
        let _ = admin
            .execute("DROP SCHEMA IF EXISTS t_work_queue_claim CASCADE")
            .await;
        admin
            .execute("CREATE SCHEMA t_work_queue_claim")
            .await
            .expect("schema");
        admin.close().await;
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .after_connect(|conn, _| {
                Box::pin(async move {
                    conn.execute("SET search_path = t_work_queue_claim").await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .expect("schema pool");
        let q = Arc::new(PostgresWorkQueue::with_pool(pool).await.expect("store"));

        // One queued item, N workers race to claim it concurrently.
        q.enqueue_session("env_a", "s1").await.expect("enqueue");
        let mut handles = Vec::new();
        for w in 0..8u32 {
            let q = q.clone();
            handles.push(tokio::spawn(async move {
                q.claim("env_a", &format!("worker_{w}"), 0).await
            }));
        }
        let mut winners = 0;
        for h in handles {
            if h.await.unwrap().expect("claim").is_some() {
                winners += 1;
            }
        }
        assert_eq!(
            winners, 1,
            "exactly one worker wins the lease under concurrent contention"
        );
        // And the store agrees: exactly one active row (pending == 1), depth drained.
        let st = q.stats("env_a", 0).await.expect("stats");
        assert_eq!(
            (st.depth, st.pending),
            (0, 1),
            "the single-active cap holds: one active lease, nothing left queued"
        );
        q.remove_env("env_a").await.unwrap();
    }
}
