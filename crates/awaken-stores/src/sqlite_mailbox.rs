//! SQLite-backed implementation of [`MailboxStore`].

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_server_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_server_contract::contract::mailbox::{
    MailboxInterrupt, MailboxInterruptDetails, MailboxStore, RunDispatch, RunDispatchParts,
    RunDispatchResult, RunDispatchStatus,
};
use awaken_server_contract::contract::storage::StorageError;
use rusqlite::{Connection, Row, params};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::mailbox_state;

// ── SqliteMailboxStore ─────────────────────────────────────────────

/// SQLite-backed persistent mailbox store.
///
/// Uses WAL mode for concurrent read access. All writes are serialized
/// through `tokio::sync::Mutex` wrapping a single `rusqlite::Connection`.
pub struct SqliteMailboxStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteMailboxStore {
    /// Open (or create) a SQLite database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let conn =
            Connection::open(path).map_err(|e| StorageError::Io(format!("sqlite open: {e}")))?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        // Block on table creation — called once at startup.
        let rt_conn = store.conn.clone();
        // We cannot use async here, so access the lock via try_lock
        // since no one else holds it yet.
        {
            let guard = rt_conn.try_lock().expect("no contention at construction");
            Self::create_tables(&guard)?;
        }
        Ok(store)
    }

    /// Open an in-memory SQLite database (useful for tests).
    pub fn open_memory() -> Result<Self, StorageError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| StorageError::Io(format!("sqlite open_memory: {e}")))?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        {
            let guard = store
                .conn
                .try_lock()
                .expect("no contention at construction");
            Self::create_tables(&guard)?;
        }
        Ok(store)
    }

    fn create_tables(conn: &Connection) -> Result<(), StorageError> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .map_err(|e| StorageError::Io(format!("pragma: {e}")))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS run_dispatches (
                dispatch_id         TEXT PRIMARY KEY,
                thread_id           TEXT NOT NULL,
                run_id              TEXT NOT NULL,
                priority            INTEGER NOT NULL DEFAULT 128,
                dedupe_key          TEXT,
                dispatch_epoch      INTEGER NOT NULL DEFAULT 0,
                status              TEXT NOT NULL DEFAULT 'Queued',
                available_at        INTEGER NOT NULL DEFAULT 0,
                attempt_count       INTEGER NOT NULL DEFAULT 0,
                max_attempts        INTEGER NOT NULL DEFAULT 5,
                last_error          TEXT,
                claim_token         TEXT,
                claimed_by          TEXT,
                lease_until         INTEGER,
                dispatch_instance_id TEXT,
                run_status          TEXT,
                termination         TEXT,
                run_response        TEXT,
                run_error           TEXT,
                completed_at        INTEGER,
                created_at          INTEGER NOT NULL,
                updated_at          INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_run_dispatches_thread_status
                ON run_dispatches (thread_id, status);

            CREATE INDEX IF NOT EXISTS idx_run_dispatches_dedupe
                ON run_dispatches (thread_id, dedupe_key)
                WHERE dedupe_key IS NOT NULL;

            CREATE INDEX IF NOT EXISTS idx_run_dispatches_lease
                ON run_dispatches (status, lease_until)
                WHERE status = 'Claimed';

            CREATE TABLE IF NOT EXISTS thread_dispatch_epochs (
                thread_id         TEXT PRIMARY KEY,
                current_epoch     INTEGER NOT NULL DEFAULT 0
            );",
        )
        .map_err(|e| StorageError::Io(format!("create tables: {e}")))?;
        Ok(())
    }
}

// ── Serialization helpers ──────────────────────────────────────────

fn status_to_str(s: RunDispatchStatus) -> &'static str {
    match s {
        RunDispatchStatus::Queued => "Queued",
        RunDispatchStatus::Claimed => "Claimed",
        RunDispatchStatus::Acked => "Acked",
        RunDispatchStatus::Cancelled => "Cancelled",
        RunDispatchStatus::Superseded => "Superseded",
        RunDispatchStatus::DeadLetter => "DeadLetter",
    }
}

fn str_to_status(s: &str) -> Result<RunDispatchStatus, StorageError> {
    match s {
        "Queued" => Ok(RunDispatchStatus::Queued),
        "Claimed" => Ok(RunDispatchStatus::Claimed),
        "Acked" => Ok(RunDispatchStatus::Acked),
        "Cancelled" => Ok(RunDispatchStatus::Cancelled),
        "Superseded" => Ok(RunDispatchStatus::Superseded),
        "DeadLetter" => Ok(RunDispatchStatus::DeadLetter),
        other => Err(StorageError::Io(format!(
            "unknown RunDispatchStatus: {other}"
        ))),
    }
}

fn run_status_to_str(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Created => "created",
        RunStatus::Running => "running",
        RunStatus::Waiting => "waiting",
        RunStatus::Done => "done",
    }
}

fn str_to_run_status(s: &str) -> Result<RunStatus, StorageError> {
    match s {
        "created" => Ok(RunStatus::Created),
        "running" => Ok(RunStatus::Running),
        "waiting" => Ok(RunStatus::Waiting),
        "done" => Ok(RunStatus::Done),
        other => Err(StorageError::Io(format!("unknown RunStatus: {other}"))),
    }
}

fn termination_to_json(
    termination: Option<&TerminationReason>,
) -> Result<Option<String>, StorageError> {
    termination
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| StorageError::Io(format!("serialize termination: {e}")))
}

fn row_to_dispatch(row: &Row<'_>) -> Result<RunDispatch, rusqlite::Error> {
    let status_str: String = row.get("status")?;
    let status = str_to_status(&status_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            11,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{e:?}"),
            )),
        )
    })?;

    let run_status: Option<RunStatus> = {
        let value: Option<String> = row.get("run_status")?;
        value
            .map(|s| {
                str_to_run_status(&s).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("{e:?}"),
                        )),
                    )
                })
            })
            .transpose()?
    };
    let termination: Option<TerminationReason> = {
        let value: Option<String> = row.get("termination")?;
        value
            .map(|s| serde_json::from_str(&s))
            .transpose()
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?
    };

    let priority_i64: i64 = row.get("priority")?;
    let dispatch_epoch_i64: i64 = row.get("dispatch_epoch")?;
    let available_at_i64: i64 = row.get("available_at")?;
    let attempt_count_i64: i64 = row.get("attempt_count")?;
    let max_attempts_i64: i64 = row.get("max_attempts")?;
    let lease_until: Option<i64> = row.get("lease_until")?;
    let completed_at: Option<i64> = row.get("completed_at")?;
    let created_at_i64: i64 = row.get("created_at")?;
    let updated_at_i64: i64 = row.get("updated_at")?;

    RunDispatch::from_persisted_parts(RunDispatchParts {
        dispatch_id: row.get("dispatch_id")?,
        thread_id: row.get("thread_id")?,
        run_id: row.get("run_id")?,
        priority: priority_i64 as u8,
        dedupe_key: row.get("dedupe_key")?,
        dispatch_epoch: dispatch_epoch_i64 as u64,
        status,
        available_at: available_at_i64 as u64,
        attempt_count: attempt_count_i64 as u32,
        max_attempts: max_attempts_i64 as u32,
        last_error: row.get("last_error")?,
        claim_token: row.get("claim_token")?,
        claimed_by: row.get("claimed_by")?,
        lease_until: lease_until.map(|v| v as u64),
        dispatch_instance_id: row.get("dispatch_instance_id")?,
        run_status,
        termination,
        run_response: row.get("run_response")?,
        run_error: row.get("run_error")?,
        completed_at: completed_at.map(|v| v as u64),
        created_at: created_at_i64 as u64,
        updated_at: updated_at_i64 as u64,
    })
    .map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn current_epoch_for_conn(conn: &Connection, thread_id: &str) -> Result<u64, StorageError> {
    let epoch = conn
        .prepare_cached("SELECT current_epoch FROM thread_dispatch_epochs WHERE thread_id = ?1")
        .map_err(|e| StorageError::Io(format!("prepare current dispatch_epoch: {e}")))?
        .query_row(params![thread_id], |row| row.get::<_, i64>(0))
        .optional()
        .map_err(|e| StorageError::Io(format!("current dispatch_epoch select: {e}")))?;
    Ok(epoch.unwrap_or(0) as u64)
}

