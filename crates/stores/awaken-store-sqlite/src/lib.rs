//! SQLite durable implementation of the runtime commit boundary.
//!
//! [`SqliteCommitCoordinator`] is the embedded sibling of the Postgres backend:
//! it implements the same neutral [`Coordinator`] write boundary (G1/G13) and the
//! `RunStore` / `ThreadReader` read ports against an in-process SQLite database,
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
use awaken_agent_contract::audit::record::Record as EventRecord;
use awaken_agent_contract::thread::commit::coordinator::{Coordinator as CommitCoordinator, Error};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::checkpoint::{CheckpointReader, EventScope};
use awaken_agent_contract::thread::read::run_store::RunStore;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use rusqlite::{Connection, TransactionBehavior, params};

pub use awaken_store_schema::{COMMIT_BUNDLE_ID as BUNDLE_ID, commit_bundle};

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
        let conn = Connection::open(path).map_err(|err| StoreError::Open(err.to_string()))?;
        Self::from_connection(conn)
    }

    /// Open a private in-memory database (a fresh, isolated schema per call).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(|err| StoreError::Open(err.to_string()))?;
        Self::from_connection(conn)
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

    /// The awaiting run on `thread`, if any, with its committed ticket. Read from
    /// hydrated durable truth so a rebuilt session can recover its awaiting position
    /// after a restart (G1/G13).
    pub fn open_wait_for_thread(&self, thread: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        self.projection.lock().ok().and_then(|p| {
            p.resume_tickets
                .iter()
                .find(|(_, ticket)| &ticket.thread_id == thread)
                .map(|(run_id, ticket)| (run_id.clone(), ticket.clone()))
        })
    }

    /// Payloads of committed `Continuation` events for `thread`, in commit order.
    /// Read straight from the durable event log (the projection does not
    /// materialize events), so the outcome-round history survives a restart.
    pub fn continuation_payloads(&self, thread: &ThreadId) -> Vec<serde_json::Value> {
        let kind =
            match serde_json::to_string(&awaken_agent_contract::audit::kind::Kind::Continuation) {
                Ok(kind) => kind,
                Err(_) => return Vec::new(),
            };
        let conn = match self.conn.lock() {
            Ok(conn) => conn,
            Err(_) => return Vec::new(),
        };
        let mut stmt = match conn.prepare(&format!(
            "SELECT e.payload FROM {NS}_event e \
             JOIN {NS}_run_record r ON e.run_id = r.run_id \
             WHERE r.thread_id = ?1 AND e.kind = ?2 ORDER BY e.sequence"
        )) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map(params![thread.0.as_str(), kind], |row| {
            row.get::<_, String>(0)
        }) {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };
        rows.filter_map(|row| row.ok())
            .filter_map(|payload| serde_json::from_str::<serde_json::Value>(&payload).ok())
            .collect()
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

        // Terminal-is-final (exactly-once committed LOG under a stale reclaim):
        // reject a post-terminal commit for a run whose committed state is already
        // `Ended`. A stale owner whose lease lapsed mid-flight and was superseded by
        // a reclaimer that already drove the run to `Ended` would otherwise append a
        // duplicate transcript and a second terminal fact. The first `Ended` commit
        // lands (the run is not yet terminal); only a SUBSEQUENT commit is fenced.
        // The state is read from the same fact-derived projection that serves
        // `run`/`latest_run`. (See the in-memory reference for the full rationale.)
        //
        // This read is process-LOCAL (the in-memory projection), which is correct
        // here because the SQLite store is single-writer / single-process by
        // construction (the write lock serializes all commits in one process; the
        // fleet uses Postgres, whose guard is instead a durable in-transaction
        // `SELECT ... FOR UPDATE` on the shared DB). Do not assume this SQLite fence
        // holds across processes sharing a database file.
        {
            let projection = lock(&self.projection)?;
            if projection
                .run_records
                .get(commit.run_id())
                .is_some_and(|record| !record.state.permits(&commit.run_state()))
            {
                return Err(Error::Rejected(format!(
                    "run {} is already terminal; refusing post-terminal commit",
                    commit.run_id().0
                )));
            }
        }

        let next = lock(&self.projection)?.sequence + 1;

        let conn = self.conn.clone();
        let data = commit.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn
                .lock()
                .map_err(|_| Error::Rejected("sqlite connection poisoned".to_string()))?;
            write_commit(&mut guard, next, &data)
        })
        .await
        .map_err(|err| Error::Rejected(err.to_string()))??;

        // The transaction is durable; advance the in-memory projection to match.
        let run_state = commit.run_state();
        let run_id = commit.run_id().clone();
        let thread_id = commit.thread_id.clone();
        let resume_ticket = commit.resume_ticket().cloned();
        let awaiting = matches!((&resume_ticket, &run_state), (Some(_), RunState::Awaiting));

        let mut projection = lock(&self.projection)?;
        projection.sequence = next;
        for message in commit.messages {
            projection.messages.push((thread_id.clone(), message));
        }
        for command in commit.state {
            projection.state.push((thread_id.clone(), command));
        }
        for (offset, draft) in commit.events.into_iter().enumerate() {
            projection.events.push(EventRecord {
                sequence: next * 1_000 + offset as u64,
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

        Ok(CommitRecord { sequence: next })
    }
}

impl RunStore for SqliteCommitCoordinator {
    fn get(&self, id: &RunId) -> Option<RunRecord> {
        self.projection
            .lock()
            .ok()
            .and_then(|p| p.run_records.get(id).cloned())
    }
}

impl ThreadReader for SqliteCommitCoordinator {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        SqliteCommitCoordinator::committed_messages(self, thread_id)
    }

    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket> {
        self.resume_ticket_for(run_id)
    }

    fn committed_state(&self, thread_id: &ThreadId) -> Vec<StateCommand> {
        SqliteCommitCoordinator::committed_state(self, thread_id)
    }
}

