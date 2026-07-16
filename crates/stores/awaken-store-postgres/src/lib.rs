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
use awaken_agent_contract::audit::record::Record as EventRecord;
use awaken_agent_contract::thread::commit::coordinator::{Coordinator as CommitCoordinator, Error};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::checkpoint::{CheckpointReader, EventScope};
use awaken_agent_contract::thread::read::run_store::RunStore;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;

pub use awaken_store_schema::{COMMIT_BUNDLE_ID as BUNDLE_ID, commit_bundle};

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
    /// The latest committed run per thread, in commit order (for `latest_run`).
    latest_by_thread: HashMap<ThreadId, RunRecord>,
    /// Committed events in commit order (for `list_events`), a fact-derived cache
    /// rebuilt from the durable event table on construction.
    events: Vec<EventRecord>,
    waiting: HashMap<RunId, WaitingTicket>,
}

/// The component namespace for this runtime's tables. One runtime is one
/// component, so its commit and dispatch tables share this prefix; the scoped
/// migration ledger isolates it from any other component in the same database.
/// Built in, not configured.
const NS: &str = "runtime";

/// Max Postgres connections for a runtime pool: `AWAKEN_PG_MAX_CONNECTIONS` if set,
/// else `available_parallelism() + 8` so the pool covers the served dispatch pool's
/// `available_parallelism()` concurrent drives plus foreground/heartbeat headroom.
/// (sqlx's default of 10 is below the drain concurrency on most hosts and starves.)
pub(crate) fn pg_max_connections() -> u32 {
    std::env::var("AWAKEN_PG_MAX_CONNECTIONS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| {
            let cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4) as u32;
            cores + 8
        })
}

/// A Postgres-backed [`Coordinator`] plus the read ports it serves.
pub struct PostgresCommitCoordinator {
    pool: PgPool,
    projection: Mutex<Projection>,
}

impl PostgresCommitCoordinator {
    /// Connect, apply the commit-schema migrations, and hydrate the projection.
    ///
    /// The pool is sized to the process's drain concurrency, not sqlx's default of
    /// 10: the served dispatch pool spawns `available_parallelism()` drain tasks, and
    /// each concurrent drive holds a connection to commit — with only 10, a burst of
    /// concurrent runs starves for connections and strands the queue. Overridable via
    /// `AWAKEN_PG_MAX_CONNECTIONS` (a shared Postgres fleet may cap it to fit the
    /// server's own `max_connections`).
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(pg_max_connections())
            .connect(url)
            .await
            .map_err(|err| StoreError::Connect(err.to_string()))?;
        Self::with_pool(pool).await
    }

    /// Build from an existing pool: apply migrations and hydrate the projection
    /// under the runtime namespace.
    pub async fn with_pool(pool: PgPool) -> Result<Self, StoreError> {
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
        // The Postgres-only commit-sequence object, applied after the portable
        // schema so `{prefix}_commit` exists when the sequence is seeded past any
        // pre-existing rows. Its own scoped bundle keeps it off the SQLite path.
        let pg_bundle = commit_pg_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        runner
            .run_bundle(&pg_bundle)
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
        commit
            .validate()
            .map_err(|e| Error::Rejected(e.to_string()))?;
        let run_id = commit.run_fact.run_id.clone();
        let thread_id = commit.thread_id.clone();
        let phase = commit.run_fact.phase.clone();
        let p = NS;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|err| Error::Rejected(err.to_string()))?;

        // Terminal-is-final (exactly-once committed LOG under a stale reclaim),
        // fenced DURABLY and IN-TRANSACTION — this is the authoritative check, not
        // the per-process projection. In a fleet the projection is per-process
        // (cross-node visibility is reconnect-only), so a stale owner on a DIFFERENT
        // node would never see the reclaimer's `Ended` in its own projection; only
        // the shared database is common ground. `SELECT ... FOR UPDATE` locks the
        // run's `run_record` row (a row exists once any Running/Ended fact was
        // committed, which is always true by the time a run could be terminal), so
        // two committers racing the same run serialize on it: the first commits
        // `Ended`; the second's read then sees `Ended` and is rejected — no
        // duplicate terminal fact or transcript lands in the shared DB. A rejected
        // commit drops `tx` unread, rolling back and releasing the lock. When no row
        // exists (the run's first-ever commit) there is nothing to reject, so the
        // first commit — even a first `Ended` — lands.
        let existing: Option<Json<Phase>> = sqlx::query_scalar(&format!(
            "SELECT phase FROM {p}_run_record WHERE run_id = $1 FOR UPDATE"
        ))
        .bind(&run_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(reject)?;
        if matches!(existing, Some(Json(Phase::Ended(_)))) {
            return Err(Error::Rejected(format!(
                "run {} is already terminal; refusing post-terminal commit",
                run_id.0
            )));
        }

        // Allocate the commit sequence at the database, not from the in-process
        // projection counter: that counter is per-process, so concurrent commits —
        // parallel drives in one process AND a cross-process fleet — would read the
        // same value and collide on `runtime_commit_pkey`, failing every committer
        // but one. `nextval` on the dedicated sequence allocates the next value with
        // a short internal latch that is NOT held to transaction end, so committers
        // never convoy on one lock across the whole commit fsync (an earlier
        // transaction-scoped advisory lock did exactly that, starving the pool under
        // a fleet burst until leases expired and runs double-executed). A rolled-back
        // allocation leaves a gap; the commit log's contract is strict monotonicity,
        // not contiguity, so a gap is harmless.
        let next: i64 = sqlx::query_scalar(&format!("SELECT nextval('{p}_commit_seq')"))
            .fetch_one(&mut *tx)
            .await
            .map_err(reject)?;
        let next = next as u64;

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
        // The fence only ever moves forward: with lock-free `nextval` allocation two
        // commits can interleave (allocate 5, then 6, but 6 commits first), so take
        // the max rather than clobbering with this commit's own (possibly lower)
        // sequence. `commit_count` reads this fence and must never regress.
        let mut projection = lock(&self.projection)?;
        projection.sequence = projection.sequence.max(next);
        for message in commit.messages {
            projection.messages.push((thread_id.clone(), message));
        }
        projection.events.extend(committed_events);
        let record = RunRecord {
            id: run_id.clone(),
            thread_id: thread_id.clone(),
            phase,
        };
        projection
            .run_records
            .insert(run_id.clone(), record.clone());
        projection.latest_by_thread.insert(thread_id, record);
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

/// The merged read repository (ADR-0039 D1), served from the fact-derived
/// projection that `hydrate` rebuilt from the durable tables (D4).
impl CheckpointReader for PostgresCommitCoordinator {
    fn run(&self, id: &RunId) -> Option<RunRecord> {
        self.get(id)
    }

    fn latest_run(&self, thread_id: &ThreadId) -> Option<RunRecord> {
        self.projection
            .lock()
            .ok()
            .and_then(|p| p.latest_by_thread.get(thread_id).cloned())
    }

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
        let record = RunRecord {
            id: RunId(run_id.clone()),
            thread_id: ThreadId(thread_id.clone()),
            phase,
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
    .fetch_all(pool)
    .await?;
    for row in event_rows {
        let sequence: i64 = row.try_get("sequence")?;
        let run_id: String = row.try_get("run_id")?;
        let Json(kind) =
            row.try_get::<Json<awaken_agent_contract::audit::kind::Kind>, _>("kind")?;
        let Json(payload) = row.try_get::<Json<serde_json::Value>, _>("payload")?;
        projection.events.push(EventRecord {
            sequence: sequence as u64,
            run_id: RunId(run_id),
            kind,
            payload,
        });
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
