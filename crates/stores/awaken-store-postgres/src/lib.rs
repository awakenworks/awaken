// @formal entrypoint background_task.terminal_observer_reads_authoritative_committed_state_not_process_projection
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
//! Postgres durable implementation of the runtime commit boundary.
//!
//! [`PostgresCommitCoordinator`] implements the neutral [`Coordinator`] write
//! boundary (G1/G13) against Postgres: each `commit` writes the staged
//! `ThreadCommit` — messages, state commands, events, the run fact, and the
//! awaiting-ticket transition — in one SQL transaction (ADR-0006). The committed
//! fact log is the authority; the `run_record` table is a derived cache equal to
//! the latest fact (G32).
//!
//! Startup reads only the global commit fence. Coordinator opens a Thread's
//! process-local execution view on demand from one consistent recovery snapshot;
//! durable event, lifecycle and policy reads query committed SQL directly.

use std::collections::HashMap;

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
    require_commit_operation_payload_hash,
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
    decode_resume_ticket_value_for_owner, project_committed_run_records,
};
use awaken_store_schema::StoredU64;
use awaken_sync::InvariantLock;
use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;
use sqlx::{Postgres, Transaction};

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

fn dense_commit_sequence_successor(last: i64) -> Result<u64, Error> {
    let last = StoredU64::try_from(last).map_err(|error| Error::Rejected(error.to_string()))?;
    last.checked_add(1)
        .map(StoredU64::domain_value)
        .map_err(|error| Error::Rejected(error.to_string()))
}

#[cfg(kani)]
/// @obligation store_history.commits_form_one_dense_stable_global_order
#[kani::proof]
fn dense_commit_sequence_is_exact_nonnegative_and_nonwrapping() {
    // Cause/effect table over every stored i64 tail: C1 negative -> reject;
    // C2 MAX -> reject; C3 0..MAX-1 -> exact positive successor. The separate
    // i128 equation is the oracle. PostgreSQL table-lock/transaction behavior
    // remains database TCB; this proves the production allocation decision.
    let last: i64 = kani::any();
    match dense_commit_sequence_successor(last) {
        Ok(next) => {
            assert!(last >= 0 && last < i64::MAX);
            assert_eq!(i128::from(next), i128::from(last) + 1);
        }
        Err(_) => assert!(last < 0 || last == i64::MAX),
    }
}

/// Bundle id for Postgres-only commit-order compatibility. Scoped so it never
/// collides with the portable commit schema (`awaken.runtime_commit`) or the
/// dispatch schema in a shared database.
pub const COMMIT_PG_BUNDLE_ID: &str = "awaken.runtime_commit_pg";

/// Postgres-only commit-order migrations, embedded from their SQL files so the
/// compatibility history remains explicit and checksum guarded.
const COMMIT_PG_FILES: &[(&str, &str)] = &[
    (
        "V0001__commit_sequence.sql",
        include_str!("migrations/V0001__commit_sequence.sql"),
    ),
    (
        "V0002__transactional_dense_commit_order.sql",
        include_str!("migrations/V0002__transactional_dense_commit_order.sql"),
    ),
];

/// Build the Postgres-only commit-order compatibility bundle. V1 records the
/// historical sequence allocator; V2 compacts its possible gaps and retires it.
/// This remains separate from the portable [`commit_bundle`] so existing scoped
/// receipts keep their checksums and upgrade order. It runs after the portable
/// schema, where the commit table is the current transactional order authority.
pub fn commit_pg_bundle()
-> Result<awaken_scoped_migration::MigrationBundle, awaken_scoped_migration::MigrationError> {
    let migrations = COMMIT_PG_FILES
        .iter()
        .map(|(name, contents)| {
            let version = name
                .trim_start_matches('V')
                .split("__")
                .next()
                .and_then(|d| d.parse::<i64>().ok())
                .unwrap_or(0);
            let description = contents
                .lines()
                .map(str::trim)
                .find_map(|line| line.strip_prefix("--").map(|rest| rest.trim().to_string()))
                .filter(|desc| !desc.is_empty())
                .unwrap_or_else(|| (*name).to_string());
            awaken_scoped_migration::Migration::new(version, description, contents.trim())
        })
        .collect::<Result<Vec<_>, _>>()?;
    awaken_scoped_migration::MigrationBundle::new(COMMIT_PG_BUNDLE_ID, migrations)
}

/// Errors from constructing or migrating the store. Commit-time failures use the
/// neutral [`Coordinator`] error.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("migrate: {0}")]
    Migrate(String),
    #[error("read: {0}")]
    Read(String),
    #[error("hydrate: {0}")]
    Hydrate(String),
}

/// Process-local execution view. A Coordinator creates one for an active Thread
/// from a committed recovery snapshot and advances it after its own commits.
#[derive(Debug, Default)]
struct Projection {
    sequence: u64,
    messages: Vec<(ThreadId, Message)>,
    /// Committed state commands per thread, in commit order (for `committed_state`).
    /// A resumed run rebuilds its materialized state from these durable rows.
    state: Vec<(ThreadId, StateCommand)>,
    run_records: HashMap<RunId, RunRecord>,
    /// The latest committed run per thread, in commit order (for `latest_run`).
    latest_by_thread: HashMap<ThreadId, RunRecord>,
    /// Committed events in commit order (for `list_events`), a fact-derived cache
    /// rebuilt from the durable event table on construction.
    events: Vec<EventRecord>,
    resume_tickets: HashMap<RunId, ResumeTicket>,
}

/// The component namespace for this runtime's tables. One runtime is one
/// component, so its commit and dispatch tables share this prefix; the scoped
/// migration ledger isolates it from any other component in the same database.
/// Built in, not configured.
const NS: &str = "runtime";

