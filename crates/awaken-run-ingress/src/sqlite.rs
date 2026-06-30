//! SQLite durable implementation of the dispatch-store ports.
//!
//! The embedded sibling of [`PostgresDispatchStore`](crate::PostgresDispatchStore),
//! over the *same* dispatch schema. SQLite has no `FOR UPDATE SKIP LOCKED`, but it
//! does not need it: every claim runs in a `BEGIN IMMEDIATE` transaction that
//! takes the database write lock, so claims serialize and a run is owned by one
//! worker at a time. The claim policy — recover an expired lease, then wake a
//! parked run with pending input, then a fresh run — matches
//! [`MemoryDispatchStore`](crate::MemoryDispatchStore) exactly. The synchronous
//! `rusqlite` driver runs each operation on a blocking thread.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::dispatch::{
    CasOutcome, Claimed, DispatchError, DispatchOutcome, Lease, PendingInbox, PendingInput,
    PendingRecord, RunDispatch,
};
use crate::dispatch_schema::dispatch_bundle;
use crate::request::RunExecutionRequest;

/// Errors from constructing or migrating the dispatch store. Claim/settle-time
/// failures use the neutral [`DispatchError`].
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("open: {0}")]
    Open(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// A SQLite-backed dispatch store.
pub struct SqliteDispatchStore {
    conn: Arc<Mutex<Connection>>,
    prefix: String,
}

impl SqliteDispatchStore {
    /// Open (or create) a database file and apply the dispatch migrations.
    pub fn open(path: &str, prefix: impl Into<String>) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(|err| StoreError::Open(err.to_string()))?;
        Self::from_connection(conn, prefix)
    }

    /// Open a private in-memory database.
    pub fn open_in_memory(prefix: impl Into<String>) -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(|err| StoreError::Open(err.to_string()))?;
        Self::from_connection(conn, prefix)
    }

    fn from_connection(conn: Connection, prefix: impl Into<String>) -> Result<Self, StoreError> {
        let prefix = prefix.into();
        let bundle = dispatch_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        awaken_scoped_migration::sqlite::SqliteMigrationRunner::with_prefix(&prefix)
            .map_err(|err| StoreError::Migrate(err.to_string()))?
            .run_bundle(&conn, &bundle)
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            prefix,
        })
    }

    /// Run a closure with the locked connection on a blocking thread.
    async fn with_conn<T, F>(&self, f: F) -> Result<T, DispatchError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection, &str) -> Result<T, DispatchError> + Send + 'static,
    {
        let conn = self.conn.clone();
        let prefix = self.prefix.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn
                .lock()
                .map_err(|_| DispatchError::Rejected("dispatch connection poisoned".to_string()))?;
            f(&mut guard, &prefix)
        })
        .await
        .map_err(|err| DispatchError::Rejected(err.to_string()))?
    }
}

#[async_trait]
impl RunDispatch for SqliteDispatchStore {
    async fn enqueue(&self, request: RunExecutionRequest) -> Result<(), DispatchError> {
        let run_id = request.run_id().0.clone();
        let thread_id = request.thread_id().0.clone();
        let request_json = json(&request)?;
        self.with_conn(move |conn, p| {
            conn.execute(
                &format!(
                    "INSERT INTO {p}_dispatch (run_id, thread_id, request, status) \
                     VALUES (?1,?2,?3,'pending') ON CONFLICT(run_id) DO NOTHING"
                ),
                params![run_id, thread_id, request_json],
            )
            .map_err(reject)?;
            Ok(())
        })
        .await
    }

    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let owner = owner.to_string();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;

