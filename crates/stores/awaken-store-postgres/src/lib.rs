//! Postgres durable implementation of the runtime commit boundary.
//!
//! [`PostgresCommitCoordinator`] implements the neutral [`Coordinator`] write
//! boundary (G1/G13) against Postgres: each `commit` writes the staged
//! `ThreadCommit` — messages, state commands, events, the run fact, and the
//! awaiting-ticket transition — in one SQL transaction (ADR-0006). The committed
//! fact log is the authority; the `run_record` table is a derived cache equal to
//! the latest fact (G32).
//!
//! The `CommittedThreadView` read port is synchronous, so the
//! coordinator keeps an in-memory projection of committed truth that it rebuilds
//! from Postgres on construction (durable across restart) and updates in lockstep
//! with each commit. The projection is never an independent authority — it always
//! equals what replay would derive from the log.

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
};
use awaken_store_schema::StoredU64;
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

/// Bundle id for the Postgres-only commit-sequence object. Scoped so it never
/// collides with the portable commit schema (`awaken.runtime_commit`) or the
/// dispatch schema in a shared database.
pub const COMMIT_PG_BUNDLE_ID: &str = "awaken.runtime_commit_pg";

/// The Postgres-only migration for the commit-sequence object, embedded from its
/// `.sql` file so the DDL lives beside the schema, not in a Rust string.
const COMMIT_PG_FILES: &[(&str, &str)] = &[(
    "V0001__commit_sequence.sql",
    include_str!("migrations/V0001__commit_sequence.sql"),
)];

/// Build the Postgres-only commit-sequence bundle. Kept separate from the portable
/// [`commit_bundle`] because a Postgres `SEQUENCE` has no SQLite analogue: the
/// SQLite backend allocates the commit sequence in-process (single writer), so only
/// the Postgres path needs a lock-free database-side allocator. Runs after the
/// portable commit schema, so `{prefix}_commit` exists when the sequence is seeded.
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

/// In-memory projection of committed truth, rebuilt from Postgres on construction
/// and advanced with every commit. Serves the synchronous read ports.
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

/// The synchronous execution projection retains complete transcript/state/event
/// facts. Refuse startup before allocating an unbounded fact history; large
/// authorities must use compaction/snapshot export rather than risking OOM.
const MAX_HYDRATED_FACT_ROWS: u64 = 1_000_000;

/// A Postgres-backed [`Coordinator`] plus the read ports it serves.
pub struct PostgresCommitCoordinator {
    pool: PgPool,
    projection: Mutex<Projection>,
}

impl PostgresCommitCoordinator {
    async fn pool(url: &str, max_connections: u32) -> Result<PgPool, StoreError> {
        sqlx::postgres::PgPoolOptions::new()
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

    /// Connect, apply the commit-schema migrations, and hydrate the projection.
    ///
    /// Pool sizing is an explicit composition input, not store-owned process
    /// configuration.
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = Self::pool(url, max_connections).await?;
        Self::with_pool(pool).await
    }

    /// Connect to commit schemas applied by the deployment migration phase.
    /// The ledger is verified before the durable projection is hydrated; no DDL
    /// is executed on this path.
    pub async fn connect_existing(url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = Self::pool(url, max_connections).await?;
        Self::with_existing_pool(pool).await
    }

    /// Build from an existing pool: apply migrations and hydrate the projection
    /// under the runtime namespace.
    pub async fn with_pool(pool: PgPool) -> Result<Self, StoreError> {
        Self::migrate_pool(&pool).await?;
        Self::hydrate_pool(pool).await
    }

    /// Build from an existing pool after verifying both the portable commit
    /// bundle and the Postgres-only sequence bundle.
    pub async fn with_existing_pool(pool: PgPool) -> Result<Self, StoreError> {
        Self::verify_pool(&pool).await?;
        Self::hydrate_pool(pool).await
    }