/// A Postgres-backed [`Coordinator`] plus asynchronous durable read ports.
pub struct PostgresCommitCoordinator {
    pool: PgPool,
    projection: InvariantLock<Projection>,
    projection_thread: Option<ThreadId>,
}

impl PostgresCommitCoordinator {
    async fn pool(url: &str, max_connections: u32) -> Result<PgPool, StoreError> {
        awaken_sql_storage::postgres_pool_options()
            .max_connections(max_connections)
            .connect(url)
            .await
            .map_err(|err| StoreError::Connect(err.to_string()))
    }

    async fn migrate_pool(pool: &PgPool) -> Result<(), StoreError> {
        let bundle = commit_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        let runner = awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
            pool.clone(),
            NS,
        )
        .map_err(|err| StoreError::Migrate(err.to_string()))?;
        runner
            .run_bundle(&bundle)
            .await
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
        // Apply the Postgres-only sequence after the portable tables it reads.
        // Its independent bundle id keeps this object off the SQLite path.
        let pg_bundle = commit_pg_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        runner
            .run_bundle(&pg_bundle)
            .await
            .map(|_| ())
            .map_err(|err| StoreError::Migrate(err.to_string()))
    }

    async fn verify_pool(pool: &PgPool) -> Result<(), StoreError> {
        let bundle = commit_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        let runner = awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
            pool.clone(),
            NS,
        )
        .map_err(|err| StoreError::Migrate(err.to_string()))?;
        runner
            .verify_bundle(&bundle)
            .await
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
        let pg_bundle = commit_pg_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        runner
            .verify_bundle(&pg_bundle)
            .await
            .map_err(|err| StoreError::Migrate(err.to_string()))
    }

    /// Apply the portable and Postgres-only commit bundles without hydrating a
    /// runtime projection. Operational migration commands use this schema-only
    /// path.
    pub async fn migrate(url: &str, max_connections: u32) -> Result<(), StoreError> {
        let pool = Self::pool(url, max_connections).await?;
        Self::migrate_pool(&pool).await
    }

    /// Connect and apply the commit-schema migrations without loading history.
    ///
    /// Pool sizing is an explicit composition input, not store-owned process
    /// configuration.
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = Self::pool(url, max_connections).await?;
        Self::with_pool(pool).await
    }

    /// Connect to commit schemas applied by the deployment migration phase.
    /// The ledger is verified before the bounded commit fence is read; no DDL
    /// is executed on this path.
    pub async fn connect_existing(url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = Self::pool(url, max_connections).await?;
        Self::with_existing_pool(pool).await
    }

    /// Build from an existing pool: apply migrations and read the global fence.
    pub async fn with_pool(pool: PgPool) -> Result<Self, StoreError> {
        Self::migrate_pool(&pool).await?;
        Self::open_pool(pool).await
    }

    /// Build from an existing pool after verifying both the portable commit
    /// bundle and the Postgres-only sequence bundle.
    pub async fn with_existing_pool(pool: PgPool) -> Result<Self, StoreError> {
        Self::verify_pool(&pool).await?;
        Self::open_pool(pool).await
    }

    async fn open_pool(pool: PgPool) -> Result<Self, StoreError> {
        let sequence: i64 = sqlx::query_scalar(&format!(
            "SELECT COALESCE(MAX(sequence), 0) FROM {NS}_commit"
        ))
        .fetch_one(&pool)
        .await
        .map_err(|error| StoreError::Read(error.to_string()))?;
        let sequence =
            decode_authority(sequence).map_err(|error| StoreError::Read(error.to_string()))?;
        Ok(Self {
            pool,
            projection: InvariantLock::new(Projection {
                sequence,
                ..Projection::default()
            }),
            projection_thread: None,
        })
    }

    /// Open one active Thread's synchronous execution view from a single
    /// repeatable-read recovery prefix. The returned handle shares the database
    /// pool but owns only this Thread's disposable process-local projection.
    pub async fn open_thread_projection(&self, thread_id: &ThreadId) -> Result<Self, StoreError> {
        let snapshot = self
            .recovery_snapshot(thread_id, &RunId("__projection_read__".to_owned()))
            .await
            .map_err(|error| StoreError::Hydrate(error.to_string()))?;
        Ok(Self {
            pool: self.pool.clone(),
            projection: InvariantLock::new(projection_from_snapshot(snapshot)),
            projection_thread: Some(thread_id.clone()),
        })
    }

    fn require_projection_thread(&self, thread_id: &ThreadId) -> Result<(), Error> {
        if self
            .projection_thread
            .as_ref()
            .is_some_and(|opened| opened != thread_id)
        {
            return Err(Error::Rejected(format!(
                "Thread {} does not match this execution view",
                thread_id.0
            )));
        }
        Ok(())
    }

    /// Last global commit fence observed by this process or execution view.
    /// Cross-replica decisions must query SQL, not use this diagnostic value.
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

    /// The active awaiting ticket for a run, if it is currently awaiting.
    pub fn resume_ticket_for(&self, run_id: &RunId) -> Option<ResumeTicket> {
        self.projection
            .lock()
            .ok()
            .and_then(|p| p.resume_tickets.get(run_id).cloned())
    }

    /// Committed state commands in this process-local execution view. A scoped
    /// view is opened from one durable recovery prefix; cross-replica decisions
    /// use [`Self::authoritative_committed_state`] instead.
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

    /// Read one Run directly from committed PostgreSQL truth. Active-active
    /// peers use this narrow reconciliation read when another process performed
    /// the commit and therefore only that process advanced its local projection.
    pub async fn authoritative_run_record(
        &self,
        run_id: &RunId,
    ) -> Result<Option<RunRecord>, StoreError> {
        let row = sqlx::query(&format!(
            "SELECT thread_id, phase FROM {NS}_run_record WHERE run_id = $1"
        ))
        .bind(&run_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| StoreError::Read(error.to_string()))?;
        row.map(|row| {
            let thread_id = ThreadId(
                row.try_get("thread_id")
                    .map_err(|error| StoreError::Read(error.to_string()))?,
            );
            let Json(state): Json<RunState> = row
                .try_get("phase")
                .map_err(|error| StoreError::Read(error.to_string()))?;
            Ok(RunRecord {
                id: run_id.clone(),
                thread_id,
                state,
            })
        })
        .transpose()
    }

    /// Read the latest Run for a Thread directly from committed PostgreSQL
    /// truth. The commit sequence selects the Run while `run_record` supplies
    /// its current state, so an active-active peer never relies on its local
    /// compatibility projection.
    pub async fn authoritative_latest_run_record(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Option<RunRecord>, StoreError> {
        let row = sqlx::query(&format!(
            "SELECT latest.run_id, run.phase \
             FROM (\
                 SELECT run_id FROM {NS}_commit \
                 WHERE thread_id = $1 ORDER BY sequence DESC LIMIT 1\
             ) AS latest \
             JOIN {NS}_run_record AS run ON run.run_id = latest.run_id"
        ))
        .bind(&thread_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| StoreError::Read(error.to_string()))?;
        row.map(|row| {
            let run_id = RunId(
                row.try_get("run_id")
                    .map_err(|error| StoreError::Read(error.to_string()))?,
            );
            let Json(state): Json<RunState> = row
                .try_get("phase")
                .map_err(|error| StoreError::Read(error.to_string()))?;
            Ok(RunRecord {
                id: run_id,
                thread_id: thread_id.clone(),
                state,
            })
        })
        .transpose()
    }

    /// Read the latest Run's awaiting ticket directly from committed PostgreSQL
    /// truth. This is deliberately one query over the latest commit and its
    /// optional waiting row: a peer may have completed a newer Run without
    /// advancing this process's compatibility projection.
    pub async fn authoritative_open_wait_for_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Option<(RunId, ResumeTicket)>, StoreError> {
        let row = sqlx::query(&format!(
            "SELECT latest.run_id, run.phase, waiting.ticket \
             FROM (\
                 SELECT run_id FROM {NS}_commit \
                 WHERE thread_id = $1 ORDER BY sequence DESC LIMIT 1\
             ) AS latest \
             JOIN {NS}_run_record AS run ON run.run_id = latest.run_id \
             JOIN {NS}_waiting AS waiting ON waiting.run_id = latest.run_id"
        ))
        .bind(&thread_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| StoreError::Read(error.to_string()))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let run_id = RunId(
            row.try_get("run_id")
                .map_err(|error| StoreError::Read(error.to_string()))?,
        );
        let Json(state): Json<RunState> = row
            .try_get("phase")
            .map_err(|error| StoreError::Read(error.to_string()))?;
        if !matches!(state, RunState::Awaiting) {
            tracing::error!(
                awaken.run.id = %run_id.0,
                awaken.thread.id = %thread_id.0,
                "quarantining a stale waiting row whose Run is not Awaiting"
            );
            return Ok(None);
        }
        let Json(ticket): Json<serde_json::Value> = row
            .try_get("ticket")
            .map_err(|error| StoreError::Read(error.to_string()))?;
        Ok(decode_resume_ticket(&run_id, thread_id, ticket).map(|ticket| (run_id, ticket)))
    }

    /// Read a thread's complete message history directly from committed
    /// PostgreSQL truth. Active-active peers use this instead of their
    /// process-local compatibility projection when projecting public history.
    pub async fn authoritative_committed_messages(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<Message>, StoreError> {
        let rows = sqlx::query(&format!(
            "SELECT data FROM {NS}_message WHERE thread_id = $1 ORDER BY id"
        ))
        .bind(&thread_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| StoreError::Read(error.to_string()))?;
        rows.into_iter()
            .map(|row| {
                let Json(message): Json<Message> = row
                    .try_get("data")
                    .map_err(|error| StoreError::Read(error.to_string()))?;
                Ok(message)
            })
            .collect()
    }

    /// Read a thread's complete state-command log from committed PostgreSQL
    /// truth. Active-active peers use this for usage and state-derived policy;
    /// their process-local projections advance only for commits they applied.
    pub async fn authoritative_committed_state(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<StateCommand>, StoreError> {
        let rows = sqlx::query(&format!(
            "SELECT data FROM {NS}_state_command WHERE thread_id = $1 ORDER BY id"
        ))
        .bind(&thread_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| StoreError::Read(error.to_string()))?;
        rows.into_iter()
            .map(|row| {
                let Json(command): Json<StateCommand> = row
                    .try_get("data")
                    .map_err(|error| StoreError::Read(error.to_string()))?;
                Ok(command)
            })
            .collect()
    }

    /// Query durable PostgreSQL truth directly instead of consulting the
    /// process-local projection. Collision checks must see commits from peers.
    pub async fn authoritative_thread_exists(
        &self,
        thread_id: &ThreadId,
    ) -> Result<bool, StoreError> {
        sqlx::query_scalar::<_, bool>(&format!(
            "SELECT EXISTS(SELECT 1 FROM {NS}_commit WHERE thread_id = $1)"
        ))
        .bind(&thread_id.0)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| StoreError::Read(error.to_string()))
    }
}

#[async_trait]
impl CommitCoordinator for PostgresCommitCoordinator {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, Error> {
        self.require_projection_thread(&commit.thread_id)?;
        commit
            .validate()
            .map_err(|e| Error::Rejected(e.to_string()))?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|err| Error::Rejected(err.to_string()))?;
        let current_version = lock_thread_version(&mut tx, &commit.thread_id).await?;
        let (next, committed_events) = append_commit(&mut tx, &commit).await?;
        set_thread_version(
            &mut tx,
            &commit.thread_id,
            increment_authority(current_version)?,
        )
        .await?;
        tx.commit().await.map_err(reject)?;
        let mut projection = lock(&self.projection)?;
        advance_projection(&mut projection, commit, next, committed_events);
        Ok(CommitRecord { sequence: next })
    }
}