fn supersede_claimed_loaded(
    conn: &Connection,
    dispatch: &RunDispatch,
    claim_token: &str,
    now: u64,
    reason: &str,
) -> Result<Option<RunDispatch>, StorageError> {
    if dispatch.status() != RunDispatchStatus::Claimed {
        return Ok(None);
    }
    if dispatch.claim_token() != Some(claim_token) {
        return Err(StorageError::VersionConflict {
            expected: 0,
            actual: 1,
        });
    }
    let current_epoch = current_epoch_for_conn(conn, &dispatch.thread_id())?;
    let terminal_epoch = dispatch.dispatch_epoch().max(current_epoch);
    let changed = conn
        .execute(
            "UPDATE run_dispatches
             SET status = 'Superseded',
                 dispatch_epoch = ?1,
                 last_error = ?2,
                 claim_token = NULL,
                 claimed_by = NULL,
                 lease_until = NULL,
                 completed_at = ?3,
                 updated_at = ?4
             WHERE dispatch_id = ?5
               AND status = 'Claimed'
               AND claim_token = ?6",
            params![
                terminal_epoch as i64,
                reason,
                now as i64,
                now as i64,
                &dispatch.dispatch_id(),
                claim_token
            ],
        )
        .map_err(|e| StorageError::Io(format!("supersede claimed update: {e}")))?;
    if changed == 0 {
        return Ok(None);
    }
    conn.prepare_cached("SELECT * FROM run_dispatches WHERE dispatch_id = ?1")
        .map_err(|e| StorageError::Io(format!("prepare supersede claimed reload: {e}")))?
        .query_row(params![&dispatch.dispatch_id()], row_to_dispatch)
        .optional()
        .map_err(|e| StorageError::Io(format!("supersede claimed reload: {e}")))
}

fn supersede_stale_queued_for_thread(
    conn: &Connection,
    thread_id: &str,
    now: u64,
) -> Result<usize, StorageError> {
    let current_epoch = current_epoch_for_conn(conn, thread_id)?;
    conn.execute(
        "UPDATE run_dispatches
         SET status = 'Superseded',
             dispatch_epoch = ?1,
             last_error = ?2,
             claim_token = NULL,
             claimed_by = NULL,
             lease_until = NULL,
             completed_at = ?3,
             updated_at = ?4
         WHERE thread_id = ?5
           AND status = 'Queued'
           AND dispatch_epoch < ?6",
        params![
            current_epoch as i64,
            mailbox_state::REASON_QUEUED_SUPERSEDED_BY_EPOCH,
            now as i64,
            now as i64,
            thread_id,
            current_epoch as i64
        ],
    )
    .map_err(|e| StorageError::Io(format!("supersede stale queued: {e}")))
}

fn supersede_stale_claimed_if_needed(
    conn: &Connection,
    dispatch: &RunDispatch,
    claim_token: &str,
    now: u64,
    reason: &str,
) -> Result<bool, StorageError> {
    let current_epoch = current_epoch_for_conn(conn, &dispatch.thread_id())?;
    if dispatch.dispatch_epoch() >= current_epoch {
        return Ok(false);
    }
    supersede_claimed_loaded(conn, dispatch, claim_token, now, reason)?;
    Ok(true)
}

// ── MailboxStore impl ──────────────────────────────────────────────

#[async_trait]
impl MailboxStore for SqliteMailboxStore {
    async fn enqueue(&self, dispatch: &RunDispatch) -> Result<(), StorageError> {
        dispatch.validate_for_enqueue()?;
        let conn = self.conn.lock().await;
        // Dedupe check.
        if let Some(ref dk) = dispatch.dedupe_key() {
            let dup: bool = conn
                .prepare_cached(
                    "SELECT EXISTS(
                        SELECT 1 FROM run_dispatches
                        WHERE thread_id = ?1
                          AND dedupe_key = ?2
                          AND status NOT IN ('Acked','Cancelled','Superseded','DeadLetter')
                    )",
                )
                .map_err(|e| StorageError::Io(format!("prepare dedupe: {e}")))?
                .query_row(params![dispatch.thread_id(), dk], |row| {
                    row.get::<_, bool>(0)
                })
                .map_err(|e| StorageError::Io(format!("dedupe check: {e}")))?;

            if dup {
                return Err(StorageError::AlreadyExists(format!("dedupe_key={dk}")));
            }
        }

        // Auto-create dispatch_epoch row; fetch current dispatch_epoch.
        conn.execute(
            "INSERT INTO thread_dispatch_epochs (thread_id, current_epoch)
             VALUES (?1, 0)
             ON CONFLICT (thread_id) DO NOTHING",
            params![dispatch.thread_id()],
        )
        .map_err(|e| StorageError::Io(format!("upsert dispatch_epoch: {e}")))?;

