//! SQLite durable implementation of the runtime commit boundary.
//!
//! [`SqliteCommitCoordinator`] is the embedded sibling of the Postgres backend:
//! it implements the same neutral [`Coordinator`] write boundary (G1/G13) and the
//! unified `CommittedThreadView` read port against an in-process SQLite database,
//! using the *same* portable commit schema ([`awaken_store_schema`]). It satisfies
//! the identical commit contract (ADR-0006): the committed fact log is the
//! authority and the `run_record` table is a derived cache equal to the latest
//! fact (G32).
//!
//! SQLite's driver (`rusqlite`) is synchronous, so each `commit` runs the SQL
//! transaction on a blocking thread; the async write boundary is preserved.
//! Commits are serialized by a write lock, so the monotonic fence is assigned
//! without a race, mirroring SQLite's own single-writer model. The synchronous
//! read ports are served from an in-memory projection rebuilt from the log on
//! construction (durable across restart) and advanced in lockstep with each
//! commit — never an independent authority.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::state::Command as StateCommand;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::kind::Kind as AuditKind;
use awaken_agent_contract::audit::record::Record as EventRecord;
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator as CommitCoordinator, Error, OperationCoordinator,
};
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitPayloadHash, CommitReceipt,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::checkpoint::{CheckpointReader, EventScope};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::lifecycle::{
    RunLifecycleCursor, RunLifecycleEvent, RunLifecycleFeed, RunLifecycleFeedError,
    RunLifecyclePage, classify_run_lifecycle_record, decode_run_lifecycle_cursor,
    encode_run_lifecycle_cursor,
};
use awaken_agent_contract::thread::read::recovery::{
    RecoveryError, RunRecoverySnapshot, RunRecoverySource, RunResumeTicket,
    decode_resume_ticket_json_for_owner,
};
use awaken_store_schema::StoredU64;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

pub use awaken_store_schema::{COMMIT_BUNDLE_ID as BUNDLE_ID, commit_bundle};

fn encode_authority(value: u64) -> Result<i64, Error> {
    StoredU64::try_from(value)
        .map(StoredU64::database_value)
        .map_err(|error| Error::Rejected(error.to_string()))
}

fn decode_authority(value: i64) -> Result<u64, Error> {
    StoredU64::try_from(value)
        .map(StoredU64::domain_value)
        .map_err(|error| Error::Rejected(error.to_string()))
}

fn increment_authority(value: u64) -> Result<u64, Error> {
    StoredU64::try_from(value)
        .and_then(|value| value.checked_add(1))
        .map(StoredU64::domain_value)
        .map_err(|error| Error::Rejected(error.to_string()))
}

fn sqlite_authority(value: i64) -> Result<u64, rusqlite::Error> {
    StoredU64::try_from(value)
        .map(StoredU64::domain_value)
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })
}

/// Errors from constructing or migrating the store. Commit-time failures use the
/// neutral [`Coordinator`] error.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("open: {0}")]
    Open(String),
    #[error("migrate: {0}")]
    Migrate(String),
    #[error("hydrate: {0}")]
    Hydrate(String),
}

/// In-memory projection of committed truth, rebuilt on construction and advanced
/// with every commit. Serves the synchronous read ports.
#[derive(Debug, Default)]
struct Projection {
    sequence: u64,
    thread_versions: HashMap<ThreadId, u64>,
    run_commit_counts: HashMap<RunId, u64>,
    messages: Vec<(ThreadId, Message)>,
    /// Committed state commands per thread, in commit order (for `committed_state`).
    /// A resumed run rebuilds its materialized state from these durable rows.
    state: Vec<(ThreadId, StateCommand)>,
    run_records: HashMap<RunId, RunRecord>,
    /// The latest committed run per thread, in commit order (for `latest_run`).
    latest_by_thread: HashMap<ThreadId, RunRecord>,
    /// Committed events in commit order (for `list_events`). A fact-derived cache
    /// rebuilt from the durable event table on construction.
    events: Vec<EventRecord>,
    resume_tickets: HashMap<RunId, ResumeTicket>,
}

/// The component namespace for this runtime's tables (see the Postgres store).
/// Built in, not configured — one runtime is one component.
const NS: &str = "runtime";

/// A SQLite-backed [`Coordinator`] plus the read ports it serves.
pub struct SqliteCommitCoordinator {
    conn: Arc<Mutex<Connection>>,
    projection: Mutex<Projection>,
    /// Serializes commits so the fence is assigned without a race and the
    /// projection advances in commit order.
    write_lock: tokio::sync::Mutex<()>,
}