#[async_trait]
impl OperationCoordinator for PostgresCommitCoordinator {
    async fn commit_operation(&self, operation: CommitOperation) -> Result<CommitReceipt, Error> {
        self.require_projection_thread(&operation.commit.thread_id)?;
        operation
            .commit
            .validate()
            .map_err(|error| Error::Rejected(error.to_string()))?;
        require_commit_operation_identity(&operation)
            .map_err(|error| Error::Rejected(error.to_string()))?;
        let operation_ordinal = i64::try_from(operation.operation_id.ordinal).map_err(|_| {
            Error::Rejected("commit operation ordinal exceeds backend range".to_string())
        })?;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let current_version = lock_thread_version(&mut tx, &operation.commit.thread_id).await?;
        let existing = load_receipt(&mut tx, &operation).await?;
        if let Some(receipt) = replay_commit_operation_receipt(
            &operation,
            existing
                .as_ref()
                .map(|(thread_id, receipt)| (thread_id, receipt)),
        )
        .map_err(|error| Error::Rejected(error.to_string()))?
        {
            return Ok(receipt);
        }
        require_commit_operation_payload_hash(&operation)
            .map_err(|error| Error::Rejected(error.to_string()))?;
        let current_ordinal: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {NS}_commit WHERE run_id = $1"
        ))
        .bind(&operation.operation_id.run_id.0)
        .fetch_one(&mut *tx)
        .await
        .map_err(reject)?;
        let current_ordinal = decode_authority(current_ordinal)?;
        let thread_version =
            admit_commit_operation_append(&operation, current_version, current_ordinal)
                .map_err(|error| Error::Rejected(error.to_string()))?;
        let (next, committed_events) = append_commit(&mut tx, &operation.commit).await?;
        set_thread_version(&mut tx, &operation.commit.thread_id, thread_version).await?;
        sqlx::query(&format!(
            "INSERT INTO {NS}_commit_receipt \
             (operation_run_id, operation_ordinal, thread_id, payload_hash, \
              commit_sequence, thread_version) VALUES ($1, $2, $3, $4, $5, $6)"
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
        let receipt = applied_commit_receipt(
            operation.operation_id,
            operation.payload_hash,
            next,
            thread_version,
        );
        let mut projection = lock(&self.projection)?;
        advance_projection(&mut projection, operation.commit, next, committed_events);
        Ok(receipt)
    }
}