        let dispatch_epoch: i64 = conn
            .prepare_cached("SELECT current_epoch FROM thread_dispatch_epochs WHERE thread_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare dispatch_epoch select: {e}")))?
            .query_row(params![dispatch.thread_id()], |row| row.get(0))
            .map_err(|e| StorageError::Io(format!("dispatch_epoch select: {e}")))?;

        conn.execute(
            "INSERT INTO run_dispatches (
                dispatch_id, thread_id, run_id,
                priority, dedupe_key, dispatch_epoch,
                status, available_at, attempt_count, max_attempts,
                last_error, claim_token, claimed_by, lease_until,
                created_at, updated_at
            ) VALUES (
                ?1, ?2, ?3,
                ?4, ?5, ?6,
                ?7, ?8, ?9, ?10,
                ?11, ?12, ?13, ?14,
                ?15, ?16
            )",
            params![
                dispatch.dispatch_id(),
                dispatch.thread_id(),
                dispatch.run_id(),
                dispatch.priority() as i64,
                dispatch.dedupe_key(),
                dispatch_epoch,
                status_to_str(RunDispatchStatus::Queued),
                dispatch.available_at() as i64,
                dispatch.attempt_count() as i64,
                dispatch.max_attempts() as i64,
                dispatch.last_error(),
                dispatch.claim_token(),
                dispatch.claimed_by(),
                dispatch.lease_until().map(|v| v as i64),
                dispatch.created_at() as i64,
                dispatch.updated_at() as i64,
            ],
        )
        .map_err(|e| StorageError::Io(format!("insert dispatch: {e}")))?;

        Ok(())
    }

    async fn claim(
        &self,
        thread_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        let conn = self.conn.lock().await;

        supersede_stale_queued_for_thread(&conn, thread_id, now)?;

        // Cannot claim while another dispatch is already Claimed for this thread.
        let has_claimed: bool = conn
            .prepare_cached(
                "SELECT EXISTS(
                    SELECT 1 FROM run_dispatches
                    WHERE thread_id = ?1 AND status = 'Claimed'
                )",
            )
            .map_err(|e| StorageError::Io(format!("prepare claim check: {e}")))?
            .query_row(params![thread_id], |row| row.get::<_, bool>(0))
            .map_err(|e| StorageError::Io(format!("claim check: {e}")))?;

        if has_claimed {
            return Ok(vec![]);
        }

        // Find oldest Queued dispatches eligible for claiming.
        let mut stmt = conn
            .prepare_cached(
                "SELECT dispatch_id FROM run_dispatches
                 WHERE thread_id = ?1
                   AND status = 'Queued'
                   AND available_at <= ?2
                 ORDER BY priority ASC, created_at ASC
                 LIMIT ?3",
            )
            .map_err(|e| StorageError::Io(format!("prepare claim select: {e}")))?;

        let dispatch_ids: Vec<String> = stmt
            .query_map(params![thread_id, now as i64, limit as i64], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|e| StorageError::Io(format!("claim select: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::Io(format!("claim collect: {e}")))?;

        if dispatch_ids.is_empty() {
            return Ok(vec![]);
        }

        let token = Uuid::now_v7().to_string();
        let lease_until = now + lease_ms;

        // Update each dispatch to Claimed.
        let mut update_stmt = conn
            .prepare_cached(
                "UPDATE run_dispatches
                 SET status = 'Claimed',
                     claim_token = ?1,
                     claimed_by = ?2,
                     lease_until = ?3,
                     updated_at = ?4
                 WHERE dispatch_id = ?5",
            )
            .map_err(|e| StorageError::Io(format!("prepare claim update: {e}")))?;

        for id in &dispatch_ids {
            update_stmt
                .execute(params![
                    token,
                    consumer_id,
                    lease_until as i64,
                    now as i64,
                    id
                ])
                .map_err(|e| StorageError::Io(format!("claim update: {e}")))?;
        }

        // Re-read the claimed dispatches.
        drop(update_stmt);
        drop(stmt);

        let mut result = Vec::with_capacity(dispatch_ids.len());
        let mut load_stmt = conn
            .prepare_cached("SELECT * FROM run_dispatches WHERE dispatch_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare claim reload: {e}")))?;

        for id in &dispatch_ids {
            let dispatch = load_stmt
                .query_row(params![id], row_to_dispatch)
                .map_err(|e| StorageError::Io(format!("claim reload: {e}")))?;
            result.push(dispatch);
        }

        Ok(result)
    }

    async fn claim_dispatch(
        &self,
        dispatch_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
    ) -> Result<Option<RunDispatch>, StorageError> {
        let conn = self.conn.lock().await;

        // Check that the dispatch exists and is Queued.
        let mut stmt = conn
            .prepare_cached("SELECT * FROM run_dispatches WHERE dispatch_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare claim_dispatch load: {e}")))?;

        let dispatch = stmt
            .query_row(params![dispatch_id], row_to_dispatch)
            .optional()
            .map_err(|e| StorageError::Io(format!("claim_dispatch load: {e}")))?;

        let dispatch = match dispatch {
            Some(j) if j.status() == RunDispatchStatus::Queued => j,
            _ => return Ok(None),
        };

        if dispatch.dispatch_epoch() < current_epoch_for_conn(&conn, &dispatch.thread_id())? {
            conn.execute(
                "UPDATE run_dispatches
                 SET status = 'Superseded',
                     dispatch_epoch = ?1,
                     last_error = ?2,
                     claim_token = NULL,
                     claimed_by = NULL,
                     lease_until = NULL,
                     completed_at = ?3,
                     updated_at = ?4
                 WHERE dispatch_id = ?5
                   AND status = 'Queued'",
                params![
                    current_epoch_for_conn(&conn, &dispatch.thread_id())? as i64,
                    mailbox_state::REASON_QUEUED_SUPERSEDED_BY_EPOCH,
                    now as i64,
                    now as i64,
                    dispatch_id
                ],
            )
            .map_err(|e| StorageError::Io(format!("claim_dispatch supersede stale: {e}")))?;
            return Ok(None);
        }

        // Same-thread exclusivity: reject if another dispatch for this thread is Claimed.
        let has_other_claimed: bool = conn
            .prepare_cached(
                "SELECT EXISTS(
                    SELECT 1 FROM run_dispatches
                    WHERE thread_id = ?1
                      AND dispatch_id != ?2
                      AND status = 'Claimed'
                )",
            )
            .map_err(|e| StorageError::Io(format!("prepare claim_dispatch check: {e}")))?
            .query_row(params![dispatch.thread_id(), dispatch_id], |row| {
                row.get::<_, bool>(0)
            })
            .map_err(|e| StorageError::Io(format!("claim_dispatch check: {e}")))?;

        if has_other_claimed {
            return Ok(None);
        }

        let token = Uuid::now_v7().to_string();
        let lease_until = now + lease_ms;

        conn.execute(
            "UPDATE run_dispatches
             SET status = 'Claimed',
                 claim_token = ?1,
                 claimed_by = ?2,
                 lease_until = ?3,
                 updated_at = ?4
             WHERE dispatch_id = ?5",
            params![
                token,
                consumer_id,
                lease_until as i64,
                now as i64,
                dispatch_id
            ],
        )
        .map_err(|e| StorageError::Io(format!("claim_dispatch update: {e}")))?;

        // Re-read the updated dispatch.
        drop(stmt);
        let updated = conn
            .prepare_cached("SELECT * FROM run_dispatches WHERE dispatch_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare claim_dispatch reload: {e}")))?
            .query_row(params![dispatch_id], row_to_dispatch)
            .map_err(|e| StorageError::Io(format!("claim_dispatch reload: {e}")))?;

        Ok(Some(updated))
    }

    async fn ack(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        let conn = self.conn.lock().await;

        let dispatch = conn
            .prepare_cached("SELECT * FROM run_dispatches WHERE dispatch_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare ack load: {e}")))?
            .query_row(params![dispatch_id], row_to_dispatch)
            .optional()
            .map_err(|e| StorageError::Io(format!("ack load: {e}")))?
            .ok_or_else(|| StorageError::NotFound(dispatch_id.to_string()))?;

        if dispatch.claim_token() != Some(claim_token) {
            return Err(StorageError::VersionConflict {
                expected: 0,
                actual: 1,
            });
        }
        if supersede_stale_claimed_if_needed(
            &conn,
            &dispatch,
            claim_token,
            now,
            mailbox_state::REASON_CLAIMED_SUPERSEDED_BEFORE_ACK,
        )? {
            return Err(StorageError::VersionConflict {
                expected: dispatch.dispatch_epoch(),
                actual: current_epoch_for_conn(&conn, &dispatch.thread_id())?,
            });
        }

        conn.execute(
            "UPDATE run_dispatches
             SET status = 'Acked',
                 claim_token = NULL,
                 claimed_by = NULL,
                 lease_until = NULL,
                 completed_at = ?1,
                 updated_at = ?2
             WHERE dispatch_id = ?3",
            params![now as i64, now as i64, dispatch_id],
        )
        .map_err(|e| StorageError::Io(format!("ack update: {e}")))?;

        Ok(())
    }

    async fn record_dispatch_start(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        dispatch_instance_id: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        let conn = self.conn.lock().await;

        let dispatch = conn
            .prepare_cached("SELECT * FROM run_dispatches WHERE dispatch_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare record_dispatch_start load: {e}")))?
            .query_row(params![dispatch_id], row_to_dispatch)
            .optional()
            .map_err(|e| StorageError::Io(format!("record_dispatch_start load: {e}")))?
            .ok_or_else(|| StorageError::NotFound(dispatch_id.to_string()))?;

        if dispatch.status() != RunDispatchStatus::Claimed
            || dispatch.claim_token() != Some(claim_token)
        {
            return Err(StorageError::VersionConflict {
                expected: 0,
                actual: 1,
            });
        }
        if supersede_stale_claimed_if_needed(
            &conn,
            &dispatch,
            claim_token,
            now,
            mailbox_state::REASON_CLAIMED_SUPERSEDED_BEFORE_START,
        )? {
            return Err(StorageError::VersionConflict {
                expected: dispatch.dispatch_epoch(),
                actual: current_epoch_for_conn(&conn, &dispatch.thread_id())?,
            });
        }

        conn.execute(
            "UPDATE run_dispatches
             SET dispatch_instance_id = ?1,
                 run_status = ?2,
                 termination = NULL,
                 run_response = NULL,
                 run_error = NULL,
                 completed_at = NULL,
                 updated_at = ?3
             WHERE dispatch_id = ?4",
            params![
                dispatch_instance_id,
                run_status_to_str(RunStatus::Running),
                now as i64,
                dispatch_id
            ],
        )
        .map_err(|e| StorageError::Io(format!("record_dispatch_start update: {e}")))?;

        Ok(())
    }

    async fn record_run_result(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        result: &RunDispatchResult,
        now: u64,
    ) -> Result<(), StorageError> {
        let conn = self.conn.lock().await;

        let dispatch = conn
            .prepare_cached("SELECT * FROM run_dispatches WHERE dispatch_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare record_run_result load: {e}")))?
            .query_row(params![dispatch_id], row_to_dispatch)
            .optional()
            .map_err(|e| StorageError::Io(format!("record_run_result load: {e}")))?
            .ok_or_else(|| StorageError::NotFound(dispatch_id.to_string()))?;

        if dispatch.status() != RunDispatchStatus::Claimed
            || dispatch.claim_token() != Some(claim_token)
        {
            return Err(StorageError::VersionConflict {
                expected: 0,
                actual: 1,
            });
        }
        if supersede_stale_claimed_if_needed(
            &conn,
            &dispatch,
            claim_token,
            now,
            mailbox_state::REASON_CLAIMED_SUPERSEDED_BEFORE_RESULT,
        )? {
            return Err(StorageError::VersionConflict {
                expected: dispatch.dispatch_epoch(),
                actual: current_epoch_for_conn(&conn, &dispatch.thread_id())?,
            });
        }

        let termination = termination_to_json(result.termination.as_ref())?;

        conn.execute(
            "UPDATE run_dispatches
             SET dispatch_instance_id = ?1,
                 run_status = ?2,
                 termination = ?3,
                 run_response = ?4,
                 run_error = ?5,
                 completed_at = ?6,
                 updated_at = ?7
            WHERE dispatch_id = ?8",
            params![
                &result.dispatch_instance_id,
                run_status_to_str(result.status),
                termination,
                result.response.as_deref(),
                result.error.as_deref(),
                now as i64,
                now as i64,
                dispatch_id
            ],
        )
        .map_err(|e| StorageError::Io(format!("record_run_result update: {e}")))?;

        Ok(())
    }

    async fn nack(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        retry_at: u64,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        let conn = self.conn.lock().await;

        let dispatch = conn
            .prepare_cached("SELECT * FROM run_dispatches WHERE dispatch_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare nack load: {e}")))?
            .query_row(params![dispatch_id], row_to_dispatch)
            .optional()
            .map_err(|e| StorageError::Io(format!("nack load: {e}")))?
            .ok_or_else(|| StorageError::NotFound(dispatch_id.to_string()))?;

        if dispatch.claim_token() != Some(claim_token) {
            return Err(StorageError::VersionConflict {
                expected: 0,
                actual: 1,
            });
        }
        if supersede_stale_claimed_if_needed(
            &conn,
            &dispatch,
            claim_token,
            now,
            mailbox_state::REASON_CLAIMED_SUPERSEDED_BEFORE_NACK,
        )? {
            return Err(StorageError::VersionConflict {
                expected: dispatch.dispatch_epoch(),
                actual: current_epoch_for_conn(&conn, &dispatch.thread_id())?,
            });
        }

        let new_attempt_count = dispatch.attempt_count() + 1;

        if new_attempt_count >= dispatch.max_attempts() {
            // Dead letter.
            conn.execute(
                "UPDATE run_dispatches
                 SET status = 'DeadLetter',
                     attempt_count = ?1,
                     last_error = ?2,
                     claim_token = NULL,
                     claimed_by = NULL,
                     lease_until = NULL,
                     completed_at = ?3,
                     updated_at = ?4
                 WHERE dispatch_id = ?5",
                params![
                    new_attempt_count as i64,
                    error,
                    now as i64,
                    now as i64,
                    dispatch_id
                ],
            )
            .map_err(|e| StorageError::Io(format!("nack dead_letter update: {e}")))?;
        } else {
            // Requeue.
            conn.execute(
                "UPDATE run_dispatches
                 SET status = 'Queued',
                     attempt_count = ?1,
                     last_error = ?2,
                     available_at = ?3,
                     claim_token = NULL,
                     claimed_by = NULL,
                     lease_until = NULL,
                     updated_at = ?4
                 WHERE dispatch_id = ?5",
                params![
                    new_attempt_count as i64,
                    error,
                    retry_at as i64,
                    now as i64,
                    dispatch_id
                ],
            )
            .map_err(|e| StorageError::Io(format!("nack requeue update: {e}")))?;
        }

        Ok(())
    }

    async fn dead_letter(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        let conn = self.conn.lock().await;

        let dispatch = conn
            .prepare_cached("SELECT * FROM run_dispatches WHERE dispatch_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare dead_letter load: {e}")))?
            .query_row(params![dispatch_id], row_to_dispatch)
            .optional()
            .map_err(|e| StorageError::Io(format!("dead_letter load: {e}")))?
            .ok_or_else(|| StorageError::NotFound(dispatch_id.to_string()))?;

        if dispatch.claim_token() != Some(claim_token) {
            return Err(StorageError::VersionConflict {
                expected: 0,
                actual: 1,
            });
        }
        if supersede_stale_claimed_if_needed(
            &conn,
            &dispatch,
            claim_token,
            now,
            mailbox_state::REASON_CLAIMED_SUPERSEDED_BEFORE_DEAD_LETTER,
        )? {
            return Err(StorageError::VersionConflict {
                expected: dispatch.dispatch_epoch(),
                actual: current_epoch_for_conn(&conn, &dispatch.thread_id())?,
            });
        }

        conn.execute(
            "UPDATE run_dispatches
             SET status = 'DeadLetter',
                 last_error = ?1,
                 claim_token = NULL,
                 claimed_by = NULL,
                 lease_until = NULL,
                 completed_at = ?2,
                 updated_at = ?3
             WHERE dispatch_id = ?4",
            params![error, now as i64, now as i64, dispatch_id],
        )
        .map_err(|e| StorageError::Io(format!("dead_letter update: {e}")))?;

        Ok(())
    }

    async fn cancel(
        &self,
        dispatch_id: &str,
        now: u64,
    ) -> Result<Option<RunDispatch>, StorageError> {
        let conn = self.conn.lock().await;

        // Check that the dispatch exists and is Queued.
        let dispatch = conn
            .prepare_cached("SELECT * FROM run_dispatches WHERE dispatch_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare cancel load: {e}")))?
            .query_row(params![dispatch_id], row_to_dispatch)
            .optional()
            .map_err(|e| StorageError::Io(format!("cancel load: {e}")))?;

        match dispatch {
            Some(j) if j.status() == RunDispatchStatus::Queued => {}
            _ => return Ok(None),
        }

        conn.execute(
            "UPDATE run_dispatches
             SET status = 'Cancelled',
                 claim_token = NULL,
                 claimed_by = NULL,
                 lease_until = NULL,
                 completed_at = ?1,
                 updated_at = ?2
             WHERE dispatch_id = ?3",
            params![now as i64, now as i64, dispatch_id],
        )
        .map_err(|e| StorageError::Io(format!("cancel update: {e}")))?;

        let updated = conn
            .prepare_cached("SELECT * FROM run_dispatches WHERE dispatch_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare cancel reload: {e}")))?
            .query_row(params![dispatch_id], row_to_dispatch)
            .map_err(|e| StorageError::Io(format!("cancel reload: {e}")))?;

        Ok(Some(updated))
    }

    async fn extend_lease(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        extension_ms: u64,
        now: u64,
    ) -> Result<bool, StorageError> {
        let conn = self.conn.lock().await;

        let dispatch = conn
            .prepare_cached("SELECT * FROM run_dispatches WHERE dispatch_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare extend_lease load: {e}")))?
            .query_row(params![dispatch_id], row_to_dispatch)
            .optional()
            .map_err(|e| StorageError::Io(format!("extend_lease load: {e}")))?;
        let Some(dispatch) = dispatch else {
            return Ok(false);
        };
        if dispatch.status() != RunDispatchStatus::Claimed
            || dispatch.claim_token() != Some(claim_token)
        {
            return Ok(false);
        }
        if supersede_stale_claimed_if_needed(
            &conn,
            &dispatch,
            claim_token,
            now,
            mailbox_state::REASON_CLAIMED_SUPERSEDED_DURING_LEASE_RENEWAL,
        )? {
            return Ok(false);
        }

        let changed = conn
            .execute(
                "UPDATE run_dispatches
                 SET lease_until = ?1, updated_at = ?2
                 WHERE dispatch_id = ?3
                   AND status = 'Claimed'
                   AND claim_token = ?4",
                params![
                    (now + extension_ms) as i64,
                    now as i64,
                    dispatch_id,
                    claim_token
                ],
            )
            .map_err(|e| StorageError::Io(format!("extend_lease update: {e}")))?;

        Ok(changed > 0)
    }

    async fn interrupt(&self, thread_id: &str, now: u64) -> Result<MailboxInterrupt, StorageError> {
        self.interrupt_detailed(thread_id, now)
            .await
            .map(Into::into)
    }

    async fn interrupt_detailed(
        &self,
        thread_id: &str,
        now: u64,
    ) -> Result<MailboxInterruptDetails, StorageError> {
        let conn = self.conn.lock().await;

        // Bump dispatch_epoch: INSERT ON CONFLICT DO UPDATE +1.
        conn.execute(
            "INSERT INTO thread_dispatch_epochs (thread_id, current_epoch)
             VALUES (?1, 1)
             ON CONFLICT (thread_id) DO UPDATE
                SET current_epoch = current_epoch + 1",
            params![thread_id],
        )
        .map_err(|e| StorageError::Io(format!("interrupt bump dispatch_epoch: {e}")))?;

        let new_dispatch_epoch: i64 = conn
            .prepare_cached("SELECT current_epoch FROM thread_dispatch_epochs WHERE thread_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare interrupt dispatch_epoch: {e}")))?
            .query_row(params![thread_id], |row| row.get(0))
            .map_err(|e| StorageError::Io(format!("interrupt dispatch_epoch select: {e}")))?;

        let superseded_candidates = {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT * FROM run_dispatches
                     WHERE thread_id = ?1
                       AND status = 'Queued'
                       AND dispatch_epoch < ?2",
                )
                .map_err(|e| {
                    StorageError::Io(format!("prepare interrupt superseded select: {e}"))
                })?;
            stmt.query_map(params![thread_id, new_dispatch_epoch], row_to_dispatch)
                .map_err(|e| StorageError::Io(format!("interrupt superseded select: {e}")))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StorageError::Io(format!("interrupt superseded collect: {e}")))?
        };

        // Supersede all Queued dispatches for this thread with dispatch_epoch < new_dispatch_epoch.
        let superseded_count = conn
            .execute(
                "UPDATE run_dispatches
                 SET status = 'Superseded',
                     last_error = ?1,
                     claim_token = NULL,
                     claimed_by = NULL,
                     lease_until = NULL,
                     completed_at = ?2,
                     updated_at = ?3
                 WHERE thread_id = ?4
                   AND status = 'Queued'
                   AND dispatch_epoch < ?5",
                params![
                    mailbox_state::REASON_QUEUED_SUPERSEDED_BY_INTERRUPT,
                    now as i64,
                    now as i64,
                    thread_id,
                    new_dispatch_epoch
                ],
            )
            .map_err(|e| StorageError::Io(format!("interrupt supersede: {e}")))?;
        let mut superseded_dispatches = superseded_candidates
            .into_iter()
            .map(|mut dispatch| {
                mailbox_state::mark_superseded(
                    &mut dispatch,
                    now,
                    Some(mailbox_state::REASON_QUEUED_SUPERSEDED_BY_INTERRUPT),
                );
                dispatch
            })
            .collect::<Vec<_>>();
        superseded_dispatches.truncate(superseded_count);

        // Find active Claimed dispatch if any.
        let active_dispatch = conn
            .prepare_cached(
                "SELECT * FROM run_dispatches
                 WHERE thread_id = ?1 AND status = 'Claimed'
                 LIMIT 1",
            )
            .map_err(|e| StorageError::Io(format!("prepare interrupt active: {e}")))?
            .query_row(params![thread_id], row_to_dispatch)
            .optional()
            .map_err(|e| StorageError::Io(format!("interrupt active: {e}")))?;

        Ok(MailboxInterruptDetails {
            new_dispatch_epoch: new_dispatch_epoch as u64,
            active_dispatch,
            superseded_count,
            superseded_dispatches,
        })
    }

    async fn current_dispatch_epoch(&self, thread_id: &str) -> Result<u64, StorageError> {
        let conn = self.conn.lock().await;
        current_epoch_for_conn(&conn, thread_id)
    }

    async fn supersede_claimed(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        now: u64,
        reason: &str,
    ) -> Result<Option<RunDispatch>, StorageError> {
        let conn = self.conn.lock().await;
        let dispatch = conn
            .prepare_cached("SELECT * FROM run_dispatches WHERE dispatch_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare supersede claimed load: {e}")))?
            .query_row(params![dispatch_id], row_to_dispatch)
            .optional()
            .map_err(|e| StorageError::Io(format!("supersede claimed load: {e}")))?;
        let Some(dispatch) = dispatch else {
            return Ok(None);
        };
        supersede_claimed_loaded(&conn, &dispatch, claim_token, now, reason)
    }

    async fn load_dispatch(&self, dispatch_id: &str) -> Result<Option<RunDispatch>, StorageError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare_cached("SELECT * FROM run_dispatches WHERE dispatch_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare load_dispatch: {e}")))?;

        let result = stmt
            .query_row(params![dispatch_id], row_to_dispatch)
            .optional()
            .map_err(|e| StorageError::Io(format!("load_dispatch: {e}")))?;

        Ok(result)
    }

    async fn list_dispatches(
        &self,
        thread_id: &str,
        status_filter: Option<&[RunDispatchStatus]>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        let conn = self.conn.lock().await;

        let (sql, dyn_params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) =
            if let Some(statuses) = status_filter {
                if statuses.is_empty() {
                    return Ok(vec![]);
                }
                let placeholders: Vec<String> = statuses
                    .iter()
                    .enumerate()
                    .map(|(i, _)| format!("?{}", i + 2))
                    .collect();
                let sql = format!(
                    "SELECT * FROM run_dispatches
                     WHERE thread_id = ?1 AND status IN ({})
                     ORDER BY priority ASC, created_at ASC
                     LIMIT {} OFFSET {}",
                    placeholders.join(","),
                    limit,
                    offset
                );
                let mut p: Vec<Box<dyn rusqlite::types::ToSql>> =
                    vec![Box::new(thread_id.to_string())];
                for s in statuses {
                    p.push(Box::new(status_to_str(*s).to_string()));
                }
                (sql, p)
            } else {
                let sql = format!(
                    "SELECT * FROM run_dispatches
                     WHERE thread_id = ?1
                     ORDER BY priority ASC, created_at ASC
                     LIMIT {} OFFSET {}",
                    limit, offset
                );
                let p: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(thread_id.to_string())];
                (sql, p)
            };

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            dyn_params.iter().map(|b| b.as_ref()).collect();

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| StorageError::Io(format!("prepare list_dispatches: {e}")))?;

        let rows = stmt
            .query_map(param_refs.as_slice(), row_to_dispatch)
            .map_err(|e| StorageError::Io(format!("list_dispatches query: {e}")))?;

        let mut dispatches = Vec::new();
        for row in rows {
            dispatches
                .push(row.map_err(|e| StorageError::Io(format!("list_dispatches row: {e}")))?);
        }
        Ok(dispatches)
    }

    async fn count_dispatches_by_status(
        &self,
        status: RunDispatchStatus,
    ) -> Result<usize, StorageError> {
        let conn = self.conn.lock().await;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM run_dispatches WHERE status = ?1",
                params![status_to_str(status)],
                |row| row.get(0),
            )
            .map_err(|e| StorageError::Io(format!("count_dispatches_by_status: {e}")))?;
        usize::try_from(count)
            .map_err(|e| StorageError::Io(format!("dispatch count conversion: {e}")))
    }

    async fn list_terminal_dispatches(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare_cached(
                "SELECT * FROM run_dispatches
                 WHERE status IN ('Acked', 'Cancelled', 'Superseded', 'DeadLetter')
                 ORDER BY updated_at ASC, created_at ASC, dispatch_id ASC
                 LIMIT ?1 OFFSET ?2",
            )
            .map_err(|e| StorageError::Io(format!("prepare list_terminal_dispatches: {e}")))?;
        stmt.query_map(params![limit as i64, offset as i64], row_to_dispatch)
            .map_err(|e| StorageError::Io(format!("list_terminal_dispatches query: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::Io(format!("list_terminal_dispatches collect: {e}")))
    }

    async fn reclaim_expired_leases(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        let conn = self.conn.lock().await;

        // Step 1: Find expired Claimed dispatches.
        let mut stmt = conn
            .prepare_cached(
                "SELECT * FROM run_dispatches
                 WHERE status = 'Claimed'
                   AND lease_until < ?1
                 LIMIT ?2",
            )
            .map_err(|e| StorageError::Io(format!("prepare reclaim select: {e}")))?;

        let expired: Vec<RunDispatch> = stmt
            .query_map(params![now as i64, limit as i64], |row| {
                row_to_dispatch(row)
            })
            .map_err(|e| StorageError::Io(format!("reclaim select: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::Io(format!("reclaim collect: {e}")))?;

        if expired.is_empty() {
            return Ok(vec![]);
        }

        // Step 2: Update each expired dispatch.
        let mut requeue_stmt = conn
            .prepare_cached(
                "UPDATE run_dispatches
                 SET status = 'Queued',
                     attempt_count = ?1,
                     claim_token = NULL,
                     claimed_by = NULL,
                     lease_until = NULL,
                     updated_at = ?2
                 WHERE dispatch_id = ?3",
            )
            .map_err(|e| StorageError::Io(format!("prepare reclaim requeue: {e}")))?;

        let mut deadletter_stmt = conn
            .prepare_cached(
                "UPDATE run_dispatches
                 SET status = 'DeadLetter',
                     attempt_count = ?1,
                     claim_token = NULL,
                     claimed_by = NULL,
                     lease_until = NULL,
                     completed_at = ?2,
                     updated_at = ?3
                 WHERE dispatch_id = ?4",
            )
            .map_err(|e| StorageError::Io(format!("prepare reclaim deadletter: {e}")))?;

        for dispatch in &expired {
            let Some(claim_token) = dispatch.claim_token() else {
                continue;
            };
            if supersede_stale_claimed_if_needed(
                &conn,
                dispatch,
                claim_token,
                now,
                mailbox_state::REASON_CLAIMED_LEASE_EXPIRED_AFTER_INTERRUPT,
            )? {
                continue;
            }

            let new_attempt = dispatch.attempt_count() + 1;
            if new_attempt >= dispatch.max_attempts() {
                deadletter_stmt
                    .execute(params![
                        new_attempt as i64,
                        now as i64,
                        now as i64,
                        &dispatch.dispatch_id()
                    ])
                    .map_err(|e| StorageError::Io(format!("reclaim deadletter: {e}")))?;
            } else {
                requeue_stmt
                    .execute(params![
                        new_attempt as i64,
                        now as i64,
                        &dispatch.dispatch_id()
                    ])
                    .map_err(|e| StorageError::Io(format!("reclaim requeue: {e}")))?;
            }
        }

        // Step 3: Re-read the updated dispatches.
        drop(requeue_stmt);
        drop(deadletter_stmt);
        drop(stmt);

        let mut result = Vec::with_capacity(expired.len());
        let mut load_stmt = conn
            .prepare_cached("SELECT * FROM run_dispatches WHERE dispatch_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare reclaim reload: {e}")))?;

        for dispatch in &expired {
            let dispatch = load_stmt
                .query_row(params![&dispatch.dispatch_id()], row_to_dispatch)
                .map_err(|e| StorageError::Io(format!("reclaim reload: {e}")))?;
            if dispatch.status() != RunDispatchStatus::Superseded {
                result.push(dispatch);
            }
        }

        Ok(result)
    }

    async fn purge_terminal(&self, older_than: u64) -> Result<usize, StorageError> {
        let conn = self.conn.lock().await;

        let deleted = conn
            .execute(
                "DELETE FROM run_dispatches
                 WHERE status IN ('Acked', 'Cancelled', 'Superseded', 'DeadLetter')
                   AND updated_at < ?1",
                params![older_than as i64],
            )
            .map_err(|e| StorageError::Io(format!("purge_terminal: {e}")))?;

        Ok(deleted)
    }

    async fn queued_thread_ids(&self) -> Result<Vec<String>, StorageError> {
        let conn = self.conn.lock().await;

        let mut stmt = conn
            .prepare_cached(
                "SELECT DISTINCT thread_id FROM run_dispatches
                 WHERE status = 'Queued'
                 ORDER BY thread_id",
            )
            .map_err(|e| StorageError::Io(format!("prepare queued_thread_ids: {e}")))?;

        let ids: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| StorageError::Io(format!("queued_thread_ids query: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::Io(format!("queued_thread_ids collect: {e}")))?;

        Ok(ids)
    }
}

// We need the `optional()` extension on `Result<T, rusqlite::Error>`.
trait OptionalExt<T> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalExt<T> for Result<T, rusqlite::Error> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_dispatch(id: &str, thread_id: &str) -> RunDispatch {
        RunDispatch::queued(
            id.to_string(),
            thread_id.to_string(),
            format!("run-{id}"),
            1000,
        )
    }

    #[tokio::test]
    async fn enqueue_and_load() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let dispatch = make_dispatch("dispatch-1", "thread-a");

        store.enqueue(&dispatch).await.unwrap();

        let loaded = store.load_dispatch("dispatch-1").await.unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.dispatch_id(), "dispatch-1");
        assert_eq!(loaded.thread_id(), "thread-a");
        assert_eq!(loaded.run_id(), "run-dispatch-1");
        assert_eq!(loaded.status(), RunDispatchStatus::Queued);
        assert_eq!(loaded.dispatch_epoch(), 0);
        assert_eq!(loaded.priority(), 128);

        // Non-existent dispatch returns None.
        let missing = store.load_dispatch("no-such-dispatch").await.unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn enqueue_dedupe_rejects_duplicate() {
        let store = SqliteMailboxStore::open_memory().unwrap();

        let dispatch1 =
            make_dispatch("dispatch-1", "thread-a").with_dedupe_key(Some("dk-1".to_string()));
        store.enqueue(&dispatch1).await.unwrap();

        // Second enqueue with same dedupe_key should fail.
        let dispatch2 =
            make_dispatch("dispatch-2", "thread-a").with_dedupe_key(Some("dk-1".to_string()));
        let result = store.enqueue(&dispatch2).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            StorageError::AlreadyExists(msg) => assert!(msg.contains("dk-1")),
            other => panic!("expected AlreadyExists, got: {other:?}"),
        }

        // Different dedupe_key should succeed.
        let dispatch3 =
            make_dispatch("dispatch-3", "thread-a").with_dedupe_key(Some("dk-2".to_string()));
        store.enqueue(&dispatch3).await.unwrap();

        // Same dedupe_key in a different thread should succeed.
        let dispatch4 =
            make_dispatch("dispatch-4", "thread-b").with_dedupe_key(Some("dk-1".to_string()));
        store.enqueue(&dispatch4).await.unwrap();
    }

    #[tokio::test]
    async fn list_dispatches_filters_by_status() {
        let store = SqliteMailboxStore::open_memory().unwrap();

        // Enqueue 3 dispatches for the same thread.
        for i in 0..3 {
            let dispatch = make_dispatch(&format!("dispatch-{i}"), "thread-a");
            store.enqueue(&dispatch).await.unwrap();
        }

        // Also enqueue one for a different thread (should not appear).
        let other = make_dispatch("dispatch-other", "thread-b");
        store.enqueue(&other).await.unwrap();

        // List all dispatches for thread-a (no status filter).
        let all = store
            .list_dispatches("thread-a", None, 100, 0)
            .await
            .unwrap();
        assert_eq!(all.len(), 3);

        // Filter by Queued status.
        let queued = store
            .list_dispatches("thread-a", Some(&[RunDispatchStatus::Queued]), 100, 0)
            .await
            .unwrap();
        assert_eq!(queued.len(), 3);

        // Filter by Claimed (none exist).
        let claimed = store
            .list_dispatches("thread-a", Some(&[RunDispatchStatus::Claimed]), 100, 0)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 0);

        // Test limit.
        let limited = store.list_dispatches("thread-a", None, 2, 0).await.unwrap();
        assert_eq!(limited.len(), 2);

        // Test offset.
        let offset = store
            .list_dispatches("thread-a", None, 100, 2)
            .await
            .unwrap();
        assert_eq!(offset.len(), 1);
    }

    #[tokio::test]
    async fn list_dispatches_sorted_by_priority_then_created_at() {
        let store = SqliteMailboxStore::open_memory().unwrap();

        let j1 = make_dispatch("dispatch-low", "thread-a")
            .with_priority(200)
            .with_created_at(100);
        store.enqueue(&j1).await.unwrap();

        let j2 = make_dispatch("dispatch-high", "thread-a")
            .with_priority(10)
            .with_created_at(200);
        store.enqueue(&j2).await.unwrap();

        let j3 = make_dispatch("dispatch-high-early", "thread-a")
            .with_priority(10)
            .with_created_at(50);
        store.enqueue(&j3).await.unwrap();

        let list = store
            .list_dispatches("thread-a", None, 100, 0)
            .await
            .unwrap();
        assert_eq!(list.len(), 3);
        // priority 10 created_at 50
        assert_eq!(list[0].dispatch_id(), "dispatch-high-early");
        // priority 10 created_at 200
        assert_eq!(list[1].dispatch_id(), "dispatch-high");
        // priority 200
        assert_eq!(list[2].dispatch_id(), "dispatch-low");
    }

    #[tokio::test]
    async fn enqueue_sets_dispatch_epoch_from_store() {
        let store = SqliteMailboxStore::open_memory().unwrap();

        let dispatch = make_dispatch("dispatch-1", "thread-a").with_dispatch_epoch(999);
        store.enqueue(&dispatch).await.unwrap();

        let loaded = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
        assert_eq!(
            loaded.dispatch_epoch(),
            0,
            "dispatch_epoch should come from store, not from input"
        );
    }

    #[tokio::test]
    async fn claim_and_ack() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let dispatch = make_dispatch("dispatch-1", "thread-a");
        store.enqueue(&dispatch).await.unwrap();

        // Claim the dispatch.
        let claimed = store
            .claim("thread-a", "consumer-1", 30_000, 2000, 10)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].dispatch_id(), "dispatch-1");
        assert_eq!(claimed[0].status(), RunDispatchStatus::Claimed);
        assert!(claimed[0].claim_token().is_some());
        assert_eq!(claimed[0].claimed_by(), Some("consumer-1"));
        assert_eq!(claimed[0].lease_until(), Some(32_000));

        // Cannot double-claim while a dispatch is Claimed in this mailbox.
        let dispatch2 = make_dispatch("dispatch-2", "thread-a").with_created_at(2000);
        store.enqueue(&dispatch2).await.unwrap();
        let double = store
            .claim("thread-a", "consumer-2", 30_000, 2000, 10)
            .await
            .unwrap();
        assert!(
            double.is_empty(),
            "should not claim while another is Claimed"
        );

        // Ack the first dispatch.
        let token = claimed[0].claim_token().unwrap();
        store.ack("dispatch-1", token, 3000).await.unwrap();

        let loaded = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
        assert_eq!(loaded.status(), RunDispatchStatus::Acked);
    }

    #[tokio::test]
    async fn claim_honors_batch_limit_without_active_claim() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        for id in ["dispatch-1", "dispatch-2", "dispatch-3"] {
            store.enqueue(&make_dispatch(id, "thread-a")).await.unwrap();
        }

        let claimed = store
            .claim("thread-a", "consumer-1", 30_000, 2000, 2)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 2);
        assert!(
            claimed
                .iter()
                .all(|dispatch| dispatch.status() == RunDispatchStatus::Claimed)
        );
    }

    #[tokio::test]
    async fn nack_increments_attempt_and_requeues() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let dispatch = make_dispatch("dispatch-1", "thread-a").with_max_attempts(3);
        store.enqueue(&dispatch).await.unwrap();

        // Claim then nack.
        let claimed = store
            .claim("thread-a", "c1", 30_000, 1000, 10)
            .await
            .unwrap();
        let token = claimed[0].claim_token().unwrap();

        store
            .nack("dispatch-1", token, 5000, "transient error", 2000)
            .await
            .unwrap();

        let loaded = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
        assert_eq!(loaded.status(), RunDispatchStatus::Queued);
        assert_eq!(loaded.attempt_count(), 1);
        assert_eq!(loaded.available_at(), 5000);
        assert_eq!(loaded.last_error(), Some("transient error"));
        assert!(loaded.claim_token().is_none());
        assert!(loaded.claimed_by().is_none());
        assert!(loaded.lease_until().is_none());
    }

    #[tokio::test]
    async fn nack_dead_letters_on_max_attempts() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let dispatch = make_dispatch("dispatch-1", "thread-a").with_max_attempts(1);
        store.enqueue(&dispatch).await.unwrap();

        let claimed = store
            .claim("thread-a", "c1", 30_000, 1000, 10)
            .await
            .unwrap();
        let token = claimed[0].claim_token().unwrap();

        store
            .nack("dispatch-1", token, 5000, "fatal", 2000)
            .await
            .unwrap();

        let loaded = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
        assert_eq!(loaded.status(), RunDispatchStatus::DeadLetter);
        assert_eq!(loaded.attempt_count(), 1);
        assert_eq!(loaded.last_error(), Some("fatal"));
    }

    #[tokio::test]
    async fn dead_letter_explicit() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let dispatch = make_dispatch("dispatch-1", "thread-a");
        store.enqueue(&dispatch).await.unwrap();

        let claimed = store
            .claim("thread-a", "c1", 30_000, 1000, 10)
            .await
            .unwrap();
        let token = claimed[0].claim_token().unwrap();

        store
            .dead_letter("dispatch-1", token, "permanent failure", 2000)
            .await
            .unwrap();

        let loaded = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
        assert_eq!(loaded.status(), RunDispatchStatus::DeadLetter);
        assert_eq!(loaded.last_error(), Some("permanent failure"));
        assert!(loaded.claim_token().is_none());
        assert!(loaded.claimed_by().is_none());
        assert!(loaded.lease_until().is_none());
    }

    #[tokio::test]
    async fn ack_wrong_token_fails() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let dispatch = make_dispatch("dispatch-1", "thread-a");
        store.enqueue(&dispatch).await.unwrap();

        let claimed = store
            .claim("thread-a", "c1", 30_000, 1000, 10)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);

        let result = store.ack("dispatch-1", "wrong-token", 2000).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            StorageError::VersionConflict { .. } => {}
            other => panic!("expected VersionConflict, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn records_dispatch_start_and_run_result_separately_from_ack() {
        use awaken_server_contract::contract::lifecycle::TerminationReason;

        let store = SqliteMailboxStore::open_memory().unwrap();
        let dispatch = make_dispatch("dispatch-1", "thread-a");
        store.enqueue(&dispatch).await.unwrap();

        let claimed = store
            .claim("thread-a", "c1", 30_000, 1000, 1)
            .await
            .unwrap();
        let token = claimed[0].claim_token().unwrap();

        store
            .record_dispatch_start("dispatch-1", token, "dispatch-1", 1500)
            .await
            .unwrap();
        let running = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
        assert_eq!(running.status(), RunDispatchStatus::Claimed);
        assert_eq!(running.run_id(), dispatch.run_id());
        assert_eq!(running.dispatch_instance_id(), Some("dispatch-1"));
        assert_eq!(running.run_status(), Some(RunStatus::Running));
        assert!(running.termination().is_none());
        assert!(running.completed_at().is_none());

        let result = RunDispatchResult {
            run_id: "run-1".into(),
            dispatch_instance_id: "dispatch-1".into(),
            status: RunStatus::Done,
            termination: Some(TerminationReason::Blocked("policy".into())),
            response: None,
            error: Some("policy".into()),
        };
        store
            .record_run_result("dispatch-1", token, &result, 1800)
            .await
            .unwrap();
        let completed = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
        assert_eq!(completed.status(), RunDispatchStatus::Claimed);
        assert_eq!(completed.run_status(), Some(RunStatus::Done));
        assert_eq!(
            completed.termination(),
            Some(&TerminationReason::Blocked("policy".into()))
        );
        assert_eq!(completed.run_error(), Some("policy"));
        assert_eq!(completed.completed_at(), Some(1800));

        store.ack("dispatch-1", token, 2000).await.unwrap();
        let acked = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
        assert_eq!(acked.status(), RunDispatchStatus::Acked);
        assert_eq!(acked.run_status(), Some(RunStatus::Done));
        assert_eq!(acked.run_error(), Some("policy"));
    }

    #[tokio::test]
    async fn record_dispatch_start_rejects_stale_claim_token() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let dispatch = make_dispatch("dispatch-1", "thread-a");
        store.enqueue(&dispatch).await.unwrap();
        store
            .claim("thread-a", "c1", 30_000, 1000, 1)
            .await
            .unwrap();

        let result = store
            .record_dispatch_start("dispatch-1", "wrong-token", "dispatch-1", 1500)
            .await;
        assert!(matches!(result, Err(StorageError::VersionConflict { .. })));

        let loaded = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
        assert_eq!(loaded.run_id(), dispatch.run_id());
        assert!(loaded.run_status().is_none());
    }

    #[tokio::test]
    async fn cancel_queued_dispatch() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let dispatch = make_dispatch("dispatch-1", "thread-a");
        store.enqueue(&dispatch).await.unwrap();

        let cancelled = store.cancel("dispatch-1", 2000).await.unwrap();
        assert!(cancelled.is_some());
        let cancelled = cancelled.unwrap();
        assert_eq!(cancelled.status(), RunDispatchStatus::Cancelled);
        assert_eq!(cancelled.updated_at(), 2000);

        // Verify persisted.
        let loaded = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
        assert_eq!(loaded.status(), RunDispatchStatus::Cancelled);

        // Cancel a non-Queued dispatch returns None.
        let again = store.cancel("dispatch-1", 3000).await.unwrap();
        assert!(again.is_none());

        // Cancel non-existent dispatch returns None.
        let missing = store.cancel("no-such-dispatch", 3000).await.unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn interrupt_supersedes_queued() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        store
            .enqueue(&make_dispatch("dispatch-1", "thread-a"))
            .await
            .unwrap();
        store
            .enqueue(&make_dispatch("dispatch-2", "thread-a"))
            .await
            .unwrap();

        let result = store.interrupt("thread-a", 2000).await.unwrap();
        assert_eq!(result.new_dispatch_epoch, 1);
        assert_eq!(result.superseded_count, 2);
        assert!(result.active_dispatch.is_none());

        // Verify dispatches are Superseded.
        let listed = store
            .list_dispatches("thread-a", Some(&[RunDispatchStatus::Superseded]), 100, 0)
            .await
            .unwrap();
        assert_eq!(listed.len(), 2);
    }

    #[tokio::test]
    async fn interrupt_returns_active_claimed_dispatch() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let dispatch1 = make_dispatch("dispatch-1", "thread-a");
        store.enqueue(&dispatch1).await.unwrap();

        // Claim the first dispatch.
        let claimed = store
            .claim("thread-a", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);

        // Enqueue a second dispatch (Queued).
        store
            .enqueue(&make_dispatch("dispatch-2", "thread-a"))
            .await
            .unwrap();

        let result = store.interrupt("thread-a", 2000).await.unwrap();
        assert_eq!(result.new_dispatch_epoch, 1);
        assert_eq!(result.superseded_count, 1); // only dispatch-2 was Queued
        assert!(result.active_dispatch.is_some());
        assert_eq!(result.active_dispatch.unwrap().dispatch_id(), "dispatch-1");
    }

    #[tokio::test]
    async fn extend_lease_succeeds() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let dispatch = make_dispatch("dispatch-1", "thread-a");
        store.enqueue(&dispatch).await.unwrap();

        let claimed = store
            .claim("thread-a", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        let token = claimed[0].claim_token().unwrap();

        let ok = store
            .extend_lease("dispatch-1", &token, 60_000, 15_000)
            .await
            .unwrap();
        assert!(ok);

        let loaded = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
        assert_eq!(loaded.lease_until(), Some(75_000));

        // Wrong token returns false.
        let nope = store
            .extend_lease("dispatch-1", "wrong-token", 60_000, 20_000)
            .await
            .unwrap();
        assert!(!nope);

        // Non-existent dispatch returns false.
        let nope2 = store
            .extend_lease("no-such-dispatch", &token, 60_000, 20_000)
            .await
            .unwrap();
        assert!(!nope2);
    }

    #[tokio::test]
    async fn reclaim_expired_leases() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let dispatch = make_dispatch("dispatch-1", "thread-a");
        store.enqueue(&dispatch).await.unwrap();

        // Claim with a short lease.
        let claimed = store
            .claim("thread-a", "consumer-1", 1_000, 1000, 1)
            .await
            .unwrap();
        assert_eq!(claimed[0].lease_until(), Some(2_000));

        // At now=3000, lease is expired.
        let reclaimed = store.reclaim_expired_leases(3000, 10).await.unwrap();
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].dispatch_id(), "dispatch-1");
        assert_eq!(reclaimed[0].status(), RunDispatchStatus::Queued);
        assert_eq!(reclaimed[0].attempt_count(), 1);
        assert!(reclaimed[0].claim_token().is_none());
        assert!(reclaimed[0].claimed_by().is_none());
        assert!(reclaimed[0].lease_until().is_none());

        // Not expired yet: no reclaims.
        let claimed2 = store
            .claim("thread-a", "consumer-2", 100_000, 4000, 1)
            .await
            .unwrap();
        assert_eq!(claimed2.len(), 1);
        let none = store.reclaim_expired_leases(5000, 10).await.unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn purge_terminal_removes_old() {
        let store = SqliteMailboxStore::open_memory().unwrap();

        // Create and cancel a dispatch (terminal).
        let dispatch1 = make_dispatch("dispatch-1", "thread-a");
        store.enqueue(&dispatch1).await.unwrap();
        store.cancel("dispatch-1", 1000).await.unwrap();

        // Create a Queued dispatch (non-terminal, should not be purged).
        let dispatch2 = make_dispatch("dispatch-2", "thread-a");
        store.enqueue(&dispatch2).await.unwrap();

        // Purge with threshold after the cancelled dispatch's updated_at.
        let purged = store.purge_terminal(2000).await.unwrap();
        assert_eq!(purged, 1);

        // The cancelled dispatch is gone.
        let loaded = store.load_dispatch("dispatch-1").await.unwrap();
        assert!(loaded.is_none());

        // The queued dispatch remains.
        let loaded2 = store.load_dispatch("dispatch-2").await.unwrap();
        assert!(loaded2.is_some());
    }

    #[tokio::test]
    async fn queued_thread_ids() {
        let store = SqliteMailboxStore::open_memory().unwrap();

        // No dispatches yet.
        let ids = store.queued_thread_ids().await.unwrap();
        assert!(ids.is_empty());

        // Add queued dispatches in two threads.
        store
            .enqueue(&make_dispatch("dispatch-1", "thread-b"))
            .await
            .unwrap();
        store
            .enqueue(&make_dispatch("dispatch-2", "thread-a"))
            .await
            .unwrap();
        store
            .enqueue(&make_dispatch("dispatch-3", "thread-a"))
            .await
            .unwrap();

        let ids = store.queued_thread_ids().await.unwrap();
        assert_eq!(ids, vec!["thread-a", "thread-b"]);

        // Cancel all dispatches in thread-a.
        store.cancel("dispatch-2", 2000).await.unwrap();
        store.cancel("dispatch-3", 2000).await.unwrap();

        let ids = store.queued_thread_ids().await.unwrap();
        assert_eq!(ids, vec!["thread-b"]);
    }

    #[test]
    fn open_creates_run_dispatches_schema() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mailbox.sqlite");

        let store = SqliteMailboxStore::open(&db_path).unwrap();
        let conn = store.conn.try_lock().unwrap();
        for column in [
            "run_id",
            "dispatch_epoch",
            "dispatch_instance_id",
            "run_status",
            "termination",
            "run_response",
            "run_error",
            "completed_at",
        ] {
            let exists: bool = conn
                .prepare_cached(
                    "SELECT EXISTS(
                        SELECT 1 FROM pragma_table_info('run_dispatches')
                        WHERE name = ?1
                    )",
                )
                .unwrap()
                .query_row(params![column], |row| row.get(0))
                .unwrap();
            assert!(exists, "missing run dispatch column {column}");
        }
    }
}
