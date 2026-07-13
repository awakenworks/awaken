//! The per-thread commit boundary, in-memory or durable.
//!
//! A session's commit coordinator is the source of committed truth. It is either
//! an in-memory coordinator (tests, ephemeral sessions) or a durable one — SQLite
//! or the filesystem append-log — behind which a parked run and its history
//! survive a process restart. One concrete wrapper type keeps this a
//! composition-root choice: it coerces to `Arc<dyn Coordinator>` / `&dyn
//! ThreadReader` without trait upcasting, while exposing the two extra reads the
//! host projects — the parked position (to recover a session after a restart) and
//! the committed outcome rounds.

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::WaitingTicket;
use awaken_agent_contract::commit::coordinator::{Coordinator, Error};
use awaken_agent_contract::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::event::kind::Kind;
use awaken_agent_contract::store::checkpoint::{CheckpointReader, EventScope};
use awaken_agent_contract::store::run_store::RunStore;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use std::sync::Arc;

use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_store_fs::FsCommitCoordinator;
use awaken_store_postgres::PostgresCommitCoordinator;
use awaken_store_sqlite::SqliteCommitCoordinator;

/// One thread's commit boundary. `Memory` is ephemeral; `Sqlite`, `Fs`, and
/// `Postgres` are durable (a parked run and its history survive a process restart).
/// `Postgres` is the SHARED coordinator (keyed by thread internally) so any node
/// serves any thread's history — the cloud multi-node backend (ADR-0022 D6); the
/// others are per-thread/per-process.
// Exactly one `HostCommit` exists per live session (not stored in bulk), so the
// per-backend size spread is immaterial; boxing would only add an indirection.
#[allow(clippy::large_enum_variant)]
pub(crate) enum HostCommit {
    Memory(MemoryCommitCoordinator),
    Sqlite(SqliteCommitCoordinator),
    Fs(FsCommitCoordinator),
    Postgres(Arc<PostgresCommitCoordinator>),
    /// The database-less worker's boundary: `commit` posts facts to the cell server
    /// (the single writer) over HTTP; the committed-truth READS return empty because
    /// the worker holds no store. This is correct for a **fresh, self-contained run**
    /// (nothing prior to read — the activation carries the input), which is the cell
    /// worker's role. A resume/multi-turn run that must read prior committed context
    /// needs remote reads, which the synchronous `ThreadReader`/`RunStore` traits
    /// cannot express without blocking — a separate async-reader redesign.
    Remote(crate::commit_ingest::RemoteCoordinator),
}

/// Recover the parked position from a durable backend's fact-derived read model
/// (`CheckpointReader`): the latest run on the thread plus its committed ticket.
fn parked_from_reader<R: CheckpointReader>(
    reader: &R,
    thread: &ThreadId,
) -> Option<(RunId, WaitingTicket)> {
    let run = reader.latest_run(thread)?;
    let ticket = reader.waiting_ticket(&run.id)?;
    (&ticket.thread_id == thread).then_some((run.id, ticket))
}

impl HostCommit {
    /// The parked run on `thread`, if any, recovered from committed truth. After a
    /// restart the durable variants read their hydrated projection, so a rebuilt
    /// session can restore its parked position and be resumed.
    pub(crate) fn open_wait_for_thread(&self, thread: &ThreadId) -> Option<(RunId, WaitingTicket)> {
        match self {
            HostCommit::Memory(inner) => {
                let run = inner.committed().latest_run?;
                let ticket = inner.waiting_for(&run.id)?;
                (&ticket.thread_id == thread).then_some((run.id, ticket))
            }
            HostCommit::Sqlite(inner) => inner.open_wait_for_thread(thread),
            HostCommit::Fs(inner) => parked_from_reader(inner, thread),
            HostCommit::Postgres(inner) => parked_from_reader(inner.as_ref(), thread),
            HostCommit::Remote(_) => None,
        }
    }