async fn lock_thread_version(
    tx: &mut Transaction<'_, Postgres>,
    thread_id: &ThreadId,
) -> Result<u64, Error> {
    // One transaction-scoped lock per Thread serializes its CAS without
    // serializing unrelated Threads. The hash may create a harmless false
    // serialization collision, but never a false version conflict.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(&thread_id.0)
        .execute(&mut **tx)
        .await
        .map_err(reject)?;
    let committed: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {NS}_commit WHERE thread_id = $1"
    ))
    .bind(&thread_id.0)
    .fetch_one(&mut **tx)
    .await
    .map_err(reject)?;
    sqlx::query(&format!(
        "INSERT INTO {NS}_thread_version (thread_id, version) VALUES ($1, $2) \
         ON CONFLICT (thread_id) DO UPDATE SET version = EXCLUDED.version"
    ))
    .bind(&thread_id.0)
    .bind(committed)
    .execute(&mut **tx)
    .await
    .map_err(reject)?;
    decode_authority(committed)
}

async fn set_thread_version(
    tx: &mut Transaction<'_, Postgres>,
    thread_id: &ThreadId,
    version: u64,
) -> Result<(), Error> {
    sqlx::query(&format!(
        "UPDATE {NS}_thread_version SET version = $2 WHERE thread_id = $1"
    ))
    .bind(&thread_id.0)
    .bind(encode_authority(version)?)
    .execute(&mut **tx)
    .await
    .map_err(reject)?;
    Ok(())
}

async fn load_receipt(
    tx: &mut Transaction<'_, Postgres>,
    operation: &CommitOperation,
) -> Result<Option<(ThreadId, CommitReceipt)>, Error> {
    let row = sqlx::query(&format!(
        "SELECT thread_id, payload_hash, commit_sequence, thread_version \
         FROM {NS}_commit_receipt \
         WHERE operation_run_id = $1 AND operation_ordinal = $2"
    ))
    .bind(&operation.operation_id.run_id.0)
    .bind(encode_authority(operation.operation_id.ordinal)?)
    .fetch_optional(&mut **tx)
    .await
    .map_err(reject)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let thread_id: String = row.try_get("thread_id").map_err(reject)?;
    let payload_hash: String = row.try_get("payload_hash").map_err(reject)?;
    Ok(Some((
        ThreadId(thread_id),
        CommitReceipt {
            operation_id: operation.operation_id.clone(),
            commit_sequence: decode_authority(
                row.try_get::<i64, _>("commit_sequence").map_err(reject)?,
            )?,
            thread_version: decode_authority(
                row.try_get::<i64, _>("thread_version").map_err(reject)?,
            )?,
            payload_hash: CommitPayloadHash(payload_hash),
            duplicate: true,
        },
    )))
}

