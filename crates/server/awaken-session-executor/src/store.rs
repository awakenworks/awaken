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
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_store_fs::FsCommitCoordinator;
use awaken_store_sqlite::SqliteCommitCoordinator;

/// One thread's commit boundary. `Memory` is ephemeral; `Sqlite` and `Fs` are
/// durable (a parked run and its history survive a process restart).
// Exactly one `HostCommit` exists per live session (not stored in bulk), so the
// per-backend size spread is immaterial; boxing would only add an indirection.
#[allow(clippy::large_enum_variant)]
pub(crate) enum HostCommit {
    Memory(MemoryCommitCoordinator),
    Sqlite(SqliteCommitCoordinator),
    Fs(FsCommitCoordinator),
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
        }
    }
}

impl ThreadReader for HostCommit {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        match self {
            HostCommit::Memory(inner) => inner.committed_messages(thread_id),
            HostCommit::Sqlite(inner) => inner.committed_messages(thread_id),
            HostCommit::Fs(inner) => inner.committed_messages(thread_id),
        }
    }

    fn waiting_ticket(&self, run_id: &RunId) -> Option<WaitingTicket> {
        match self {
            HostCommit::Memory(inner) => inner.waiting_ticket(run_id),
            HostCommit::Sqlite(inner) => inner.waiting_ticket(run_id),
            HostCommit::Fs(inner) => inner.waiting_ticket(run_id),
        }
    }
}

impl RunStore for HostCommit {
    fn get(&self, id: &RunId) -> Option<RunRecord> {
        match self {
            HostCommit::Memory(inner) => inner.get(id),
            HostCommit::Sqlite(inner) => inner.get(id),
            HostCommit::Fs(inner) => inner.get(id),
        }
    }
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
    let Some(dir) = store_dir else {
        return false;
    };
    let fs_backend = std::env::var("AWAKEN_STORE").is_ok_and(|value| value == "fs");
    if fs_backend {
        dir.join(sanitize_thread(thread)).exists()
    } else {
        dir.join(format!("{}.db", sanitize_thread(thread))).exists()
    }
}