    /// Payloads of committed `Continuation` (outcome-round) events for `thread`, in
    /// commit order — projected from durable truth, so the round history survives a
    /// restart.
    pub(crate) fn continuation_payloads(&self, thread: &ThreadId) -> Vec<serde_json::Value> {
        match self {
            HostCommit::Memory(inner) => inner
                .committed()
                .events
                .into_iter()
                .filter(|event| event.kind == Kind::Continuation)
                .map(|event| event.payload)
                .collect(),
            HostCommit::Sqlite(inner) => inner.continuation_payloads(thread),
            HostCommit::Fs(inner) => inner
                .list_events(&EventScope::Thread(thread.clone()), None, usize::MAX)
                .into_iter()
                .filter(|event| event.kind == Kind::Continuation)
                .map(|event| event.payload)
                .collect(),
            HostCommit::Postgres(inner) => inner
                .list_events(&EventScope::Thread(thread.clone()), None, usize::MAX)
                .into_iter()
                .filter(|event| event.kind == Kind::Continuation)
                .map(|event| event.payload)
                .collect(),
            HostCommit::Remote(_) => Vec::new(),
        }
    }
}

#[async_trait::async_trait]
impl Coordinator for HostCommit {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, Error> {
        match self {
            HostCommit::Memory(inner) => inner.commit(commit).await,
            HostCommit::Sqlite(inner) => inner.commit(commit).await,
            HostCommit::Fs(inner) => inner.commit(commit).await,
            HostCommit::Postgres(inner) => inner.commit(commit).await,
            HostCommit::Remote(inner) => inner.commit(commit).await,
        }
    }
}

impl ThreadReader for HostCommit {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        match self {
            HostCommit::Memory(inner) => inner.committed_messages(thread_id),
            HostCommit::Sqlite(inner) => inner.committed_messages(thread_id),
            HostCommit::Fs(inner) => inner.committed_messages(thread_id),
            HostCommit::Postgres(inner) => inner.committed_messages(thread_id),
            HostCommit::Remote(_) => Vec::new(),
        }
    }

    fn waiting_ticket(&self, run_id: &RunId) -> Option<WaitingTicket> {
        match self {
            HostCommit::Memory(inner) => inner.waiting_ticket(run_id),
            HostCommit::Sqlite(inner) => inner.waiting_ticket(run_id),
            HostCommit::Fs(inner) => inner.waiting_ticket(run_id),
            HostCommit::Postgres(inner) => inner.waiting_ticket(run_id),
            HostCommit::Remote(_) => None,
        }
    }

    fn committed_state(
        &self,
        thread_id: &ThreadId,
    ) -> Vec<awaken_agent_contract::agent::state::Command> {
        match self {
            HostCommit::Memory(inner) => inner.committed_state(thread_id),
            HostCommit::Sqlite(inner) => inner.committed_state(thread_id),
            HostCommit::Fs(inner) => inner.committed_state(thread_id),
            HostCommit::Postgres(inner) => inner.committed_state(thread_id),
            HostCommit::Remote(_) => Vec::new(),
        }
    }
}

impl RunStore for HostCommit {
    fn get(&self, id: &RunId) -> Option<RunRecord> {
        match self {
            HostCommit::Memory(inner) => inner.get(id),
            HostCommit::Sqlite(inner) => inner.get(id),
            HostCommit::Fs(inner) => inner.get(id),
            HostCommit::Postgres(inner) => inner.get(id),
            HostCommit::Remote(_) => None,
        }
    }
}

/// Select the shared Postgres commit backend (ADR-0022 D6), or fail closed when it
/// was not initialised at startup. Extracted from `build_commit` so both the
/// initialised and the misconfigured paths are unit-testable without a `SharedHost`.
pub(crate) fn postgres_commit_or_err() -> Result<HostCommit, crate::host::HostError> {
    commit_or_err(crate::commit_backend::shared_postgres_commit())
}