async fn append_commit(
    tx: &mut Transaction<'_, Postgres>,
    commit: &ThreadCommit,
) -> Result<(u64, Vec<EventRecord>), Error> {
    let run_id = commit.run_id();
    let thread_id = &commit.thread_id;
    let run_state = commit.run_state();
    let p = NS;
    let existing: Option<Json<RunState>> = sqlx::query_scalar(&format!(
        "SELECT phase FROM {p}_run_record WHERE run_id = $1 FOR UPDATE"
    ))
    .bind(&run_id.0)
    .fetch_optional(&mut **tx)
    .await
    .map_err(reject)?;
    if existing
        .as_ref()
        .is_some_and(|Json(state)| !state.permits(&run_state))
    {
        return Err(Error::Rejected(format!(
            "run {} is already terminal; refusing post-terminal commit",
            run_id.0
        )));
    }

    // One existing authority owns allocation and append: the transaction locks
    // the commit table, observes its exact tail, then inserts the successor.
    // A rollback releases both the lock and candidate, so no durable gap can be
    // created by a failed later write.
    sqlx::query(&format!(
        "LOCK TABLE {p}_commit IN SHARE ROW EXCLUSIVE MODE"
    ))
    .execute(&mut **tx)
    .await
    .map_err(reject)?;
    let last: i64 = sqlx::query_scalar(&format!(
        "SELECT COALESCE(MAX(sequence), 0) FROM {p}_commit"
    ))
    .fetch_one(&mut **tx)
    .await
    .map_err(reject)?;
    let next = dense_commit_sequence_successor(last)?;
    sqlx::query(&format!(
        "INSERT INTO {p}_commit (sequence, thread_id, run_id, phase) VALUES ($1, $2, $3, $4)"
    ))
    .bind(encode_authority(next)?)
    .bind(&thread_id.0)
    .bind(&run_id.0)
    .bind(Json(&run_state))
    .execute(&mut **tx)
    .await
    .map_err(reject)?;

    append_ordered_thread_facts(tx, "message", next, thread_id, &commit.messages).await?;
    append_ordered_thread_facts(tx, "state_command", next, thread_id, &commit.state).await?;
    let mut committed_events = Vec::with_capacity(commit.events.len());
    for (offset, draft) in commit.events.iter().enumerate() {
        let sequence = encode_run_lifecycle_cursor(next, offset)
            .map_err(|error| Error::Rejected(error.to_string()))?;
        let _ = encode_authority(sequence.0)?;
        committed_events.push(EventRecord {
            sequence: sequence.0,
            run_id: run_id.clone(),
            kind: draft.kind.clone(),
            payload: draft.payload.clone(),
        });
    }
    append_ordered_events(tx, run_id, &committed_events).await?;
    sqlx::query(&format!(
        "INSERT INTO {p}_run_record (run_id, thread_id, phase) VALUES ($1, $2, $3) \
         ON CONFLICT (run_id) DO UPDATE \
         SET thread_id = EXCLUDED.thread_id, phase = EXCLUDED.phase, updated_at = now()"
    ))
    .bind(&run_id.0)
    .bind(&thread_id.0)
    .bind(Json(&run_state))
    .execute(&mut **tx)
    .await
    .map_err(reject)?;

    if matches!(run_state, RunState::Awaiting) {
        let ticket = commit
            .resume_ticket()
            .expect("awaiting disposition has a ticket");
        sqlx::query(&format!(
            "INSERT INTO {p}_waiting (run_id, ticket) VALUES ($1, $2) \
             ON CONFLICT (run_id) DO UPDATE SET ticket = EXCLUDED.ticket"
        ))
        .bind(&run_id.0)
        .bind(Json(ticket))
        .execute(&mut **tx)
        .await
        .map_err(reject)?;
    } else {
        sqlx::query(&format!("DELETE FROM {p}_waiting WHERE run_id = $1"))
            .bind(&run_id.0)
            .execute(&mut **tx)
            .await
            .map_err(reject)?;
    }
    Ok((next, committed_events))
}

/// Keep one bounded SQL statement per fact kind instead of one round trip per
/// row while the dense global commit lock is held. `WITH ORDINALITY` makes the
/// source order explicit before PostgreSQL assigns the portable row ids used
/// by transcript/state readers. The table name is a private static call-site
/// choice, never input from a request or configuration.
async fn append_ordered_thread_facts<T: serde::Serialize>(
    tx: &mut Transaction<'_, Postgres>,
    table: &'static str,
    sequence: u64,
    thread_id: &ThreadId,
    facts: &[T],
) -> Result<(), Error> {
    const FACTS_PER_STATEMENT: usize = 256;
    let sequence = encode_authority(sequence)?;
    for chunk in facts.chunks(FACTS_PER_STATEMENT) {
        sqlx::query(&format!(
            "INSERT INTO {NS}_{table} (commit_sequence, thread_id, data) \
             SELECT $1, $2, entry.value \
             FROM jsonb_array_elements($3::jsonb) WITH ORDINALITY AS entry(value, ordinal) \
             ORDER BY entry.ordinal"
        ))
        .bind(sequence)
        .bind(&thread_id.0)
        .bind(Json(chunk))
        .execute(&mut **tx)
        .await
        .map_err(reject)?;
    }
    Ok(())
}

async fn append_ordered_events(
    tx: &mut Transaction<'_, Postgres>,
    run_id: &RunId,
    events: &[EventRecord],
) -> Result<(), Error> {
    const EVENTS_PER_STATEMENT: usize = 256;
    for chunk in events.chunks(EVENTS_PER_STATEMENT) {
        sqlx::query(&format!(
            "INSERT INTO {NS}_event (sequence, run_id, kind, payload) \
             SELECT (entry.value->>'sequence')::bigint, $1, \
                    entry.value->'kind', entry.value->'payload' \
             FROM jsonb_array_elements($2::jsonb) WITH ORDINALITY AS entry(value, ordinal) \
             ORDER BY entry.ordinal"
        ))
        .bind(&run_id.0)
        .bind(Json(chunk))
        .execute(&mut **tx)
        .await
        .map_err(reject)?;
    }
    Ok(())
}