impl SqliteCommitCoordinator {
    /// Open (or create) a database file, apply the commit migrations, and
    /// hydrate the projection.
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let conn = awaken_sqlite_runtime::SqliteConnectionFactory::file(path)
            .open()
            .map_err(|err| StoreError::Open(err.to_string()))?;
        Self::from_connection(conn)
    }

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = awaken_sqlite_runtime::SqliteConnectionFactory::memory()
            .open()
            .map_err(|err| StoreError::Open(err.to_string()))?;
        Self::from_connection(conn)
    }

    /// Constrain this connection to its current page count for deterministic
    /// SQLITE_FULL atomicity tests. Production composition never exposes or
    /// invokes this test-only fault injection seam.
    #[cfg(feature = "test-support")]
    pub fn exhaust_growth_capacity_for_test(&self) -> Result<(), StoreError> {
        let connection = self
            .conn
            .lock()
            .map_err(|_| StoreError::Open("sqlite connection poisoned".into()))?;
        let pages: i64 = connection
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .map_err(|error| StoreError::Open(error.to_string()))?;
        connection
            .pragma_update(None, "max_page_count", pages)
            .map_err(|error| StoreError::Open(error.to_string()))
    }

    /// Force deterministic SQLITE_READONLY failures without relying on host
    /// filesystem ownership (test runners commonly execute as root).
    #[cfg(feature = "test-support")]
    pub fn make_read_only_for_test(&self) -> Result<(), StoreError> {
        let connection = self
            .conn
            .lock()
            .map_err(|_| StoreError::Open("sqlite connection poisoned".into()))?;
        connection
            .pragma_update(None, "query_only", true)
            .map_err(|error| StoreError::Open(error.to_string()))
    }

    fn from_connection(conn: Connection) -> Result<Self, StoreError> {
        let bundle = commit_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|err| StoreError::Migrate(err.to_string()))?
            .run_bundle(&conn, &bundle)
            .map_err(|err| StoreError::Migrate(err.to_string()))?;

        let projection = hydrate(&conn).map_err(|err| StoreError::Hydrate(err.to_string()))?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            projection: Mutex::new(projection),
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Number of commits applied so far (the durable fence).
    pub fn commit_count(&self) -> u64 {
        self.projection.lock().map(|p| p.sequence).unwrap_or(0)
    }

    /// Committed messages for a thread, in commit order.
    pub fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        self.projection
            .lock()
            .map(|p| {
                p.messages
                    .iter()
                    .filter(|(tid, _)| tid == thread_id)
                    .map(|(_, m)| m.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Committed state commands for a thread, in commit order — served from the
    /// durable `state_command` rows the projection rebuilt on open, so a resumed
    /// run replays its accumulated state (and usage/compaction accounting) from
    /// durable truth (G1/G13).
    pub fn committed_state(&self, thread_id: &ThreadId) -> Vec<StateCommand> {
        self.projection
            .lock()
            .map(|p| {
                p.state
                    .iter()
                    .filter(|(tid, _)| tid == thread_id)
                    .map(|(_, c)| c.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The active awaiting ticket for a run, if it is currently awaiting.
    pub fn resume_ticket_for(&self, run_id: &RunId) -> Option<ResumeTicket> {
        self.projection
            .lock()
            .ok()
            .and_then(|p| p.resume_tickets.get(run_id).cloned())
    }

    /// The latest run on `thread` when that run is awaiting, with its committed
    /// ticket. Older runs may retain historical waiting rows, but they cannot keep
    /// a thread awaiting after a newer run has completed. Read from hydrated durable
    /// truth so a rebuilt session preserves the same lifecycle boundary after a
    /// restart (G1/G13).
    pub fn open_wait_for_thread(&self, thread: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        self.projection.lock().ok().and_then(|p| {
            let latest = p.latest_by_thread.get(thread)?;
            if !matches!(latest.state, RunState::Awaiting) {
                return None;
            }
            let ticket = p.resume_tickets.get(&latest.id)?;
            (&ticket.thread_id == thread).then(|| (latest.id.clone(), ticket.clone()))
        })
    }
}

#[async_trait]
impl CommitCoordinator for SqliteCommitCoordinator {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, Error> {
        commit
            .validate()
            .map_err(|e| Error::Rejected(e.to_string()))?;
        // Serialize commits: assign the fence and advance the projection without
        // a race, matching SQLite's single-writer model.
        let _writing = self.write_lock.lock().await;
        let conn = self.conn.clone();
        let data = commit.clone();
        let next = tokio::task::spawn_blocking(move || {
            let mut guard = conn
                .lock()
                .map_err(|_| Error::Rejected("sqlite connection poisoned".to_string()))?;
            write_commit(&mut guard, &data)
        })
        .await
        .map_err(|err| Error::Rejected(err.to_string()))??;
        let mut projection = lock(&self.projection)?;
        advance_projection(&mut projection, commit, next)?;
        Ok(CommitRecord { sequence: next })
    }
}

#[async_trait]
impl OperationCoordinator for SqliteCommitCoordinator {
    async fn commit_operation(&self, operation: CommitOperation) -> Result<CommitReceipt, Error> {
        operation
            .commit
            .validate()
            .map_err(|error| Error::Rejected(error.to_string()))?;
        let _writing = self.write_lock.lock().await;
        let conn = self.conn.clone();
        let data = operation.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let mut guard = conn
                .lock()
                .map_err(|_| Error::Rejected("sqlite connection poisoned".to_string()))?;
            write_operation(&mut guard, &data)
        })
        .await
        .map_err(|error| Error::Rejected(error.to_string()))??;
        match outcome {
            OperationWrite::Duplicate(receipt) => Ok(receipt),
            OperationWrite::Applied(receipt) => {
                let mut projection = lock(&self.projection)?;
                advance_projection(&mut projection, operation.commit, receipt.commit_sequence)?;
                Ok(receipt)
            }
        }
    }
}

impl CommittedThreadView for SqliteCommitCoordinator {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        SqliteCommitCoordinator::committed_messages(self, thread_id)
    }

    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket> {
        self.resume_ticket_for(run_id)
    }

    fn run(&self, run_id: &RunId) -> Option<RunRecord> {
        self.projection
            .lock()
            .ok()
            .and_then(|projection| projection.run_records.get(run_id).cloned())
    }

    fn latest_run(&self, thread_id: &ThreadId) -> Option<RunRecord> {
        self.projection
            .lock()
            .ok()
            .and_then(|projection| projection.latest_by_thread.get(thread_id).cloned())
    }

    fn run_state(&self, run_id: &RunId) -> Option<RunState> {
        self.projection
            .lock()
            .ok()
            .and_then(|projection| projection.run_records.get(run_id).map(|r| r.state.clone()))
    }

    fn committed_state(&self, thread_id: &ThreadId) -> Vec<StateCommand> {
        SqliteCommitCoordinator::committed_state(self, thread_id)
    }
}

/// The merged read repository (ADR-0039 D1), served from the fact-derived
/// projection that `hydrate` rebuilt from the durable tables (D4).
impl CheckpointReader for SqliteCommitCoordinator {
    fn list_events(&self, scope: &EventScope, from: Option<u64>, limit: usize) -> Vec<EventRecord> {
        let after = from.unwrap_or(0);
        let projection = match self.projection.lock() {
            Ok(projection) => projection,
            Err(_) => return Vec::new(),
        };
        projection
            .events
            .iter()
            .filter(|event| event.sequence > after)
            .filter(|event| match scope {
                EventScope::All => true,
                EventScope::Run(run_id) => &event.run_id == run_id,
                EventScope::Thread(thread_id) => projection
                    .run_records
                    .get(&event.run_id)
                    .map(|record| &record.thread_id == thread_id)
                    .unwrap_or(false),
            })
            .take(limit)
            .cloned()
            .collect()
    }
}

/// Authoritative lifecycle feed for multiple processes sharing one SQLite file.
///
/// The synchronous committed execution view retains its process-local projection,
/// but lifecycle delivery must observe commits made by a peer Worker or Control
/// process. This query therefore reads durable event truth on every page, just
/// like the PostgreSQL implementation.
#[async_trait]
impl RunLifecycleFeed for SqliteCommitCoordinator {
    async fn events_after(
        &self,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunLifecycleFeedError> {
        if limit == 0 {
            return Ok(RunLifecyclePage {
                events: Vec::new(),
                next_cursor: cursor,
            });
        }
        let after = i64::try_from(cursor.0).map_err(|_| {
            RunLifecycleFeedError::Rejected("cursor exceeds SQLite INTEGER range".into())
        })?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = {
            let conn = self.conn.lock().map_err(|_| {
                RunLifecycleFeedError::Rejected("commit connection poisoned".into())
            })?;
            let mut statement = conn
                .prepare(&format!(
                    "WITH lifecycle AS (\
                         SELECT event.sequence, event.run_id, run.thread_id, event.kind, event.payload, \
                                (SELECT prior.payload FROM {NS}_event AS prior \
                                 WHERE prior.run_id = event.run_id \
                                   AND prior.sequence < event.sequence \
                                   AND prior.kind IN ('\"RunStateChanged\"', '\"RunPhaseChanged\"') \
                                 ORDER BY prior.sequence DESC LIMIT 1) AS previous_payload \
                         FROM {NS}_event AS event \
                         JOIN {NS}_run_record AS run ON run.run_id = event.run_id \
                         WHERE event.kind IN (\
                             '\"RunStateChanged\"', '\"RunPhaseChanged\"', '\"RunRescheduled\"'\
                         )\
                     ) \
                     SELECT sequence, run_id, thread_id, kind, payload, previous_payload \
                     FROM lifecycle WHERE sequence > ?1 ORDER BY sequence LIMIT ?2"
                ))
                .map_err(|error| RunLifecycleFeedError::Rejected(error.to_string()))?;
            let mapped = statement
                .query_map(params![after, limit], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                })
                .map_err(|error| RunLifecycleFeedError::Rejected(error.to_string()))?;
            mapped
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| RunLifecycleFeedError::Rejected(error.to_string()))?
        };

        let mut events = Vec::with_capacity(rows.len());
        for (sequence, run_id, thread_id, kind, payload, previous_payload) in rows {
            let sequence = u64::try_from(sequence).map_err(|_| {
                RunLifecycleFeedError::Rejected(
                    "persisted lifecycle sequence is negative".to_string(),
                )
            })?;
            let audit_kind = serde_json::from_str::<AuditKind>(&kind).map_err(|error| {
                RunLifecycleFeedError::Rejected(format!(
                    "persisted lifecycle event {sequence} has an invalid kind: {error}"
                ))
            })?;
            let payload = serde_json::from_str::<serde_json::Value>(&payload)
                .map_err(|_| RunLifecycleFeedError::InvalidState { sequence })?;
            let state = serde_json::from_value::<RunState>(
                payload.get("state").cloned().unwrap_or_default(),
            )
            .map_err(|_| RunLifecycleFeedError::InvalidState { sequence })?;
            let await_reason = payload
                .get("await_reason")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|_| RunLifecycleFeedError::InvalidState { sequence })?;
            let previous = previous_payload
                .map(|payload| {
                    serde_json::from_str::<serde_json::Value>(&payload)
                        .ok()
                        .and_then(|payload| payload.get("state").cloned())
                        .and_then(|state| serde_json::from_value::<RunState>(state).ok())
                        .ok_or(RunLifecycleFeedError::InvalidState { sequence })
                })
                .transpose()?;
            events.push(RunLifecycleEvent {
                cursor: RunLifecycleCursor(sequence),
                source_commit_cursor: decode_run_lifecycle_cursor(RunLifecycleCursor(sequence)).0,
                thread_id: ThreadId(thread_id),
                run_id: RunId(run_id),
                kind: classify_run_lifecycle_record(&audit_kind, &state, previous.as_ref())
                    .ok_or_else(|| {
                        RunLifecycleFeedError::Rejected(format!(
                            "persisted lifecycle event {sequence} has a non-lifecycle kind"
                        ))
                    })?,
                state,
                await_reason,
            });
        }
        let next_cursor = events.last().map_or(cursor, |event| event.cursor);
        Ok(RunLifecyclePage {
            events,
            next_cursor,
        })
    }
}

#[async_trait]
impl RunRecoverySource for SqliteCommitCoordinator {
    async fn recovery_snapshot(
        &self,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| RecoveryError::Rejected("commit connection poisoned".to_string()))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(recovery_reject)?;
        let store_cursor: i64 = tx
            .query_row(
                &format!("SELECT COALESCE(MAX(sequence), 0) FROM {NS}_commit"),
                [],
                |row| row.get(0),
            )
            .map_err(recovery_reject)?;
        let thread_version: i64 = tx
            .query_row(
                &format!("SELECT COUNT(*) FROM {NS}_commit WHERE thread_id = ?1"),
                params![&thread_id.0],
                |row| row.get(0),
            )
            .map_err(recovery_reject)?;
        let next_commit_ordinal: i64 = tx
            .query_row(
                &format!("SELECT COUNT(*) FROM {NS}_commit WHERE run_id = ?1"),
                params![&claimed_run_id.0],
                |row| row.get(0),
            )
            .map_err(recovery_reject)?;

        let mut runs = Vec::<RunRecord>::new();
        let mut latest_run_id = None;
        {
            let mut statement = tx
                .prepare(&format!(
                    "SELECT run_id, phase FROM {NS}_commit \
                     WHERE thread_id = ?1 ORDER BY sequence"
                ))
                .map_err(recovery_reject)?;
            let rows = statement
                .query_map(params![&thread_id.0], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(recovery_reject)?;
            for row in rows {
                let (run_id, state) = row.map_err(recovery_reject)?;
                let run_id = RunId(run_id);
                let state = serde_json::from_str::<RunState>(&state).map_err(recovery_reject)?;
                let record = RunRecord {
                    id: run_id.clone(),
                    thread_id: thread_id.clone(),
                    state,
                };
                latest_run_id = Some(run_id.clone());
                if let Some(existing) = runs.iter_mut().find(|existing| existing.id == run_id) {
                    *existing = record;
                } else {
                    runs.push(record);
                }
            }
        }

        let (message_commit_cursors, messages) =
            read_thread_json_rows_with_commit::<Message>(&tx, "message", thread_id)?;
        let (state_commit_cursors, state) =
            read_thread_json_rows_with_commit::<StateCommand>(&tx, "state_command", thread_id)?;
        let mut events = Vec::new();
        {
            let mut statement = tx
                .prepare(&format!(
                    "SELECT event.sequence, event.run_id, event.kind, event.payload \
                     FROM {NS}_event AS event \
                     JOIN {NS}_run_record AS run ON run.run_id = event.run_id \
                     WHERE run.thread_id = ?1 ORDER BY event.sequence"
                ))
                .map_err(recovery_reject)?;
            let rows = statement
                .query_map(params![&thread_id.0], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(recovery_reject)?;
            for row in rows {
                let (sequence, run_id, kind, payload) = row.map_err(recovery_reject)?;
                events.push(EventRecord {
                    sequence: StoredU64::try_from(sequence)
                        .map(StoredU64::domain_value)
                        .map_err(recovery_reject)?,
                    run_id: RunId(run_id),
                    kind: serde_json::from_str(&kind).map_err(recovery_reject)?,
                    payload: serde_json::from_str(&payload).map_err(recovery_reject)?,
                });
            }
        }
        let mut resume_tickets = Vec::new();
        {
            let mut statement = tx
                .prepare(&format!(
                    "SELECT waiting.run_id, waiting.ticket \
                     FROM {NS}_waiting AS waiting \
                     JOIN {NS}_run_record AS run ON run.run_id = waiting.run_id \
                     WHERE run.thread_id = ?1 ORDER BY waiting.run_id"
                ))
                .map_err(recovery_reject)?;
            let rows = statement
                .query_map(params![&thread_id.0], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(recovery_reject)?;
            for row in rows {
                let (run_id, ticket) = row.map_err(recovery_reject)?;
                let run_id = RunId(run_id);
                let is_awaiting = runs
                    .iter()
                    .any(|run| run.id == run_id && matches!(run.state, RunState::Awaiting));
                if !is_awaiting {
                    tracing::error!(
                        awaken.run.id = %run_id.0,
                        awaken.thread.id = %thread_id.0,
                        "quarantining a stale waiting row whose Run is not Awaiting"
                    );
                    continue;
                }
                if let Some(ticket) = decode_resume_ticket(&run_id, thread_id, &ticket) {
                    resume_tickets.push(RunResumeTicket { run_id, ticket });
                }
            }
        }
        tx.commit().map_err(recovery_reject)?;
        Ok(RunRecoverySnapshot {
            thread_id: thread_id.clone(),
            claimed_run_id: claimed_run_id.clone(),
            runs,
            latest_run_id,
            messages,
            message_commit_cursors,
            state,
            state_commit_cursors,
            events,
            resume_tickets,
            thread_version: StoredU64::try_from(thread_version)
                .map(StoredU64::domain_value)
                .map_err(recovery_reject)?,
            store_cursor: StoredU64::try_from(store_cursor)
                .map(StoredU64::domain_value)
                .map_err(recovery_reject)?,
            next_commit_ordinal: StoredU64::try_from(next_commit_ordinal)
                .map(StoredU64::domain_value)
                .map_err(recovery_reject)?,
        })
    }
}

fn read_thread_json_rows_with_commit<T>(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    thread_id: &ThreadId,
) -> Result<(Vec<u64>, Vec<T>), RecoveryError>
where
    T: serde::de::DeserializeOwned,
{
    let mut statement = tx
        .prepare(&format!(
            "SELECT commit_sequence, data FROM {NS}_{table} WHERE thread_id = ?1 ORDER BY id"
        ))
        .map_err(recovery_reject)?;
    let rows = statement
        .query_map(params![&thread_id.0], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(recovery_reject)?;
    let mut cursors = Vec::new();
    let mut values = Vec::new();
    for row in rows {
        let (cursor, value) = row.map_err(recovery_reject)?;
        cursors.push(
            StoredU64::try_from(cursor)
                .map(StoredU64::domain_value)
                .map_err(recovery_reject)?,
        );
        values.push(serde_json::from_str(&value).map_err(recovery_reject)?);
    }
    Ok((cursors, values))
}

fn recovery_reject(error: impl ToString) -> RecoveryError {
    RecoveryError::Rejected(error.to_string())
}

/// Decode one isolated waiting row without making the Thread's complete
/// recovery snapshot unavailable. The row is intentionally retained in SQLite;
/// Runtime interruption will either reconstruct from the other durable facts or
/// terminalize through the canonical ThreadCommit boundary.
fn decode_resume_ticket(
    row_run_id: &RunId,
    expected_thread_id: &ThreadId,
    raw: &str,
) -> Option<ResumeTicket> {
    match decode_resume_ticket_json_for_owner(raw, row_run_id, expected_thread_id) {
        Ok(ticket) => Some(ticket),
        Err(error) => {
            tracing::error!(
                awaken.run.id = %row_run_id.0,
                awaken.thread.id = %expected_thread_id.0,
                %error,
                "quarantining an invalid ResumeTicket while retaining Thread recovery facts"
            );
            None
        }
    }
}

fn lock(projection: &Mutex<Projection>) -> Result<std::sync::MutexGuard<'_, Projection>, Error> {
    projection
        .lock()
        .map_err(|_| Error::Rejected("commit projection poisoned".to_string()))
}

enum OperationWrite {
    Applied(CommitReceipt),
    Duplicate(CommitReceipt),
}

/// Write one staged commit in a single IMMEDIATE transaction (atomic, G1/G13).
fn write_commit(conn: &mut Connection, commit: &ThreadCommit) -> Result<u64, Error> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(reject)?;
    validate_transition_tx(&tx, commit)?;
    ensure_thread_version(&tx, &commit.thread_id)?;
    let current: i64 = tx
        .query_row(
            &format!("SELECT COALESCE(MAX(sequence), 0) FROM {NS}_commit"),
            [],
            |row| row.get(0),
        )
        .map_err(reject)?;
    let next = increment_authority(decode_authority(current)?)?;
    write_commit_rows(&tx, next, commit)?;
    let current_version: i64 = tx
        .query_row(
            &format!("SELECT version FROM {NS}_thread_version WHERE thread_id = ?1"),
            params![&commit.thread_id.0],
            |row| row.get(0),
        )
        .map_err(reject)?;
    let next_version = increment_authority(decode_authority(current_version)?)?;
    tx.execute(
        &format!("UPDATE {NS}_thread_version SET version = ?2 WHERE thread_id = ?1"),
        params![&commit.thread_id.0, encode_authority(next_version)?],
    )
    .map_err(reject)?;
    tx.commit().map_err(reject)?;
    Ok(next)
}

fn write_operation(
    conn: &mut Connection,
    operation: &CommitOperation,
) -> Result<OperationWrite, Error> {
    if operation.operation_id.run_id != *operation.commit.run_id() {
        return Err(Error::Rejected(
            "commit operation run_id does not match ThreadCommit run_id".to_string(),
        ));
    }
    let operation_ordinal = i64::try_from(operation.operation_id.ordinal).map_err(|_| {
        Error::Rejected("commit operation ordinal exceeds backend range".to_string())
    })?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(reject)?;
    let existing = tx
        .query_row(
            &format!(
                "SELECT payload_hash, commit_sequence, thread_version \
                 FROM {NS}_commit_receipt \
                 WHERE operation_run_id = ?1 AND operation_ordinal = ?2"
            ),
            params![&operation.operation_id.run_id.0, operation_ordinal],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(reject)?;
    if let Some((payload_hash, commit_sequence, thread_version)) = existing {
        if payload_hash != operation.payload_hash.0 {
            return Err(Error::Rejected(format!(
                "commit operation {}:{} was reused with another payload",
                operation.operation_id.run_id.0, operation.operation_id.ordinal
            )));
        }
        return Ok(OperationWrite::Duplicate(CommitReceipt {
            operation_id: operation.operation_id.clone(),
            commit_sequence: decode_authority(commit_sequence)?,
            thread_version: decode_authority(thread_version)?,
            payload_hash: CommitPayloadHash(payload_hash),
            duplicate: true,
        }));
    }

    ensure_thread_version(&tx, &operation.commit.thread_id)?;
    let current_version: i64 = tx
        .query_row(
            &format!("SELECT version FROM {NS}_thread_version WHERE thread_id = ?1"),
            params![&operation.commit.thread_id.0],
            |row| row.get(0),
        )
        .map_err(reject)?;
    let current_version = decode_authority(current_version)?;
    if current_version != operation.expected_thread_version {
        return Err(Error::Rejected(format!(
            "thread version conflict: expected {}, current {}",
            operation.expected_thread_version, current_version
        )));
    }
    let current_ordinal: i64 = tx
        .query_row(
            &format!("SELECT COUNT(*) FROM {NS}_commit WHERE run_id = ?1"),
            params![&operation.operation_id.run_id.0],
            |row| row.get(0),
        )
        .map_err(reject)?;
    let current_ordinal = decode_authority(current_ordinal)?;
    if current_ordinal != operation.operation_id.ordinal {
        return Err(Error::Rejected(format!(
            "commit operation ordinal conflict: expected {}, current {}",
            operation.operation_id.ordinal, current_ordinal
        )));
    }
    validate_transition_tx(&tx, &operation.commit)?;
    let current_sequence: i64 = tx
        .query_row(
            &format!("SELECT COALESCE(MAX(sequence), 0) FROM {NS}_commit"),
            [],
            |row| row.get(0),
        )
        .map_err(reject)?;
    let next = increment_authority(decode_authority(current_sequence)?)?;
    write_commit_rows(&tx, next, &operation.commit)?;
    let thread_version = increment_authority(current_version)?;
    tx.execute(
        &format!("UPDATE {NS}_thread_version SET version = ?2 WHERE thread_id = ?1"),
        params![&operation.commit.thread_id.0, thread_version],
    )
    .map_err(reject)?;
    tx.execute(
        &format!(
            "INSERT INTO {NS}_commit_receipt \
             (operation_run_id, operation_ordinal, thread_id, payload_hash, \
              commit_sequence, thread_version) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
        ),
        params![
            &operation.operation_id.run_id.0,
            operation_ordinal,
            &operation.commit.thread_id.0,
            &operation.payload_hash.0,
            encode_authority(next)?,
            encode_authority(thread_version)?
        ],
    )
    .map_err(reject)?;
    tx.commit().map_err(reject)?;
    Ok(OperationWrite::Applied(CommitReceipt {
        operation_id: operation.operation_id.clone(),
        commit_sequence: next,
        thread_version,
        payload_hash: operation.payload_hash.clone(),
        duplicate: false,
    }))
}

fn ensure_thread_version(
    tx: &rusqlite::Transaction<'_>,
    thread_id: &ThreadId,
) -> Result<(), Error> {
    tx.execute(
        &format!(
            "INSERT OR IGNORE INTO {NS}_thread_version (thread_id, version) \
             SELECT ?1, COUNT(*) FROM {NS}_commit WHERE thread_id = ?1"
        ),
        params![&thread_id.0],
    )
    .map_err(reject)?;
    Ok(())
}

fn validate_transition_tx(
    tx: &rusqlite::Transaction<'_>,
    commit: &ThreadCommit,
) -> Result<(), Error> {
    let phase: Option<String> = tx
        .query_row(
            &format!("SELECT phase FROM {NS}_run_record WHERE run_id = ?1"),
            params![&commit.run_id().0],
            |row| row.get(0),
        )
        .optional()
        .map_err(reject)?;
    if phase
        .as_deref()
        .and_then(|phase| serde_json::from_str::<RunState>(phase).ok())
        .is_some_and(|state| !state.permits(&commit.run_state()))
    {
        return Err(Error::Rejected(format!(
            "run {} is already terminal; refusing post-terminal commit",
            commit.run_id().0
        )));
    }
    Ok(())
}

/// JSON columns are stored as serialized text — the schema renders `{json}` to
/// TEXT on SQLite.
fn write_commit_rows(
    tx: &rusqlite::Transaction<'_>,
    next: u64,
    commit: &ThreadCommit,
) -> Result<(), Error> {
    let p = NS;
    let run_id = &commit.run_id().0;
    let thread_id = &commit.thread_id.0;
    let run_state = commit.run_state();
    let state_json = json(&run_state)?;

    tx.execute(
        &format!(
            "INSERT INTO {p}_commit (sequence, thread_id, run_id, phase) VALUES (?1,?2,?3,?4)"
        ),
        params![encode_authority(next)?, thread_id, run_id, state_json],
    )
    .map_err(reject)?;

    for message in &commit.messages {
        tx.execute(
            &format!(
                "INSERT INTO {p}_message (commit_sequence, thread_id, data) VALUES (?1,?2,?3)"
            ),
            params![encode_authority(next)?, thread_id, json(message)?],
        )
        .map_err(reject)?;
    }

    for command in &commit.state {
        tx.execute(
            &format!(
                "INSERT INTO {p}_state_command (commit_sequence, thread_id, data) VALUES (?1,?2,?3)"
            ),
            params![encode_authority(next)?, thread_id, json(command)?],
        )
        .map_err(reject)?;
    }

    for (offset, draft) in commit.events.iter().enumerate() {
        let sequence = encode_run_lifecycle_cursor(next, offset)
            .map_err(|error| Error::Rejected(error.to_string()))?;
        tx.execute(
            &format!(
                "INSERT INTO {p}_event (sequence, run_id, kind, payload) VALUES (?1,?2,?3,?4)"
            ),
            params![
                encode_authority(sequence.0)?,
                run_id,
                json(&draft.kind)?,
                json(&draft.payload)?
            ],
        )
        .map_err(reject)?;
    }

    tx.execute(
        &format!(
            "INSERT INTO {p}_run_record (run_id, thread_id, phase) VALUES (?1,?2,?3) \
             ON CONFLICT(run_id) DO UPDATE SET thread_id = excluded.thread_id, \
             phase = excluded.phase, updated_at = CURRENT_TIMESTAMP"
        ),
        params![run_id, thread_id, state_json],
    )
    .map_err(reject)?;

    // Await or clear the awaiting ticket atomically with the checkpoint (G5).
    let awaiting = matches!(run_state, RunState::Awaiting);
    if awaiting {
        let ticket = commit
            .resume_ticket()
            .expect("awaiting checkpoint has a ticket");
        tx.execute(
            &format!(
                "INSERT INTO {p}_waiting (run_id, ticket) VALUES (?1,?2) \
                 ON CONFLICT(run_id) DO UPDATE SET ticket = excluded.ticket"
            ),
            params![run_id, json(ticket)?],
        )
        .map_err(reject)?;
    } else {
        tx.execute(
            &format!("DELETE FROM {p}_waiting WHERE run_id = ?1"),
            params![run_id],
        )
        .map_err(reject)?;
    }

    Ok(())
}

fn advance_projection(
    projection: &mut Projection,
    commit: ThreadCommit,
    sequence: u64,
) -> Result<(), Error> {
    let run_state = commit.run_state();
    let run_id = commit.run_id().clone();
    let thread_id = commit.thread_id.clone();
    let resume_ticket = commit.resume_ticket().cloned();
    let awaiting = matches!((&resume_ticket, &run_state), (Some(_), RunState::Awaiting));

    projection.sequence = projection.sequence.max(sequence);
    let thread_version = projection
        .thread_versions
        .entry(thread_id.clone())
        .or_default();
    *thread_version = increment_authority(*thread_version)?;
    let run_commit_count = projection
        .run_commit_counts
        .entry(run_id.clone())
        .or_default();
    *run_commit_count = increment_authority(*run_commit_count)?;
    for message in commit.messages {
        projection.messages.push((thread_id.clone(), message));
    }
    for command in commit.state {
        projection.state.push((thread_id.clone(), command));
    }
    for (offset, draft) in commit.events.into_iter().enumerate() {
        let cursor = encode_run_lifecycle_cursor(sequence, offset)
            .map_err(|error| Error::Rejected(error.to_string()))?;
        projection.events.push(EventRecord {
            sequence: cursor.0,
            run_id: run_id.clone(),
            kind: draft.kind,
            payload: draft.payload,
        });
    }
    let record = RunRecord {
        id: run_id.clone(),
        thread_id: thread_id.clone(),
        state: run_state,
    };
    projection
        .run_records
        .insert(run_id.clone(), record.clone());
    projection.latest_by_thread.insert(thread_id, record);
    if awaiting {
        projection
            .resume_tickets
            .insert(run_id, resume_ticket.expect("awaiting has a ticket"));
    } else {
        projection.resume_tickets.remove(&run_id);
    }
    Ok(())
}

/// Rebuild the read projection from the committed log in SQLite.
fn hydrate(conn: &Connection) -> Result<Projection, rusqlite::Error> {
    let sequence = conn.query_row(
        &format!("SELECT COALESCE(MAX(sequence), 0) FROM {NS}_commit"),
        [],
        |row| row.get::<_, i64>(0),
    )?;
    let sequence = sqlite_authority(sequence)?;
    let mut projection = Projection {
        sequence,
        ..Default::default()
    };

    let mut stmt = conn.prepare(&format!(
        "SELECT thread_id, data FROM {NS}_message ORDER BY id"
    ))?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (thread_id, data) = row?;
        if let Ok(message) = serde_json::from_str::<Message>(&data) {
            projection.messages.push((ThreadId(thread_id), message));
        }
    }

    // Rebuild the committed state-command log per thread, in commit order, so a
    // resumed run replays its accumulated state from durable truth (G1/G13).
    let mut stmt = conn.prepare(&format!(
        "SELECT thread_id, data FROM {NS}_state_command ORDER BY id"
    ))?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (thread_id, data) = row?;
        if let Ok(command) = serde_json::from_str::<StateCommand>(&data) {
            projection.state.push((ThreadId(thread_id), command));
        }
    }

    // Fold the commit log in order so the latest fact per run wins (G32).
    let mut stmt = conn.prepare(&format!(
        "SELECT run_id, thread_id, phase FROM {NS}_commit ORDER BY sequence"
    ))?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (run_id, thread_id, state_json) = row?;
        if let Ok(state) = serde_json::from_str::<RunState>(&state_json) {
            let thread_version = projection
                .thread_versions
                .entry(ThreadId(thread_id.clone()))
                .or_default();
            *thread_version = StoredU64::try_from(*thread_version)
                .and_then(|value| value.checked_add(1))
                .map(StoredU64::domain_value)
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Integer,
                        Box::new(error),
                    )
                })?;
            let run_commit_count = projection
                .run_commit_counts
                .entry(RunId(run_id.clone()))
                .or_default();
            *run_commit_count = StoredU64::try_from(*run_commit_count)
                .and_then(|value| value.checked_add(1))
                .map(StoredU64::domain_value)
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Integer,
                        Box::new(error),
                    )
                })?;
            let record = RunRecord {
                id: RunId(run_id.clone()),
                thread_id: ThreadId(thread_id.clone()),
                state,
            };
            projection.run_records.insert(RunId(run_id), record.clone());
            // Sequence order → the last commit on a thread wins as its latest run.
            projection
                .latest_by_thread
                .insert(ThreadId(thread_id), record);
        }
    }

    // Rebuild the committed event cache from the durable event log, in order.
    let mut stmt = conn.prepare(&format!(
        "SELECT sequence, run_id, kind, payload FROM {NS}_event ORDER BY sequence"
    ))?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (sequence, run_id, kind, payload) = row?;
        if let (Ok(kind), Ok(payload)) = (
            serde_json::from_str(&kind),
            serde_json::from_str::<serde_json::Value>(&payload),
        ) {
            projection.events.push(EventRecord {
                sequence: sqlite_authority(sequence)?,
                run_id: RunId(run_id),
                kind,
                payload,
            });
        }
    }

    let mut stmt = conn.prepare(&format!("SELECT run_id, ticket FROM {NS}_waiting"))?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (run_id, ticket) = row?;
        let run_id = RunId(run_id);
        let Some(record) = projection.run_records.get(&run_id) else {
            tracing::error!(
                awaken.run.id = %run_id.0,
                "quarantining a waiting row whose Run record is absent"
            );
            continue;
        };
        if !matches!(record.state, RunState::Awaiting) {
            tracing::error!(
                awaken.run.id = %run_id.0,
                awaken.thread.id = %record.thread_id.0,
                "quarantining a stale waiting row whose Run is not Awaiting"
            );
            continue;
        }
        if let Some(ticket) = decode_resume_ticket(&run_id, &record.thread_id, &ticket) {
            projection.resume_tickets.insert(run_id, ticket);
        }
    }

    Ok(projection)
}

fn json<T: serde::Serialize>(value: &T) -> Result<String, Error> {
    serde_json::to_string(value).map_err(|err| Error::Rejected(err.to_string()))
}

fn reject(err: rusqlite::Error) -> Error {
    Error::Rejected(err.to_string())
}