    async fn hydrate_pool(pool: PgPool) -> Result<Self, StoreError> {
        let projection = hydrate(&pool)
            .await
            .map_err(|err| StoreError::Hydrate(err.to_string()))?;
        Ok(Self {
            pool,
            projection: Mutex::new(projection),
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

    /// The active awaiting ticket for a run, if it is currently awaiting.
    pub fn resume_ticket_for(&self, run_id: &RunId) -> Option<ResumeTicket> {
        self.projection
            .lock()
            .ok()
            .and_then(|p| p.resume_tickets.get(run_id).cloned())
    }

    /// Committed state commands for a thread, in commit order — served from the
    /// durable `state_command` rows the projection rebuilt on connect, so a resumed
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
            "SELECT latest.run_id, waiting.ticket \
             FROM (\
                 SELECT run_id FROM {NS}_commit \
                 WHERE thread_id = $1 ORDER BY sequence DESC LIMIT 1\
             ) AS latest \
             JOIN {NS}_waiting AS waiting ON waiting.run_id = latest.run_id"
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
            let Json(ticket): Json<ResumeTicket> = row
                .try_get("ticket")
                .map_err(|error| StoreError::Read(error.to_string()))?;
            Ok((run_id, ticket))
        })
        .transpose()
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
        operation
            .commit
            .validate()
            .map_err(|error| Error::Rejected(error.to_string()))?;
        if operation.operation_id.run_id != *operation.commit.run_id() {
            return Err(Error::Rejected(
                "commit operation run_id does not match ThreadCommit run_id".to_string(),
            ));
        }
        let operation_ordinal = i64::try_from(operation.operation_id.ordinal).map_err(|_| {
            Error::Rejected("commit operation ordinal exceeds backend range".to_string())
        })?;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let current_version = lock_thread_version(&mut tx, &operation.commit.thread_id).await?;
        if let Some(receipt) = load_receipt(&mut tx, &operation).await? {
            return Ok(receipt);
        }
        if current_version != operation.expected_thread_version {
            return Err(Error::Rejected(format!(
                "thread version conflict: expected {}, current {}",
                operation.expected_thread_version, current_version
            )));
        }
        let current_ordinal: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {NS}_commit WHERE run_id = $1"
        ))
        .bind(&operation.operation_id.run_id.0)
        .fetch_one(&mut *tx)
        .await
        .map_err(reject)?;
        let current_ordinal = decode_authority(current_ordinal)?;
        if current_ordinal != operation.operation_id.ordinal {
            return Err(Error::Rejected(format!(
                "commit operation ordinal conflict: expected {}, current {}",
                operation.operation_id.ordinal, current_ordinal
            )));
        }
        let (next, committed_events) = append_commit(&mut tx, &operation.commit).await?;
        let thread_version = increment_authority(current_version)?;
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
        let receipt = CommitReceipt {
            operation_id: operation.operation_id,
            commit_sequence: next,
            thread_version,
            payload_hash: operation.payload_hash,
            duplicate: false,
        };
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
) -> Result<Option<CommitReceipt>, Error> {
    let row = sqlx::query(&format!(
        "SELECT payload_hash, commit_sequence, thread_version \
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
    let payload_hash: String = row.try_get("payload_hash").map_err(reject)?;
    if payload_hash != operation.payload_hash.0 {
        return Err(Error::Rejected(format!(
            "commit operation {}:{} was reused with another payload",
            operation.operation_id.run_id.0, operation.operation_id.ordinal
        )));
    }
    Ok(Some(CommitReceipt {
        operation_id: operation.operation_id.clone(),
        commit_sequence: decode_authority(
            row.try_get::<i64, _>("commit_sequence").map_err(reject)?,
        )?,
        thread_version: decode_authority(row.try_get::<i64, _>("thread_version").map_err(reject)?)?,
        payload_hash: CommitPayloadHash(payload_hash),
        duplicate: true,
    }))
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

    let next: i64 = sqlx::query_scalar(&format!("SELECT nextval('{p}_commit_seq')"))
        .fetch_one(&mut **tx)
        .await
        .map_err(reject)?;
    let next = decode_authority(next)?;
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

    for message in &commit.messages {
        sqlx::query(&format!(
            "INSERT INTO {p}_message (commit_sequence, thread_id, data) VALUES ($1, $2, $3)"
        ))
        .bind(encode_authority(next)?)
        .bind(&thread_id.0)
        .bind(Json(message))
        .execute(&mut **tx)
        .await
        .map_err(reject)?;
    }
    for command in &commit.state {
        sqlx::query(&format!(
            "INSERT INTO {p}_state_command (commit_sequence, thread_id, data) VALUES ($1, $2, $3)"
        ))
        .bind(encode_authority(next)?)
        .bind(&thread_id.0)
        .bind(Json(command))
        .execute(&mut **tx)
        .await
        .map_err(reject)?;
    }
    let mut committed_events = Vec::with_capacity(commit.events.len());
    for (offset, draft) in commit.events.iter().enumerate() {
        let sequence = encode_run_lifecycle_cursor(next, offset)
            .map_err(|error| Error::Rejected(error.to_string()))?;
        sqlx::query(&format!(
            "INSERT INTO {p}_event (sequence, run_id, kind, payload) VALUES ($1, $2, $3, $4)"
        ))
        .bind(encode_authority(sequence.0)?)
        .bind(&run_id.0)
        .bind(Json(&draft.kind))
        .bind(Json(&draft.payload))
        .execute(&mut **tx)
        .await
        .map_err(reject)?;
        committed_events.push(EventRecord {
            sequence: sequence.0,
            run_id: run_id.clone(),
            kind: draft.kind.clone(),
            payload: draft.payload.clone(),
        });
    }
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

/// The merged read repository (ADR-0039 D1), served from the fact-derived
/// projection that `hydrate` rebuilt from the durable tables (D4).
impl CheckpointReader for PostgresCommitCoordinator {
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
        let mut runs: Vec<RunRecord> = Vec::new();
        let mut latest_run_id = None;
        for row in commit_rows {
            let run_id = RunId(
                row.try_get::<String, _>("run_id")
                    .map_err(recovery_reject)?,
            );
            let Json(state): Json<RunState> = row.try_get("phase").map_err(recovery_reject)?;
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

        let event_rows = sqlx::query(&format!(
            "SELECT event.sequence, event.run_id, event.kind, event.payload \
             FROM {p}_event AS event \
             JOIN {p}_run_record AS run ON run.run_id = event.run_id \
             WHERE run.thread_id = $1 ORDER BY event.sequence"
        ))
        .bind(&thread_id.0)
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
            "SELECT waiting.run_id, waiting.ticket \
             FROM {p}_waiting AS waiting \
             JOIN {p}_run_record AS run ON run.run_id = waiting.run_id \
             WHERE run.thread_id = $1 ORDER BY waiting.run_id"
        ))
        .bind(&thread_id.0)
        .fetch_all(&mut *tx)
        .await
        .map_err(recovery_reject)?;
        let mut resume_tickets = Vec::with_capacity(ticket_rows.len());
        for row in ticket_rows {
            let run_id = RunId(
                row.try_get::<String, _>("run_id")
                    .map_err(recovery_reject)?,
            );
            let Json(ticket): Json<ResumeTicket> =
                row.try_get("ticket").map_err(recovery_reject)?;
            resume_tickets.push(RunResumeTicket { run_id, ticket });
        }

        tx.commit().await.map_err(recovery_reject)?;
        Ok(RunRecoverySnapshot {
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
                           AND prior.kind::text IN ('\"RunStateChanged\"', '\"RunPhaseChanged\"') \
                         ORDER BY prior.sequence DESC LIMIT 1) AS previous_payload \
                 FROM {prefix}_event AS event \
                 JOIN {prefix}_run_record AS run ON run.run_id = event.run_id \
                 WHERE event.kind::text IN (\
                     '\"RunStateChanged\"', '\"RunPhaseChanged\"', '\"RunRescheduled\"'\
                 )\
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

fn lock(projection: &Mutex<Projection>) -> Result<std::sync::MutexGuard<'_, Projection>, Error> {
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

/// Rebuild the read projection from the committed log in Postgres.
async fn hydrate(pool: &PgPool) -> Result<Projection, sqlx::Error> {
    // Hydration is one logical read. Without a shared snapshot, a commit can land
    // between the sequence/messages/run queries and produce a projection that
    // never existed in durable truth (for example sequence N with rows from N+1).
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let projection = hydrate_snapshot(&mut tx).await?;
    tx.commit().await?;
    Ok(projection)
}

async fn hydrate_snapshot(tx: &mut Transaction<'_, Postgres>) -> Result<Projection, sqlx::Error> {
    let counts = sqlx::query(&format!(
        "SELECT \
            (SELECT COUNT(*) FROM {NS}_message) AS messages, \
            (SELECT COUNT(*) FROM {NS}_state_command) AS states, \
            (SELECT COUNT(*) FROM {NS}_commit) AS commits, \
            (SELECT COUNT(*) FROM {NS}_event) AS events, \
            (SELECT COUNT(*) FROM {NS}_waiting) AS waiting"
    ))
    .fetch_one(&mut **tx)
    .await?;
    enforce_hydration_bound([
        counts.try_get("messages")?,
        counts.try_get("states")?,
        counts.try_get("commits")?,
        counts.try_get("events")?,
        counts.try_get("waiting")?,
    ])?;

    let mut projection = Projection::default();

    let sequence: i64 = sqlx::query_scalar(&format!(
        "SELECT COALESCE(MAX(sequence), 0) FROM {NS}_commit"
    ))
    .fetch_one(&mut **tx)
    .await?;
    projection.sequence = StoredU64::try_from(sequence)
        .map_err(|error| sqlx::Error::Decode(Box::new(error)))?
        .domain_value();

    let message_rows = sqlx::query(&format!(
        "SELECT thread_id, data FROM {NS}_message ORDER BY id"
    ))
    .fetch_all(&mut **tx)
    .await?;
    for row in message_rows {
        let thread_id: String = row.try_get("thread_id")?;
        let Json(message): Json<Message> = row.try_get("data")?;
        projection.messages.push((ThreadId(thread_id), message));
    }

    // Rebuild the committed state-command log per thread, in commit order, so a
    // resumed run replays its accumulated state from durable truth (G1/G13).
    let state_rows = sqlx::query(&format!(
        "SELECT thread_id, data FROM {NS}_state_command ORDER BY id"
    ))
    .fetch_all(&mut **tx)
    .await?;
    for row in state_rows {
        let thread_id: String = row.try_get("thread_id")?;
        let Json(command): Json<StateCommand> = row.try_get("data")?;
        projection.state.push((ThreadId(thread_id), command));
    }

    // Fold the commit log in order so the latest fact per run wins (G32).
    let commit_rows = sqlx::query(&format!(
        "SELECT run_id, thread_id, phase FROM {NS}_commit ORDER BY sequence"
    ))
    .fetch_all(&mut **tx)
    .await?;
    for row in commit_rows {
        let run_id: String = row.try_get("run_id")?;
        let thread_id: String = row.try_get("thread_id")?;
        let Json(state): Json<RunState> = row.try_get("phase")?;
        let record = RunRecord {
            id: RunId(run_id.clone()),
            thread_id: ThreadId(thread_id.clone()),
            state,
        };
        projection.run_records.insert(RunId(run_id), record.clone());
        projection
            .latest_by_thread
            .insert(ThreadId(thread_id), record);
    }

    // Rebuild the committed event cache from the durable event log, in order.
    let event_rows = sqlx::query(&format!(
        "SELECT sequence, run_id, kind, payload FROM {NS}_event ORDER BY sequence"
    ))
    .fetch_all(&mut **tx)
    .await?;
    for row in event_rows {
        let sequence: i64 = row.try_get("sequence")?;
        let run_id: String = row.try_get("run_id")?;
        let Json(kind) =
            row.try_get::<Json<awaken_agent_contract::audit::kind::Kind>, _>("kind")?;
        let Json(payload) = row.try_get::<Json<serde_json::Value>, _>("payload")?;
        projection.events.push(EventRecord {
            sequence: StoredU64::try_from(sequence)
                .map_err(|error| sqlx::Error::Decode(Box::new(error)))?
                .domain_value(),
            run_id: RunId(run_id),
            kind,
            payload,
        });
    }

    let resume_ticket_rows = sqlx::query(&format!("SELECT run_id, ticket FROM {NS}_waiting"))
        .fetch_all(&mut **tx)
        .await?;
    for row in resume_ticket_rows {
        let run_id: String = row.try_get("run_id")?;
        let Json(ticket): Json<ResumeTicket> = row.try_get("ticket")?;
        projection.resume_tickets.insert(RunId(run_id), ticket);
    }

    Ok(projection)
}

fn enforce_hydration_bound(counts: [i64; 5]) -> Result<(), sqlx::Error> {
    let total = counts.into_iter().try_fold(0_u64, |total, count| {
        let count = u64::try_from(count)
            .map_err(|_| sqlx::Error::Protocol("negative hydration row count".into()))?;
        total
            .checked_add(count)
            .ok_or_else(|| sqlx::Error::Protocol("hydration row count overflow".into()))
    })?;
    if total > MAX_HYDRATED_FACT_ROWS {
        return Err(sqlx::Error::Protocol(format!(
            "runtime projection has {total} facts, exceeding the safe startup limit of \
             {MAX_HYDRATED_FACT_ROWS}; compact/export a snapshot before restart"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    #[test]
    fn commit_sequence_creation_uses_the_scoped_receipt_as_its_only_guard() {
        // Cause/effect decision table: portable commit schema present + V0001
        // receipt absent => execute the pinned historical create/seed body;
        // canonical receipt present => apply nothing; edited body or unknown
        // receipt => construction/planning fails closed. The published constructor
        // owns compatibility, so adapters never rewrite the migration ledger.
        let bundle = commit_pg_bundle().expect("deterministic Postgres bundle");
        assert_eq!(bundle.migrations().len(), 1);
        assert_eq!(bundle.migrations()[0].version(), 1);
    }

    #[test]
    fn hydration_bound_accepts_the_limit_and_rejects_the_next_fact() {
        // Test design — boundary-value resource-safety contract: a projection
        // containing exactly MAX facts may start; MAX+1 must fail before any
        // fetch_all allocation. Negative/overflowed backend counts also fail
        // closed instead of wrapping into a small allocation estimate.
        assert!(enforce_hydration_bound([1_000_000, 0, 0, 0, 0]).is_ok());
        assert!(enforce_hydration_bound([1_000_000, 1, 0, 0, 0]).is_err());
        assert!(enforce_hydration_bound([-1, 0, 0, 0, 0]).is_err());
    }
}