fn projection_from_snapshot(snapshot: RunRecoverySnapshot) -> Projection {
    let thread_id = snapshot.thread_id;
    let mut projection = Projection {
        sequence: snapshot.store_cursor,
        messages: snapshot
            .messages
            .into_iter()
            .map(|message| (thread_id.clone(), message))
            .collect(),
        state: snapshot
            .state
            .into_iter()
            .map(|command| (thread_id.clone(), command))
            .collect(),
        events: snapshot.events,
        ..Projection::default()
    };
    if let Some(latest_id) = snapshot.latest_run_id
        && let Some(latest) = snapshot.runs.iter().find(|record| record.id == latest_id)
    {
        projection
            .latest_by_thread
            .insert(thread_id.clone(), latest.clone());
    }
    for record in snapshot.runs {
        projection.run_records.insert(record.id.clone(), record);
    }
    for entry in snapshot.resume_tickets {
        projection.resume_tickets.insert(entry.run_id, entry.ticket);
    }
    projection
}

fn advance_projection(
    projection: &mut Projection,
    commit: ThreadCommit,
    sequence: u64,
    committed_events: Vec<EventRecord>,
) {
    let run_id = commit.run_id().clone();
    let thread_id = commit.thread_id.clone();
    let run_state = commit.run_state();
    let resume_ticket = commit.resume_ticket().cloned();
    let awaiting = matches!(run_state, RunState::Awaiting);
    projection.sequence = projection.sequence.max(sequence);
    for message in commit.messages {
        projection.messages.push((thread_id.clone(), message));
    }
    for command in commit.state {
        projection.state.push((thread_id.clone(), command));
    }
    projection.events.extend(committed_events);
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
        projection.resume_tickets.insert(
            run_id.clone(),
            resume_ticket.expect("awaiting has a ticket"),
        );
    } else {
        projection.resume_tickets.remove(&run_id);
    }
}

impl CommittedThreadView for PostgresCommitCoordinator {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        PostgresCommitCoordinator::committed_messages(self, thread_id)
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
        PostgresCommitCoordinator::committed_state(self, thread_id)
    }
}

/// The durable event read port queries committed SQL directly. The process-local
/// execution view cannot be used for a cross-replica or cold-reader cursor.
#[async_trait]
impl CheckpointReader for PostgresCommitCoordinator {
    async fn list_events(
        &self,
        scope: &EventScope,
        from: Option<u64>,
        limit: usize,
    ) -> Result<Vec<EventRecord>, String> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let after = encode_authority(from.unwrap_or(0)).map_err(|error| error.to_string())?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let (source, predicate, scope_id) = match scope {
            EventScope::All => (format!("{NS}_event AS event"), "", ""),
            EventScope::Run(run_id) => (
                format!("{NS}_event AS event"),
                "AND event.run_id = $3",
                run_id.0.as_str(),
            ),
            EventScope::Thread(thread_id) => (
                format!(
                    "(SELECT DISTINCT run_id FROM {NS}_commit WHERE thread_id = $3) AS run \
                     JOIN {NS}_event AS event ON event.run_id = run.run_id"
                ),
                "",
                thread_id.0.as_str(),
            ),
        };
        let statement = format!(
            "SELECT event.sequence, event.run_id, event.kind, event.payload \
             FROM {source} WHERE event.sequence > $1 {predicate} \
             ORDER BY event.sequence LIMIT $2"
        );
        let query = sqlx::query(&statement).bind(after).bind(limit);
        let rows = match scope {
            EventScope::All => query.fetch_all(&self.pool).await,
            EventScope::Run(_) | EventScope::Thread(_) => {
                query.bind(scope_id).fetch_all(&self.pool).await
            }
        }
        .map_err(|error| error.to_string())?;
        rows.into_iter()
            .map(|row| {
                let sequence: i64 = row.try_get("sequence").map_err(|error| error.to_string())?;
                let Json(kind): Json<AuditKind> =
                    row.try_get("kind").map_err(|error| error.to_string())?;
                let Json(payload): Json<serde_json::Value> =
                    row.try_get("payload").map_err(|error| error.to_string())?;
                Ok(EventRecord {
                    sequence: decode_authority(sequence).map_err(|error| error.to_string())?,
                    run_id: RunId(row.try_get("run_id").map_err(|error| error.to_string())?),
                    kind,
                    payload,
                })
            })
            .collect()
    }
}