/// Wrap the (maybe-initialised) shared coordinator into a `HostCommit`, or fail
/// closed. Takes the coordinator as a parameter so both the initialised (`Some`) and
/// the misconfigured (`None`) branches are testable without the process-global.
fn commit_or_err(
    coord: Option<Arc<PostgresCommitCoordinator>>,
) -> Result<HostCommit, crate::host::HostError> {
    let coord = coord.ok_or_else(|| {
        crate::host::HostError::internal(
            "AWAKEN_STORE=postgres requires init_shared_postgres_commit() at process \
             startup (with AWAKEN_DATABASE_URL)",
        )
    })?;
    Ok(HostCommit::Postgres(coord))
}

/// Whether the shared Postgres commit coordinator holds a committed run for `thread`.
/// Extracted so the postgres branch of `durable_thread_exists` is unit-testable
/// without mutating the process `AWAKEN_STORE` env.
pub(crate) fn durable_thread_exists_postgres(thread: &ThreadId) -> bool {
    crate::commit_backend::shared_postgres_commit()
        .and_then(|c| c.latest_run(thread))
        .is_some()
}

/// A filesystem-safe database filename stem for a thread id (durable store).
pub(crate) fn sanitize_thread(thread: &str) -> String {
    thread
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// True when the durable store under `store_dir` already holds `thread`,
/// WITHOUT opening it: mirrors the commit-boundary layout exactly (fs backend
/// keys a per-thread directory, default SQLite a per-thread db file; no store
/// dir = nothing durable). Session-id minting probes through this so a
/// candidate id is never materialized as a side effect (a prematurely built
/// session context would lack the session's agent config and MCP tools).
pub(crate) fn durable_thread_exists(store_dir: Option<&std::path::Path>, thread: &str) -> bool {
    use crate::deployment_config::{DeploymentConfig, StoreKind};
    let store = DeploymentConfig::from_env().store;
    // Shared Postgres backend: the coordinator is keyed by thread, so a committed
    // run for the thread means it durably exists (no per-thread file to stat).
    if store == StoreKind::Postgres {
        return durable_thread_exists_postgres(&ThreadId(thread.to_string()));
    }
    let Some(dir) = store_dir else {
        return false;
    };
    let fs_backend = store == StoreKind::Fs;
    if fs_backend {
        dir.join(sanitize_thread(thread)).exists()
    } else {
        dir.join(format!("{}.db", sanitize_thread(thread))).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_or_err_fails_closed_without_the_shared_coordinator() {
        // AWAKEN_STORE=postgres but init_shared_postgres_commit was never called →
        // build_commit fails closed rather than silently using an ephemeral store.
        assert!(commit_or_err(None).is_err());
    }

    #[test]
    fn durable_thread_exists_routes_to_postgres_when_selected() {
        // AWAKEN_STORE=postgres routes the probe to the shared coordinator. With no
        // coordinator initialised here it reads false, but this exercises the backend
        // dispatch (the postgres branch of durable_thread_exists). Set-and-restore is
        // safe: only this crate's durable_thread_exists reads AWAKEN_STORE in a unit
        // test, and it is called nowhere else here.
        // SAFETY: single-threaded within this test's critical section; restored below.
        unsafe { std::env::set_var("AWAKEN_STORE", "postgres") };
        let exists = durable_thread_exists(None, "no-such-thread-xyz");
        unsafe { std::env::remove_var("AWAKEN_STORE") };
        assert!(!exists, "an unknown thread has no committed run");
    }

    #[tokio::test]
    async fn postgres_commit_and_thread_probe_after_init() {
        let Ok(url) = std::env::var("AWAKEN_TEST_PG_URL") else {
            eprintln!("skip: AWAKEN_TEST_PG_URL unset");
            return;
        };
        crate::commit_backend::init_shared_postgres_commit(&url)
            .await
            .expect("init");
        // Selecting the postgres backend now yields a HostCommit::Postgres.
        assert!(matches!(
            postgres_commit_or_err().expect("ok after init"),
            HostCommit::Postgres(_)
        ));
        // The thread probe: an unknown thread has no committed run in the shared DB.
        assert!(!durable_thread_exists_postgres(&ThreadId(
            "no-such-thread-xyz".to_string()
        )));
    }
}
