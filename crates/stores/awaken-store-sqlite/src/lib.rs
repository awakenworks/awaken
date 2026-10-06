// @formal entrypoint background_task.never_replay_and_attempt_exhaustion_fail_closed
// @formal entrypoint checkpoint.crash_preserves_durable_truth
// @formal entrypoint remote_worker.operation_identity_maps_to_one_hash_and_append
// @formal entrypoint run.commit_shape_atomicity
// @formal entrypoint store_history.commits_form_one_dense_stable_global_order
// @formal entrypoint store_history.concurrent_compare_and_swap_has_one_winner_per_thread_version
// @formal entrypoint store_history.exact_retry_and_every_rejection_are_atomic_stutters
// @formal entrypoint store_history.receipt_refines_one_exact_durable_record
// @formal entrypoint store_history.recovery_preserves_committed_time_and_receipt_identity
// @formal entrypoint store_history.terminal_run_is_final_for_every_backend_history
// @formal entrypoint thread_state.atomic_commit_and_replay
// @formal evidence outcome.stable_run_identity_and_exactly_once_ack
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
//! The store reaches SQLite through one sqlx connection opened under the
//! embedded policy (ADR-0087), so each `commit` is one async `BEGIN IMMEDIATE`
//! transaction. Commits are serialized by a write lock, so the monotonic fence
//! is assigned without a race, mirroring SQLite's own single-writer model. The
//! synchronous read ports are served from an in-memory projection rebuilt from
//! the log on construction (durable across restart) and advanced in lockstep
//! with each commit — never an independent authority.

use std::collections::HashMap;
use std::sync::Mutex;

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
    CommitOperation, CommitPayloadHash, CommitReceipt, admit_commit_operation_append,
    applied_commit_receipt, replay_commit_operation_receipt, require_commit_operation_identity,
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
    decode_resume_ticket_json_for_owner, project_committed_run_records,
};
use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
use awaken_sqlite_pool::EmbeddedPool;
use awaken_sqlite_pool::EmbeddedSqlite;
use awaken_store_schema::StoredU64;
use sqlx::SqliteConnection;

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

fn sqlite_authority(value: i64) -> Result<u64, String> {
    StoredU64::try_from(value)
        .map(StoredU64::domain_value)
        .map_err(|error| error.to_string())
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
    pool: EmbeddedPool,
    projection: Mutex<Projection>,
    /// Serializes commits so the fence is assigned without a race and the
    /// projection advances in commit order.
    write_lock: tokio::sync::Mutex<()>,
}

impl SqliteCommitCoordinator {
    /// Open (or create) a database file, apply the commit migrations, and
    /// hydrate the projection.
    pub async fn open(path: &str) -> Result<Self, StoreError> {
        Self::open_location(EmbeddedSqlite::file(path)).await
    }

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_in_memory() -> Result<Self, StoreError> {
        Self::open_location(EmbeddedSqlite::memory()).await
    }