#[async_trait]
impl RunRecoverySource for PostgresCommitCoordinator {
    async fn recovery_snapshot(
        &self,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(recovery_reject)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(recovery_reject)?;

        let store_cursor: i64 = sqlx::query_scalar(&format!(
            "SELECT COALESCE(MAX(sequence), 0) FROM {p}_commit"
        ))
        .fetch_one(&mut *tx)
        .await
        .map_err(recovery_reject)?;
        let thread_version: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {p}_commit WHERE thread_id = $1"
        ))
        .bind(&thread_id.0)
        .fetch_one(&mut *tx)
        .await
        .map_err(recovery_reject)?;
        let next_commit_ordinal: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {p}_commit WHERE run_id = $1"
        ))
        .bind(&claimed_run_id.0)
        .fetch_one(&mut *tx)
        .await
        .map_err(recovery_reject)?;

        let commit_rows = sqlx::query(&format!(
            "SELECT run_id, phase FROM {p}_commit WHERE thread_id = $1 ORDER BY sequence"
        ))
        .bind(&thread_id.0)
        .fetch_all(&mut *tx)
        .await
        .map_err(recovery_reject)?;
        let (runs, latest_run_id) = project_committed_run_records(
            thread_id,
            commit_rows.into_iter().map(|row| {
                let run_id = RunId(
                    row.try_get::<String, _>("run_id")
                        .map_err(recovery_reject)?,
                );
                let Json(state): Json<RunState> = row.try_get("phase").map_err(recovery_reject)?;
                Ok::<_, RecoveryError>((run_id, state))
            }),
        )?;

        let message_rows = sqlx::query(&format!(
            "SELECT commit_sequence, data FROM {p}_message WHERE thread_id = $1 ORDER BY id"
        ))
        .bind(&thread_id.0)
        .fetch_all(&mut *tx)
        .await
        .map_err(recovery_reject)?;
        let mut messages = Vec::with_capacity(message_rows.len());
        let mut message_commit_cursors = Vec::with_capacity(message_rows.len());
        for row in message_rows {
            let sequence = row
                .try_get::<i64, _>("commit_sequence")
                .map_err(recovery_reject)?;
            let Json(message): Json<Message> = row.try_get("data").map_err(recovery_reject)?;
            message_commit_cursors.push(
                decode_authority(sequence)
                    .map_err(|error| RecoveryError::Rejected(error.to_string()))?,
            );
            messages.push(message);
        }

        let state_rows = sqlx::query(&format!(
            "SELECT commit_sequence, data FROM {p}_state_command WHERE thread_id = $1 ORDER BY id"
        ))
        .bind(&thread_id.0)
        .fetch_all(&mut *tx)
        .await
        .map_err(recovery_reject)?;
        let mut committed_state = Vec::with_capacity(state_rows.len());
        let mut state_commit_cursors = Vec::with_capacity(state_rows.len());
        for row in state_rows {
            let sequence = row
                .try_get::<i64, _>("commit_sequence")
                .map_err(recovery_reject)?;
            let Json(command): Json<StateCommand> = row.try_get("data").map_err(recovery_reject)?;
            state_commit_cursors.push(
                decode_authority(sequence)
                    .map_err(|error| RecoveryError::Rejected(error.to_string()))?,
            );
            committed_state.push(command);
        }

        // Scope event/ticket reads with the Run set from this transaction's
        // committed Thread prefix. This permits direct use of the Run index
        // and prevents a damaged cache owner from misattributing committed facts.
        // One array bind preserves index access without one query per Run.
        let committed_run_ids = runs.iter().map(|run| run.id.0.as_str()).collect::<Vec<_>>();
        let event_rows = sqlx::query(&format!(
            "SELECT sequence, run_id, kind, payload FROM {p}_event \
             WHERE run_id = ANY($1) ORDER BY sequence"
        ))
        .bind(&committed_run_ids)
        .fetch_all(&mut *tx)
        .await
        .map_err(recovery_reject)?;
        let mut events = Vec::with_capacity(event_rows.len());
        for row in event_rows {
            let sequence = row.try_get::<i64, _>("sequence").map_err(recovery_reject)?;
            let run_id = row
                .try_get::<String, _>("run_id")
                .map_err(recovery_reject)?;
            let Json(kind) = row
                .try_get::<Json<awaken_agent_contract::audit::kind::Kind>, _>("kind")
                .map_err(recovery_reject)?;
            let Json(payload) = row
                .try_get::<Json<serde_json::Value>, _>("payload")
                .map_err(recovery_reject)?;
            events.push(EventRecord {
                sequence: decode_authority(sequence)
                    .map_err(|error| RecoveryError::Rejected(error.to_string()))?,
                run_id: RunId(run_id),
                kind,
                payload,
            });
        }

        let ticket_rows = sqlx::query(&format!(
            "SELECT run_id, ticket FROM {p}_waiting \
             WHERE run_id = ANY($1) ORDER BY run_id"
        ))
        .bind(&committed_run_ids)
        .fetch_all(&mut *tx)
        .await
        .map_err(recovery_reject)?;
        let mut resume_tickets = Vec::with_capacity(ticket_rows.len());
        for row in ticket_rows {
            let run_id = RunId(
                row.try_get::<String, _>("run_id")
                    .map_err(recovery_reject)?,
            );
            let is_awaiting = runs
                .iter()
                .any(|run| run.id == run_id && matches!(&run.state, RunState::Awaiting));
            if !is_awaiting {
                tracing::error!(
                    awaken.run.id = %run_id.0,
                    awaken.thread.id = %thread_id.0,
                    "quarantining a stale waiting row whose Run is not Awaiting"
                );
                continue;
            }
            let Json(ticket): Json<serde_json::Value> =
                row.try_get("ticket").map_err(recovery_reject)?;
            if let Some(ticket) = decode_resume_ticket(&run_id, thread_id, ticket) {
                resume_tickets.push(RunResumeTicket { run_id, ticket });
            }
        }

        let timestamp_rows = sqlx::query(&format!(
            "SELECT sequence, FLOOR(EXTRACT(EPOCH FROM committed_at) * 1000)::BIGINT AS millis FROM {p}_commit WHERE thread_id = $1 ORDER BY sequence"
        )).bind(&thread_id.0).fetch_all(&mut *tx).await.map_err(recovery_reject)?;
        let mut commit_timestamps = std::collections::BTreeMap::new();
        for row in timestamp_rows {
            let sequence: i64 = row.try_get("sequence").map_err(recovery_reject)?;
            let millis: i64 = row.try_get("millis").map_err(recovery_reject)?;
            commit_timestamps.insert(
                decode_authority(sequence)
                    .map_err(|error| RecoveryError::Rejected(error.to_string()))?,
                decode_authority(millis)
                    .map_err(|error| RecoveryError::Rejected(error.to_string()))?,
            );
        }
        tx.commit().await.map_err(recovery_reject)?;
        Ok(RunRecoverySnapshot {
            commit_timestamps,
            thread_id: thread_id.clone(),
            claimed_run_id: claimed_run_id.clone(),
            runs,
            latest_run_id,
            messages,
            message_commit_cursors,
            state: committed_state,
            state_commit_cursors,
            events,
            resume_tickets,
            thread_version: decode_authority(thread_version)
                .map_err(|error| RecoveryError::Rejected(error.to_string()))?,
            store_cursor: decode_authority(store_cursor)
                .map_err(|error| RecoveryError::Rejected(error.to_string()))?,
            next_commit_ordinal: decode_authority(next_commit_ordinal)
                .map_err(|error| RecoveryError::Rejected(error.to_string()))?,
        })
    }
}