/// The merged read repository (ADR-0039 D1), served from the fact-derived
/// projection that `hydrate` rebuilt from the durable tables (D4).
impl CheckpointReader for SqliteCommitCoordinator {
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

/// Write one staged commit in a single IMMEDIATE transaction (atomic, G1/G13).
/// JSON columns are stored as serialized text — the schema renders `{json}` to
/// TEXT on SQLite.
fn write_commit(conn: &mut Connection, next: u64, commit: &ThreadCommit) -> Result<(), Error> {
    let p = NS;
    let run_id = &commit.run_id().0;
    let thread_id = &commit.thread_id.0;
    let run_state = commit.run_state();
    let state_json = json(&run_state)?;

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(reject)?;

    tx.execute(
        &format!(
            "INSERT INTO {p}_commit (sequence, thread_id, run_id, phase) VALUES (?1,?2,?3,?4)"
        ),
        params![next as i64, thread_id, run_id, state_json],
    )
    .map_err(reject)?;

    for message in &commit.messages {
        tx.execute(
            &format!(
                "INSERT INTO {p}_message (commit_sequence, thread_id, data) VALUES (?1,?2,?3)"
            ),
            params![next as i64, thread_id, json(message)?],
        )
        .map_err(reject)?;
    }

    for command in &commit.state {
        tx.execute(
            &format!(
                "INSERT INTO {p}_state_command (commit_sequence, thread_id, data) VALUES (?1,?2,?3)"
            ),
            params![next as i64, thread_id, json(command)?],
        )
        .map_err(reject)?;
    }

    for (offset, draft) in commit.events.iter().enumerate() {
        let sequence = next * 1_000 + offset as u64;
        tx.execute(
            &format!(
                "INSERT INTO {p}_event (sequence, run_id, kind, payload) VALUES (?1,?2,?3,?4)"
            ),
            params![
                sequence as i64,
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

    tx.commit().map_err(reject)?;
    Ok(())
}

/// Rebuild the read projection from the committed log in SQLite.
fn hydrate(conn: &Connection) -> Result<Projection, rusqlite::Error> {
    let sequence = conn.query_row(
        &format!("SELECT COALESCE(MAX(sequence), 0) FROM {NS}_commit"),
        [],
        |row| row.get::<_, i64>(0),
    )? as u64;
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
                sequence: sequence as u64,
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
        if let Ok(ticket) = serde_json::from_str::<ResumeTicket>(&ticket) {
            projection.resume_tickets.insert(RunId(run_id), ticket);
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