    /// Constrain this connection to its current page count for deterministic
    /// SQLITE_FULL atomicity tests. Production composition never exposes or
    /// invokes this test-only fault injection seam.
    #[cfg(feature = "test-support")]
    pub async fn exhaust_growth_capacity_for_test(&self) -> Result<(), StoreError> {
        // The store owns exactly one connection, so a connection-local pragma
        // set through the pool stays on every later operation.
        let mut connection = self
            .pool
            .acquire()
            .await
            .map_err(|error| StoreError::Open(error.to_string()))?;
        let pages: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| StoreError::Open(error.to_string()))?;
        sqlx::query(&format!("PRAGMA max_page_count = {pages}"))
            .execute(&mut *connection)
            .await
            .map(drop)
            .map_err(|error| StoreError::Open(error.to_string()))
    }

    /// Force deterministic SQLITE_READONLY failures without relying on host
    /// filesystem ownership (test runners commonly execute as root).
    #[cfg(feature = "test-support")]
    pub async fn make_read_only_for_test(&self) -> Result<(), StoreError> {
        sqlx::query("PRAGMA query_only = 1")
            .execute(&self.pool)
            .await
            .map(drop)
            .map_err(|error| StoreError::Open(error.to_string()))
    }

    async fn open_location(location: EmbeddedSqlite) -> Result<Self, StoreError> {
        let pool = location
            .open()
            .await
            .map_err(|err| StoreError::Open(err.to_string()))?;
        match Self::from_pool(pool.clone()).await {
            Ok(store) => Ok(store),
            Err(error) => {
                pool.close().await;
                Err(error)
            }
        }
    }

    async fn from_pool(pool: EmbeddedPool) -> Result<Self, StoreError> {
        let bundle = commit_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        awaken_sqlite_pool::held(
            SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS)
                .map_err(|err| StoreError::Migrate(err.to_string()))?
                .run_bundle(&bundle),
        )
        .await
        .map_err(|err| StoreError::Migrate(err.to_string()))?;

        let projection = hydrate(&pool).await.map_err(StoreError::Hydrate)?;

        Ok(Self {
            pool,
            projection: Mutex::new(projection),
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Close the store's connection and wait until SQLite has released the
    /// file. Dropping the store also closes it, but in the background; a
    /// caller that next inspects or rewrites the file closes it first.
    pub async fn close(&self) {
        self.pool.close().await;
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
        let next = write_commit(&self.pool, &commit).await?;
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
        let outcome = write_operation(&self.pool, &operation).await?;
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

    fn open_wait_for_thread(&self, thread_id: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        self.projection.lock().ok().and_then(|projection| {
            let latest = projection.latest_by_thread.get(thread_id)?;
            awaken_agent_contract::thread::read::committed_thread_view::select_open_wait(
                thread_id,
                Some(latest),
                projection.resume_tickets.get(&latest.id),
            )
        })
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
#[async_trait]
impl CheckpointReader for SqliteCommitCoordinator {
    async fn list_events(
        &self,
        scope: &EventScope,
        from: Option<u64>,
        limit: usize,
    ) -> Result<Vec<EventRecord>, String> {
        let after = from.unwrap_or(0);
        let projection = match self.projection.lock() {
            Ok(projection) => projection,
            Err(_) => return Err("commit projection poisoned".to_owned()),
        };
        Ok(projection
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
            .collect())
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
        let sql = format!(
            "WITH lifecycle AS (\
                 SELECT event.sequence, event.run_id, run.thread_id, event.kind, event.payload, \
                        (SELECT prior.payload FROM {NS}_event AS prior \
                         WHERE prior.run_id = event.run_id \
                           AND prior.sequence < event.sequence \
                           AND prior.kind = '\"RunStateChanged\"' \
                         ORDER BY prior.sequence DESC LIMIT 1) AS previous_payload \
                 FROM {NS}_event AS event \
                 JOIN {NS}_run_record AS run ON run.run_id = event.run_id \
                 WHERE event.kind IN ('\"RunStateChanged\"', '\"RunRescheduled\"')\
             ) \
             SELECT sequence, run_id, thread_id, kind, payload, previous_payload \
             FROM lifecycle WHERE sequence > ?1 ORDER BY sequence LIMIT ?2"
        );
        let rows = sqlx::query_as::<_, (i64, String, String, String, String, Option<String>)>(&sql)
            .bind(after)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| RunLifecycleFeedError::Rejected(error.to_string()))?;

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
        read_recovery_snapshot(&self.pool, thread_id.clone(), claimed_run_id.clone()).await
    }
}

async fn read_recovery_snapshot(
    pool: &EmbeddedPool,
    thread_id: ThreadId,
    claimed_run_id: RunId,
) -> Result<RunRecoverySnapshot, RecoveryError> {
    let mut tx = pool.begin().await.map_err(recovery_reject)?;
    let store_cursor: i64 = sqlx::query_scalar(&format!(
        "SELECT COALESCE(MAX(sequence), 0) FROM {NS}_commit"
    ))
    .fetch_one(&mut *tx)
    .await
    .map_err(recovery_reject)?;
    let thread_version: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {NS}_commit WHERE thread_id = ?1"
    ))
    .bind(&thread_id.0)
    .fetch_one(&mut *tx)
    .await
    .map_err(recovery_reject)?;
    let next_commit_ordinal: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {NS}_commit WHERE run_id = ?1"
    ))
    .bind(&claimed_run_id.0)
    .fetch_one(&mut *tx)
    .await
    .map_err(recovery_reject)?;

    let (runs, latest_run_id) = {
        let rows = sqlx::query_as::<_, (String, String)>(&format!(
            "SELECT run_id, phase FROM {NS}_commit \
                 WHERE thread_id = ?1 ORDER BY sequence"
        ))
        .bind(&thread_id.0)
        .fetch_all(&mut *tx)
        .await
        .map_err(recovery_reject)?;
        project_committed_run_records(
            &thread_id,
            rows.into_iter().map(|(run_id, state)| {
                let state = serde_json::from_str::<RunState>(&state).map_err(recovery_reject)?;
                Ok::<_, RecoveryError>((RunId(run_id), state))
            }),
        )?
    };

    let (message_commit_cursors, messages) =
        read_thread_json_rows_with_commit::<Message>(&mut tx, "message", &thread_id).await?;
    let (state_commit_cursors, state) =
        read_thread_json_rows_with_commit::<StateCommand>(&mut tx, "state_command", &thread_id)
            .await?;
    // The committed Run set owns this Thread's event/ticket scope. A subquery
    // uses the same transaction without one bind per Run or a cache-owner join.
    let mut events = Vec::new();
    {
        let rows = sqlx::query_as::<_, (i64, String, String, String)>(&format!(
            "SELECT event.sequence, event.run_id, event.kind, event.payload \
                 FROM {NS}_event AS event \
                 WHERE event.run_id IN (SELECT run_id FROM {NS}_commit WHERE thread_id = ?1) \
                 ORDER BY event.sequence"
        ))
        .bind(&thread_id.0)
        .fetch_all(&mut *tx)
        .await
        .map_err(recovery_reject)?;
        for (sequence, run_id, kind, payload) in rows {
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
        let rows = sqlx::query_as::<_, (String, String)>(&format!(
            "SELECT waiting.run_id, waiting.ticket \
                 FROM {NS}_waiting AS waiting \
                 WHERE waiting.run_id IN (SELECT run_id FROM {NS}_commit WHERE thread_id = ?1) \
                 ORDER BY waiting.run_id"
        ))
        .bind(&thread_id.0)
        .fetch_all(&mut *tx)
        .await
        .map_err(recovery_reject)?;
        for (run_id, ticket) in rows {
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
            if let Some(ticket) = decode_resume_ticket(&run_id, &thread_id, &ticket) {
                resume_tickets.push(RunResumeTicket { run_id, ticket });
            }
        }
    }
    let commit_timestamps = sqlx::query_as::<_, (i64, i64)>(&format!(
        "SELECT sequence, CAST(strftime('%s', committed_at) AS INTEGER) * 1000 + CAST(substr(strftime('%f', committed_at), 4, 3) AS INTEGER) FROM {NS}_commit WHERE thread_id = ?1 ORDER BY sequence"
    ))
    .bind(&thread_id.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(recovery_reject)?
    .into_iter()
    .map(|(sequence, committed_at)| {
        Ok((
            u64::try_from(sequence).map_err(recovery_reject)?,
            u64::try_from(committed_at).map_err(recovery_reject)?,
        ))
    })
    .collect::<Result<std::collections::BTreeMap<_, _>, RecoveryError>>()?;
    tx.commit().await.map_err(recovery_reject)?;
    Ok(RunRecoverySnapshot {
        commit_timestamps,
        thread_id,
        claimed_run_id,
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

async fn read_thread_json_rows_with_commit<T>(
    tx: &mut SqliteConnection,
    table: &str,
    thread_id: &ThreadId,
) -> Result<(Vec<u64>, Vec<T>), RecoveryError>
where
    T: serde::de::DeserializeOwned,
{
    let rows = sqlx::query_as::<_, (i64, String)>(&format!(
        "SELECT commit_sequence, data FROM {NS}_{table} WHERE thread_id = ?1 ORDER BY id"
    ))
    .bind(&thread_id.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(recovery_reject)?;
    let mut cursors = Vec::new();
    let mut values = Vec::new();
    for (cursor, value) in rows {
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
async fn write_commit(pool: &EmbeddedPool, commit: &ThreadCommit) -> Result<u64, Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await.map_err(reject)?;
    validate_transition_tx(&mut tx, commit).await?;
    ensure_thread_version(&mut tx, &commit.thread_id).await?;
    let current: i64 = sqlx::query_scalar(&format!(
        "SELECT COALESCE(MAX(sequence), 0) FROM {NS}_commit"
    ))
    .fetch_one(&mut *tx)
    .await
    .map_err(reject)?;
    let next = increment_authority(decode_authority(current)?)?;
    write_commit_rows(&mut tx, next, commit).await?;
    let current_version: i64 = sqlx::query_scalar(&format!(
        "SELECT version FROM {NS}_thread_version WHERE thread_id = ?1"
    ))
    .bind(&commit.thread_id.0)
    .fetch_one(&mut *tx)
    .await
    .map_err(reject)?;
    let next_version = increment_authority(decode_authority(current_version)?)?;
    sqlx::query(&format!(
        "UPDATE {NS}_thread_version SET version = ?2 WHERE thread_id = ?1"
    ))
    .bind(&commit.thread_id.0)
    .bind(encode_authority(next_version)?)
    .execute(&mut *tx)
    .await
    .map_err(reject)?;
    tx.commit().await.map_err(reject)?;
    Ok(next)
}

async fn write_operation(
    pool: &EmbeddedPool,
    operation: &CommitOperation,
) -> Result<OperationWrite, Error> {
    require_commit_operation_identity(operation)
        .map_err(|error| Error::Rejected(error.to_string()))?;
    let operation_ordinal = i64::try_from(operation.operation_id.ordinal).map_err(|_| {
        Error::Rejected("commit operation ordinal exceeds backend range".to_string())
    })?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await.map_err(reject)?;
    let existing = sqlx::query_as::<_, (String, i64, i64)>(&format!(
        "SELECT payload_hash, commit_sequence, thread_version \
         FROM {NS}_commit_receipt \
         WHERE operation_run_id = ?1 AND operation_ordinal = ?2"
    ))
    .bind(&operation.operation_id.run_id.0)
    .bind(operation_ordinal)
    .fetch_optional(&mut *tx)
    .await
    .map_err(reject)?;
    let existing = existing
        .map(|(payload_hash, commit_sequence, thread_version)| {
            Ok(CommitReceipt {
                operation_id: operation.operation_id.clone(),
                commit_sequence: decode_authority(commit_sequence)?,
                thread_version: decode_authority(thread_version)?,
                payload_hash: CommitPayloadHash(payload_hash),
                duplicate: false,
            })
        })
        .transpose()?;
    if let Some(receipt) = replay_commit_operation_receipt(operation, existing.as_ref())
        .map_err(|error| Error::Rejected(error.to_string()))?
    {
        return Ok(OperationWrite::Duplicate(receipt));
    }

    ensure_thread_version(&mut tx, &operation.commit.thread_id).await?;
    let current_version: i64 = sqlx::query_scalar(&format!(
        "SELECT version FROM {NS}_thread_version WHERE thread_id = ?1"
    ))
    .bind(&operation.commit.thread_id.0)
    .fetch_one(&mut *tx)
    .await
    .map_err(reject)?;
    let current_version = decode_authority(current_version)?;
    let current_ordinal: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {NS}_commit WHERE run_id = ?1"
    ))
    .bind(&operation.operation_id.run_id.0)
    .fetch_one(&mut *tx)
    .await
    .map_err(reject)?;
    let current_ordinal = decode_authority(current_ordinal)?;
    let thread_version = admit_commit_operation_append(operation, current_version, current_ordinal)
        .map_err(|error| Error::Rejected(error.to_string()))?;
    validate_transition_tx(&mut tx, &operation.commit).await?;
    let current_sequence: i64 = sqlx::query_scalar(&format!(
        "SELECT COALESCE(MAX(sequence), 0) FROM {NS}_commit"
    ))
    .fetch_one(&mut *tx)
    .await
    .map_err(reject)?;
    let next = increment_authority(decode_authority(current_sequence)?)?;
    write_commit_rows(&mut tx, next, &operation.commit).await?;
    sqlx::query(&format!(
        "UPDATE {NS}_thread_version SET version = ?2 WHERE thread_id = ?1"
    ))
    .bind(&operation.commit.thread_id.0)
    // SQLite INTEGER is signed: a version beyond i64 is refused, never wrapped.
    .bind(i64::try_from(thread_version).map_err(|error| Error::Rejected(error.to_string()))?)
    .execute(&mut *tx)
    .await
    .map_err(reject)?;
    sqlx::query(&format!(
        "INSERT INTO {NS}_commit_receipt \
         (operation_run_id, operation_ordinal, thread_id, payload_hash, \
          commit_sequence, thread_version) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
    ))
    .bind(&operation.operation_id.run_id.0)
    .bind(operation_ordinal)
    .bind(&operation.commit.thread_id.0)
    .bind(&operation.payload_hash.0)
    .bind(encode_authority(next)?)
    .bind(encode_authority(thread_version)?)
    .execute(&mut *tx)
    .await
    .map_err(reject)?;
    tx.commit().await.map_err(reject)?;
    Ok(OperationWrite::Applied(applied_commit_receipt(
        operation.operation_id.clone(),
        operation.payload_hash.clone(),
        next,
        thread_version,
    )))
}

async fn ensure_thread_version(
    tx: &mut SqliteConnection,
    thread_id: &ThreadId,
) -> Result<(), Error> {
    sqlx::query(&format!(
        "INSERT OR IGNORE INTO {NS}_thread_version (thread_id, version) \
         SELECT ?1, COUNT(*) FROM {NS}_commit WHERE thread_id = ?1"
    ))
    .bind(&thread_id.0)
    .execute(&mut *tx)
    .await
    .map_err(reject)?;
    Ok(())
}

async fn validate_transition_tx(
    tx: &mut SqliteConnection,
    commit: &ThreadCommit,
) -> Result<(), Error> {
    let phase: Option<String> = sqlx::query_scalar(&format!(
        "SELECT phase FROM {NS}_run_record WHERE run_id = ?1"
    ))
    .bind(&commit.run_id().0)
    .fetch_optional(&mut *tx)
    .await
    .map_err(reject)?;
    let existing = phase
        .as_deref()
        .map(serde_json::from_str::<RunState>)
        .transpose()
        .map_err(|error| {
            Error::Rejected(format!(
                "decode committed run {} state before transition: {error}",
                commit.run_id().0
            ))
        })?;
    if existing.is_some_and(|state| !state.permits(&commit.run_state())) {
        return Err(Error::Rejected(format!(
            "run {} is already terminal; refusing post-terminal commit",
            commit.run_id().0
        )));
    }
    Ok(())
}

/// JSON columns are stored as serialized text — the schema renders `{json}` to
/// TEXT on SQLite.
async fn write_commit_rows(
    tx: &mut SqliteConnection,
    next: u64,
    commit: &ThreadCommit,
) -> Result<(), Error> {
    let p = NS;
    let run_id = &commit.run_id().0;
    let thread_id = &commit.thread_id.0;
    let run_state = commit.run_state();
    let state_json = json(&run_state)?;

    sqlx::query(&format!(
        "INSERT INTO {p}_commit (sequence, thread_id, run_id, phase) VALUES (?1,?2,?3,?4)"
    ))
    .bind(encode_authority(next)?)
    .bind(thread_id)
    .bind(run_id)
    .bind(&state_json)
    .execute(&mut *tx)
    .await
    .map_err(reject)?;

    for message in &commit.messages {
        sqlx::query(&format!(
            "INSERT INTO {p}_message (commit_sequence, thread_id, data) VALUES (?1,?2,?3)"
        ))
        .bind(encode_authority(next)?)
        .bind(thread_id)
        .bind(json(message)?)
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
    }

    for command in &commit.state {
        sqlx::query(&format!(
            "INSERT INTO {p}_state_command (commit_sequence, thread_id, data) VALUES (?1,?2,?3)"
        ))
        .bind(encode_authority(next)?)
        .bind(thread_id)
        .bind(json(command)?)
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
    }

    for (offset, draft) in commit.events.iter().enumerate() {
        let sequence = encode_run_lifecycle_cursor(next, offset)
            .map_err(|error| Error::Rejected(error.to_string()))?;
        sqlx::query(&format!(
            "INSERT INTO {p}_event (sequence, run_id, kind, payload) VALUES (?1,?2,?3,?4)"
        ))
        .bind(encode_authority(sequence.0)?)
        .bind(run_id)
        .bind(json(&draft.kind)?)
        .bind(json(&draft.payload)?)
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
    }

    sqlx::query(&format!(
        "INSERT INTO {p}_run_record (run_id, thread_id, phase) VALUES (?1,?2,?3) \
         ON CONFLICT(run_id) DO UPDATE SET thread_id = excluded.thread_id, \
         phase = excluded.phase, updated_at = CURRENT_TIMESTAMP"
    ))
    .bind(run_id)
    .bind(thread_id)
    .bind(&state_json)
    .execute(&mut *tx)
    .await
    .map_err(reject)?;

    // Await or clear the awaiting ticket atomically with the checkpoint (G5).
    let awaiting = matches!(run_state, RunState::Awaiting);
    if awaiting {
        let ticket = commit
            .resume_ticket()
            .expect("awaiting checkpoint has a ticket");
        sqlx::query(&format!(
            "INSERT INTO {p}_waiting (run_id, ticket) VALUES (?1,?2) \
             ON CONFLICT(run_id) DO UPDATE SET ticket = excluded.ticket"
        ))
        .bind(run_id)
        .bind(json(ticket)?)
        .execute(&mut *tx)
        .await
        .map_err(reject)?;
    } else {
        sqlx::query(&format!("DELETE FROM {p}_waiting WHERE run_id = ?1"))
            .bind(run_id)
            .execute(&mut *tx)
            .await
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
async fn hydrate(pool: &EmbeddedPool) -> Result<Projection, String> {
    let mut conn = pool.acquire().await.map_err(|error| error.to_string())?;
    let sequence: i64 = sqlx::query_scalar(&format!(
        "SELECT COALESCE(MAX(sequence), 0) FROM {NS}_commit"
    ))
    .fetch_one(&mut *conn)
    .await
    .map_err(|error| error.to_string())?;
    let sequence = sqlite_authority(sequence)?;
    let mut projection = Projection {
        sequence,
        ..Default::default()
    };

    let rows = sqlx::query_as::<_, (String, String)>(&format!(
        "SELECT thread_id, data FROM {NS}_message ORDER BY id"
    ))
    .fetch_all(&mut *conn)
    .await
    .map_err(|error| error.to_string())?;
    for (thread_id, data) in rows {
        if let Ok(message) = serde_json::from_str::<Message>(&data) {
            projection.messages.push((ThreadId(thread_id), message));
        }
    }

    // Rebuild the committed state-command log per thread, in commit order, so a
    // resumed run replays its accumulated state from durable truth (G1/G13).
    let rows = sqlx::query_as::<_, (String, String)>(&format!(
        "SELECT thread_id, data FROM {NS}_state_command ORDER BY id"
    ))
    .fetch_all(&mut *conn)
    .await
    .map_err(|error| error.to_string())?;
    for (thread_id, data) in rows {
        if let Ok(command) = serde_json::from_str::<StateCommand>(&data) {
            projection.state.push((ThreadId(thread_id), command));
        }
    }

    // Fold the commit log in order so the latest fact per run wins (G32).
    let rows = sqlx::query_as::<_, (String, String, String)>(&format!(
        "SELECT run_id, thread_id, phase FROM {NS}_commit ORDER BY sequence"
    ))
    .fetch_all(&mut *conn)
    .await
    .map_err(|error| error.to_string())?;
    for (run_id, thread_id, state_json) in rows {
        if let Ok(state) = serde_json::from_str::<RunState>(&state_json) {
            let thread_version = projection
                .thread_versions
                .entry(ThreadId(thread_id.clone()))
                .or_default();
            *thread_version = StoredU64::try_from(*thread_version)
                .and_then(|value| value.checked_add(1))
                .map(StoredU64::domain_value)
                .map_err(|error| error.to_string())?;
            let run_commit_count = projection
                .run_commit_counts
                .entry(RunId(run_id.clone()))
                .or_default();
            *run_commit_count = StoredU64::try_from(*run_commit_count)
                .and_then(|value| value.checked_add(1))
                .map(StoredU64::domain_value)
                .map_err(|error| error.to_string())?;
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
    let rows = sqlx::query_as::<_, (i64, String, String, String)>(&format!(
        "SELECT sequence, run_id, kind, payload FROM {NS}_event ORDER BY sequence"
    ))
    .fetch_all(&mut *conn)
    .await
    .map_err(|error| error.to_string())?;
    for (sequence, run_id, kind, payload) in rows {
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

    let rows =
        sqlx::query_as::<_, (String, String)>(&format!("SELECT run_id, ticket FROM {NS}_waiting"))
            .fetch_all(&mut *conn)
            .await
            .map_err(|error| error.to_string())?;
    for (run_id, ticket) in rows {
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

fn reject(err: sqlx::Error) -> Error {
    Error::Rejected(err.to_string())
}

#[cfg(test)]
mod scheduler_isolation_tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn corrupt_committed_run_state_fails_closed_before_append() {
        // Cause/effect graph: C1 a committed Running row has a decodable state;
        // C2 durable storage is corrupted to an unknown state; C3 another
        // commit targets that Run. Effects: E1 C1 allows the initial append; E2
        // C2+C3 rejects before sequence allocation or fact writes; E3 the
        // original one-record history remains unchanged. Rules S1=C1=>E1 and
        // S2=C2+C3=>E2+E3. A decode failure must never be normalized to an
        // absent/nonterminal state that widens the transition relation.
        let store = SqliteCommitCoordinator::open_in_memory()
            .await
            .expect("store");
        let thread_id = ThreadId("corrupt-state-thread".into());
        let run_id = RunId("corrupt-state-run".into());
        store
            .commit(ThreadCommit {
                thread_id: thread_id.clone(),
                run: awaken_agent_contract::thread::commit::staged::RunDisposition::running(
                    run_id.clone(),
                ),
                messages: Vec::new(),
                state: Vec::new(),
                events: Vec::new(),
            })
            .await
            .expect("S1/E1");
        sqlx::query(&format!(
            "UPDATE {NS}_run_record SET phase = ?1 WHERE run_id = ?2"
        ))
        .bind("not-json")
        .bind(&run_id.0)
        .execute(&store.pool)
        .await
        .expect("inject corrupt durable state");

        let error = store
            .commit(ThreadCommit {
                thread_id,
                run: awaken_agent_contract::thread::commit::staged::RunDisposition::running(run_id),
                messages: Vec::new(),
                state: Vec::new(),
                events: Vec::new(),
            })
            .await
            .expect_err("S2/E2 corrupt authority must fail closed");
        assert!(error.to_string().contains("decode committed run"), "S2/E2");
        assert_eq!(store.commit_count(), 1, "S2/E3");
    }

    /// Commit-read scheduler-isolation cause/effect graph: C1 an owner holds
    /// the SQLite commit connection; C2 lifecycle delivery and recovery
    /// concurrently queue on a two-worker Tokio runtime. Effects: E1 neither
    /// read blocks a runtime worker; E2 an authority timer fires while the owner
    /// still holds the connection; E3 both authoritative reads finish after
    /// release without inventing durable facts. Rule C1=C1+C2=>E1+E2+E3.
    /// This adapter rule prevents either recovery read port from bypassing the
    /// workspace's single canonical SQLite scheduling boundary.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recovery_reads_cannot_starve_authority_timers() {
        let store = Arc::new(
            SqliteCommitCoordinator::open_in_memory()
                .await
                .expect("store"),
        );
        let held = store.pool.acquire().await.expect("C1 connection held");

        let started = Instant::now();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            started.elapsed()
        });
        let lifecycle = tokio::spawn({
            let store = Arc::clone(&store);
            async move { store.events_after(RunLifecycleCursor(0), 16).await }
        });
        let recovery = tokio::spawn({
            let store = Arc::clone(&store);
            async move {
                store
                    .recovery_snapshot(&ThreadId("thread".into()), &RunId("run".into()))
                    .await
            }
        });
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let elapsed = tokio::time::timeout(Duration::from_millis(100), timer)
            .await
            .expect("C1/E1-E2 authority timer remains schedulable")
            .expect("C1 timer task");
        assert!(
            elapsed < Duration::from_millis(100),
            "C1/E2 timer fired after {elapsed:?}; recovery reads blocked runtime workers"
        );

        drop(held);
        let lifecycle = lifecycle.await.expect("lifecycle task").expect("C1/E3");
        assert!(lifecycle.events.is_empty(), "C1/E3 no invented events");
        let recovery = recovery.await.expect("recovery task").expect("C1/E3");
        assert!(recovery.runs.is_empty(), "C1/E3 no invented Runs");
    }
}