/// Authoritative lifecycle feed for active-active PostgreSQL Control Nodes.
///
/// Unlike the synchronous compatibility read ports, this query never consults
/// the process-local projection. PostgreSQL computes the preceding state per Run
/// before applying the exclusive cursor, so `Running` after `Awaiting` remains a
/// `Resumed` event even when another Control Node committed both transitions.
#[async_trait]
impl RunLifecycleFeed for PostgresCommitCoordinator {
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
        let after = i64::try_from(cursor.0)
            .map_err(|_| RunLifecycleFeedError::Rejected("cursor exceeds BIGINT range".into()))?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let prefix = NS;
        let rows = sqlx::query(&format!(
            "WITH lifecycle AS (\
                 SELECT event.sequence, event.run_id, run.thread_id, event.kind, event.payload, \
                        (SELECT prior.payload FROM {prefix}_event AS prior \
                         WHERE prior.run_id = event.run_id \
                           AND prior.sequence < event.sequence \
                           AND prior.kind::text = '\"RunStateChanged\"' \
                         ORDER BY prior.sequence DESC LIMIT 1) AS previous_payload \
                 FROM {prefix}_event AS event \
                 JOIN {prefix}_run_record AS run ON run.run_id = event.run_id \
                 WHERE event.kind::text IN ('\"RunStateChanged\"', '\"RunRescheduled\"')\
             ) \
             SELECT sequence, run_id, thread_id, kind, payload, previous_payload \
             FROM lifecycle WHERE sequence > $1 ORDER BY sequence LIMIT $2"
        ))
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| RunLifecycleFeedError::Rejected(error.to_string()))?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let sequence = row
                .try_get::<i64, _>("sequence")
                .map_err(|error| RunLifecycleFeedError::Rejected(error.to_string()))?;
            let sequence = u64::try_from(sequence).map_err(|_| {
                RunLifecycleFeedError::Rejected(
                    "persisted lifecycle sequence is negative".to_string(),
                )
            })?;
            let Json(payload): Json<serde_json::Value> = row
                .try_get("payload")
                .map_err(|error| RunLifecycleFeedError::Rejected(error.to_string()))?;
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
            let previous = row
                .try_get::<Option<Json<serde_json::Value>>, _>("previous_payload")
                .map_err(|error| RunLifecycleFeedError::Rejected(error.to_string()))?
                .map(|Json(payload)| {
                    serde_json::from_value::<RunState>(
                        payload.get("state").cloned().unwrap_or_default(),
                    )
                    .map_err(|_| RunLifecycleFeedError::InvalidState { sequence })
                })
                .transpose()?;
            let Json(audit_kind): Json<AuditKind> = row
                .try_get("kind")
                .map_err(|error| RunLifecycleFeedError::Rejected(error.to_string()))?;
            events.push(RunLifecycleEvent {
                cursor: RunLifecycleCursor(sequence),
                source_commit_cursor: decode_run_lifecycle_cursor(RunLifecycleCursor(sequence)).0,
                thread_id: ThreadId(
                    row.try_get("thread_id")
                        .map_err(|error| RunLifecycleFeedError::Rejected(error.to_string()))?,
                ),
                run_id: RunId(
                    row.try_get("run_id")
                        .map_err(|error| RunLifecycleFeedError::Rejected(error.to_string()))?,
                ),
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

fn lock(
    projection: &InvariantLock<Projection>,
) -> Result<std::sync::MutexGuard<'_, Projection>, Error> {
    projection
        .lock()
        .map_err(|_| Error::Rejected("commit projection poisoned".to_string()))
}

fn reject(err: sqlx::Error) -> Error {
    Error::Rejected(err.to_string())
}

fn recovery_reject(err: sqlx::Error) -> RecoveryError {
    RecoveryError::Rejected(err.to_string())
}

/// Decode one isolated waiting row without making the Thread's complete
/// recovery snapshot or process startup unavailable. The durable row is kept;
/// canonical Runtime interruption owns reconstruction or terminal quarantine.
fn decode_resume_ticket(
    row_run_id: &RunId,
    expected_thread_id: &ThreadId,
    value: serde_json::Value,
) -> Option<ResumeTicket> {
    match decode_resume_ticket_value_for_owner(value, row_run_id, expected_thread_id) {
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

#[cfg(test)]
mod migration_tests {
    use super::*;

    #[test]
    fn commit_sequence_creation_uses_the_scoped_receipt_as_its_only_guard() {
        // Cause/effect decision table: portable schema + no receipts => run
        // historical V1 then V2; V1-only database => compact dependent keys and
        // drop the sequence; both receipts => no-op; edited/unknown receipt =>
        // fail closed. The published constructor remains the only compatibility
        // owner, so adapters never rewrite the migration ledger.
        let bundle = commit_pg_bundle().expect("deterministic Postgres bundle");
        assert_eq!(bundle.migrations().len(), 2);
        assert_eq!(bundle.migrations()[0].version(), 1);
        assert_eq!(bundle.migrations()[1].version(), 2);
        assert!(COMMIT_PG_FILES[1].1.contains("ROW_NUMBER()"));
        assert!(COMMIT_PG_FILES[1].1.contains("DROP SEQUENCE"));
    }
}