            // Priority: recover an expired lease, then wake a parked run with
            // pending input, then a fresh pending run. SQLite has no SKIP LOCKED;
            // the IMMEDIATE transaction is the single-owner guard.
            let recovery = format!(
                "SELECT run_id, request FROM {p}_dispatch \
                 WHERE status = 'running' AND lease_until IS NOT NULL AND lease_until < ?1 \
                 ORDER BY created_at LIMIT 1"
            );
            let wake = format!(
                "SELECT run_id, request FROM {p}_dispatch d \
                 WHERE d.status = 'parked' AND EXISTS ( \
                     SELECT 1 FROM {p}_pending pe WHERE pe.run_id = d.run_id) \
                 ORDER BY created_at LIMIT 1"
            );
            let fresh = format!(
                "SELECT run_id, request FROM {p}_dispatch \
                 WHERE status = 'pending' ORDER BY created_at LIMIT 1"
            );

            let row = |sql: &str,
                       bind_now: bool|
             -> Result<Option<(String, String)>, DispatchError> {
                let map = |r: &rusqlite::Row| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?));
                if bind_now {
                    tx.query_row(sql, params![now_ms as i64], map)
                } else {
                    tx.query_row(sql, [], map)
                }
                .optional()
                .map_err(reject)
            };

            let picked = match row(&recovery, true)? {
                Some(found) => Some(found),
                None => match row(&wake, false)? {
                    Some(found) => Some(found),
                    None => row(&fresh, false)?,
                },
            };

            let Some((run_id, request_json)) = picked else {
                return Ok(None);
            };
            let request: RunExecutionRequest =
                serde_json::from_str(&request_json).map_err(json_err)?;

            let expires = now_ms + lease_ms;
            tx.execute(
                &format!(
                    "UPDATE {p}_dispatch SET status = 'running', lease_owner = ?1, \
                     lease_until = ?2 WHERE run_id = ?3"
                ),
                params![owner, expires as i64, run_id],
            )
            .map_err(reject)?;

            // Hand the run's current pending input to the worker (not removed
            // here: settle removes exactly what the worker reports it consumed).
            let pending = {
                let mut stmt = tx
                    .prepare(&format!(
                        "SELECT message_id, thread_id, correlation_id, result FROM {p}_pending \
                         WHERE run_id = ?1 ORDER BY created_at"
                    ))
                    .map_err(reject)?;
                let rows = stmt
                    .query_map(params![run_id], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                        ))
                    })
                    .map_err(reject)?;
                let mut pending = Vec::new();
                for row in rows {
                    let (message_id, thread_id, correlation_id, result) = row.map_err(reject)?;
                    pending.push(PendingInput {
                        message_id,
                        run_id: RunId(run_id.clone()),
                        thread_id: ThreadId(thread_id),
                        correlation_id,
                        result: serde_json::from_str(&result).map_err(json_err)?,
                    });
                }
                pending
            };

            tx.commit().map_err(reject)?;

            Ok(Some(Claimed {
                request,
                lease: Lease {
                    run_id: RunId(run_id),
                    owner,
                    expires_ms: expires,
                },
                pending,
            }))
        })
        .await
    }

    async fn settle(
        &self,
        run_id: &RunId,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<(), DispatchError> {
        let run_id = run_id.0.clone();
        let consumed = consumed.to_vec();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            match outcome {
                DispatchOutcome::Done => {
                    tx.execute(
                        &format!("DELETE FROM {p}_pending WHERE run_id = ?1"),
                        params![run_id],
                    )
                    .map_err(reject)?;
                    tx.execute(
                        &format!("DELETE FROM {p}_dispatch WHERE run_id = ?1"),
                        params![run_id],
                    )
                    .map_err(reject)?;
                }
                DispatchOutcome::Parked => {
                    for message_id in &consumed {
                        tx.execute(
                            &format!("DELETE FROM {p}_pending WHERE message_id = ?1"),
                            params![message_id],
                        )
                        .map_err(reject)?;
                    }
                    tx.execute(
                        &format!(
                            "UPDATE {p}_dispatch SET status = 'parked', lease_owner = NULL, \
                             lease_until = NULL WHERE run_id = ?1"
                        ),
                        params![run_id],
                    )
                    .map_err(reject)?;
                }
            }
            tx.commit().map_err(reject)?;
            Ok(())
        })
        .await
    }
}

