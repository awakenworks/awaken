//! Postgres durable implementation of the runtime commit boundary.
//!
//! [`PostgresCommitCoordinator`] implements the neutral [`Coordinator`] write
//! boundary (G1/G13) against Postgres: each `commit` writes the staged
//! `ThreadCommit` — messages, state commands, events, the run fact, and the
//! waiting-ticket transition — in one SQL transaction (ADR-0006). The committed
//! fact log is the authority; the `run_record` table is a derived cache equal to
//! the latest fact (G32).
//!
//! The read ports (`RunStore`, `ThreadReader`) are synchronous, so the
//! coordinator keeps an in-memory projection of committed truth that it rebuilds
//! from Postgres on construction (durable across restart) and updates in lockstep
//! with each commit. The projection is never an independent authority — it always
//! equals what replay would derive from the log.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, Phase, Record as RunRecord};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::WaitingTicket;
use awaken_agent_contract::commit::coordinator::{Coordinator as CommitCoordinator, Error};
use awaken_agent_contract::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::event::record::Record as EventRecord;
use awaken_agent_contract::store::run_store::RunStore;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;

pub use awaken_store_schema::{COMMIT_BUNDLE_ID as BUNDLE_ID, commit_bundle};

/// Errors from constructing or migrating the store. Commit-time failures use the
/// neutral [`Coordinator`] error.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("migrate: {0}")]
    Migrate(String),
    #[error("hydrate: {0}")]
    Hydrate(String),
}

/// In-memory projection of committed truth, rebuilt from Postgres on construction
/// and advanced with every commit. Serves the synchronous read ports.
#[derive(Debug, Default)]
struct Projection {
    sequence: u64,
    messages: Vec<(ThreadId, Message)>,
    run_records: HashMap<RunId, RunRecord>,
    waiting: HashMap<RunId, WaitingTicket>,
}

/// The component namespace for this runtime's tables. One runtime is one
/// component, so its commit and dispatch tables share this prefix; the scoped
/// migration ledger isolates it from any other component in the same database.
/// Built in, not configured.
const NS: &str = "runtime";

/// A Postgres-backed [`Coordinator`] plus the read ports it serves.
pub struct PostgresCommitCoordinator {
    pool: PgPool,
    projection: Mutex<Projection>,
}

impl PostgresCommitCoordinator {
    /// Connect, apply the commit-schema migrations, and hydrate the projection.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = PgPool::connect(url)
            .await
            .map_err(|err| StoreError::Connect(err.to_string()))?;
        Self::with_pool(pool).await
    }

    /// Build from an existing pool: apply migrations and hydrate the projection
    /// under the runtime namespace.
    pub async fn with_pool(pool: PgPool) -> Result<Self, StoreError> {
        let bundle = commit_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|err| StoreError::Migrate(err.to_string()))?
            .run_bundle(&bundle)
            .await
            .map_err(|err| StoreError::Migrate(err.to_string()))?;

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

    /// The active waiting ticket for a run, if it is currently parked.
    pub fn waiting_for(&self, run_id: &RunId) -> Option<WaitingTicket> {
        self.projection
            .lock()
            .ok()
            .and_then(|p| p.waiting.get(run_id).cloned())
    }
}

#[async_trait]
impl CommitCoordinator for PostgresCommitCoordinator {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, Error> {
        let next = {
            let projection = lock(&self.projection)?;
            projection.sequence + 1
        };
        let run_id = commit.run_fact.run_id.clone();
        let thread_id = commit.thread_id.clone();
        let phase = commit.run_fact.phase.clone();
        let p = NS;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|err| Error::Rejected(err.to_string()))?;

        sqlx::query(&format!(
            "INSERT INTO {p}_commit (sequence, thread_id, run_id, phase) VALUES ($1, $2, $3, $4)"
        ))
        .bind(next as i64)
        .bind(&thread_id.0)
        .bind(&run_id.0)
        .bind(Json(&phase))
        .execute(&mut *tx)
        .await
        .map_err(reject)?;

        for message in &commit.messages {
            sqlx::query(&format!(
                "INSERT INTO {p}_message (commit_sequence, thread_id, data) VALUES ($1, $2, $3)"
            ))
            .bind(next as i64)
            .bind(&thread_id.0)
            .bind(Json(message))
            .execute(&mut *tx)
            .await
            .map_err(reject)?;
        }

        for command in &commit.state {
            sqlx::query(&format!(
                "INSERT INTO {p}_state_command (commit_sequence, thread_id, data) VALUES ($1, $2, $3)"
            ))
            .bind(next as i64)
            .bind(&thread_id.0)
            .bind(Json(command))
            .execute(&mut *tx)
            .await
            .map_err(reject)?;
        }