#[async_trait]
impl PendingInbox for SqliteDispatchStore {
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let result_json = json(&input.result)?;
        self.with_conn(move |conn, p| {
            let changed = conn
                .execute(
                    &format!(
                        "INSERT INTO {p}_pending (message_id, run_id, thread_id, correlation_id, result) \
                         VALUES (?1,?2,?3,?4,?5) ON CONFLICT(message_id) DO NOTHING"
                    ),
                    params![
                        input.message_id,
                        input.run_id.0,
                        input.thread_id.0,
                        input.correlation_id,
                        result_json
                    ],
                )
                .map_err(reject)?;
            Ok(changed > 0)
        })
        .await
    }

    async fn list(&self, thread_id: &ThreadId) -> Result<Vec<PendingRecord>, DispatchError> {
        let thread = thread_id.clone();
        self.with_conn(move |conn, p| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT message_id, run_id, correlation_id, result, revision FROM {p}_pending \
                     WHERE thread_id = ?1 ORDER BY created_at"
                ))
                .map_err(reject)?;
            let rows = stmt
                .query_map(params![thread.0], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                })
                .map_err(reject)?;
            let mut records = Vec::new();
            for row in rows {
                let (message_id, run_id, correlation_id, result, revision) = row.map_err(reject)?;
                records.push(PendingRecord {
                    input: PendingInput {
                        message_id,
                        run_id: RunId(run_id),
                        thread_id: thread.clone(),
                        correlation_id,
                        result: serde_json::from_str(&result).map_err(json_err)?,
                    },
                    revision: revision as u64,
                });
            }
            Ok(records)
        })
        .await
    }

    async fn retract(
        &self,
        message_id: &str,
        expected_revision: u64,
    ) -> Result<CasOutcome, DispatchError> {
        let message_id = message_id.to_string();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let outcome = match current_revision(&tx, p, &message_id)? {
                None => CasOutcome::NotFound,
                Some(rev) if rev != expected_revision => CasOutcome::RevisionMismatch,
                Some(_) => {
                    tx.execute(
                        &format!("DELETE FROM {p}_pending WHERE message_id = ?1"),
                        params![message_id],
                    )
                    .map_err(reject)?;
                    CasOutcome::Applied
                }
            };
            tx.commit().map_err(reject)?;
            Ok(outcome)
        })
        .await
    }

    async fn edit(
        &self,
        message_id: &str,
        expected_revision: u64,
        result: ResumeResult,
    ) -> Result<CasOutcome, DispatchError> {
        let message_id = message_id.to_string();
        let result_json = json(&result)?;
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let outcome = match current_revision(&tx, p, &message_id)? {
                None => CasOutcome::NotFound,
                Some(rev) if rev != expected_revision => CasOutcome::RevisionMismatch,
                Some(_) => {
                    tx.execute(
                        &format!(
                            "UPDATE {p}_pending SET result = ?1, revision = revision + 1 \
                             WHERE message_id = ?2"
                        ),
                        params![result_json, message_id],
                    )
                    .map_err(reject)?;
                    CasOutcome::Applied
                }
            };
            tx.commit().map_err(reject)?;
            Ok(outcome)
        })
        .await
    }
}

fn current_revision(
    tx: &rusqlite::Transaction<'_>,
    prefix: &str,
    message_id: &str,
) -> Result<Option<u64>, DispatchError> {
    tx.query_row(
        &format!("SELECT revision FROM {prefix}_pending WHERE message_id = ?1"),
        params![message_id],
        |r| r.get::<_, i64>(0),
    )
    .optional()
    .map_err(reject)
    .map(|opt| opt.map(|r| r as u64))
}

fn json<T: serde::Serialize>(value: &T) -> Result<String, DispatchError> {
    serde_json::to_string(value).map_err(json_err)
}

fn json_err(err: serde_json::Error) -> DispatchError {
    DispatchError::Rejected(err.to_string())
}

fn reject(err: rusqlite::Error) -> DispatchError {
    DispatchError::Rejected(err.to_string())
}