        let mut committed_events = Vec::with_capacity(commit.events.len());
        for (offset, draft) in commit.events.iter().enumerate() {
            let sequence = next * 1_000 + offset as u64;
            sqlx::query(&format!(
                "INSERT INTO {p}_event (sequence, run_id, kind, payload) VALUES ($1, $2, $3, $4)"
            ))
            .bind(sequence as i64)
            .bind(&run_id.0)
            .bind(Json(&draft.kind))
            .bind(Json(&draft.payload))
            .execute(&mut *tx)
            .await
            .map_err(reject)?;
            committed_events.push(EventRecord {
                sequence,
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
        .bind(Json(&phase))
        .execute(&mut *tx)
        .await
        .map_err(reject)?;

        // Park or clear the waiting ticket atomically with the checkpoint: a
        // `Some` ticket on a `Waiting` phase parks the run; anything else clears
        // it so a resumed/terminal run can no longer be resumed (G5).
        let parked = matches!((&commit.waiting, &phase), (Some(_), Phase::Waiting));
        if parked {
            let ticket = commit.waiting.as_ref().expect("parked has a ticket");
            sqlx::query(&format!(
                "INSERT INTO {p}_waiting (run_id, ticket) VALUES ($1, $2) \
                 ON CONFLICT (run_id) DO UPDATE SET ticket = EXCLUDED.ticket"
            ))
            .bind(&run_id.0)
            .bind(Json(ticket))
            .execute(&mut *tx)
            .await
            .map_err(reject)?;
        } else {
            sqlx::query(&format!("DELETE FROM {p}_waiting WHERE run_id = $1"))
                .bind(&run_id.0)
                .execute(&mut *tx)
                .await
                .map_err(reject)?;
        }

        tx.commit().await.map_err(reject)?;

        // The transaction is durable; advance the in-memory projection to match.
        let mut projection = lock(&self.projection)?;
        projection.sequence = next;
        for message in commit.messages {
            projection.messages.push((thread_id.clone(), message));
        }
        let record = RunRecord {
            id: run_id.clone(),
            thread_id,
            phase,
        };
        projection.run_records.insert(run_id.clone(), record);
        if parked {
            projection
                .waiting
                .insert(run_id.clone(), commit.waiting.expect("parked has a ticket"));
        } else {
            projection.waiting.remove(&run_id);
        }

        Ok(CommitRecord { sequence: next })
    }
}

impl RunStore for PostgresCommitCoordinator {
    fn get(&self, id: &RunId) -> Option<RunRecord> {
        self.projection
            .lock()
            .ok()
            .and_then(|p| p.run_records.get(id).cloned())
    }
}

impl ThreadReader for PostgresCommitCoordinator {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        PostgresCommitCoordinator::committed_messages(self, thread_id)
    }

    fn waiting_ticket(&self, run_id: &RunId) -> Option<WaitingTicket> {
        self.waiting_for(run_id)
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

/// Rebuild the read projection from the committed log in Postgres.
async fn hydrate(pool: &PgPool) -> Result<Projection, sqlx::Error> {
    let mut projection = Projection::default();

    let sequence: i64 = sqlx::query_scalar(&format!(
        "SELECT COALESCE(MAX(sequence), 0) FROM {NS}_commit"
    ))
    .fetch_one(pool)
    .await?;
    projection.sequence = sequence.max(0) as u64;

    let message_rows = sqlx::query(&format!(
        "SELECT thread_id, data FROM {NS}_message ORDER BY id"
    ))
    .fetch_all(pool)
    .await?;
    for row in message_rows {
        let thread_id: String = row.try_get("thread_id")?;
        let Json(message): Json<Message> = row.try_get("data")?;
        projection.messages.push((ThreadId(thread_id), message));
    }

    // Fold the commit log in order so the latest fact per run wins (G32).
    let commit_rows = sqlx::query(&format!(
        "SELECT run_id, thread_id, phase FROM {NS}_commit ORDER BY sequence"
    ))
    .fetch_all(pool)
    .await?;
    for row in commit_rows {
        let run_id: String = row.try_get("run_id")?;
        let thread_id: String = row.try_get("thread_id")?;
        let Json(phase): Json<Phase> = row.try_get("phase")?;
        projection.run_records.insert(
            RunId(run_id.clone()),
            RunRecord {
                id: RunId(run_id),
                thread_id: ThreadId(thread_id),
                phase,
            },
        );
    }

    let waiting_rows = sqlx::query(&format!("SELECT run_id, ticket FROM {NS}_waiting"))
        .fetch_all(pool)
        .await?;
    for row in waiting_rows {
        let run_id: String = row.try_get("run_id")?;
        let Json(ticket): Json<WaitingTicket> = row.try_get("ticket")?;
        projection.waiting.insert(RunId(run_id), ticket);
    }

    Ok(projection)
}
