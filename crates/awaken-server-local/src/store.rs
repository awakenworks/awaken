//! The per-thread commit boundary, in-memory or durable.
//!
//! A session's commit coordinator is the source of committed truth. It is either
//! an in-memory coordinator (tests, ephemeral sessions) or a SQLite-backed one
//! (durable: a parked run and its history survive a process restart). One
//! concrete wrapper type keeps this a composition-root choice: it coerces to
//! `Arc<dyn Coordinator>` / `&dyn ThreadReader` without trait upcasting, while
//! exposing the two extra reads the host projects — the parked position (to
//! recover a session after a restart) and the committed outcome rounds.

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::WaitingTicket;
use awaken_agent_contract::commit::coordinator::{Coordinator, Error};
use awaken_agent_contract::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::event::kind::Kind;
use awaken_agent_contract::store::run_store::RunStore;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_store_sqlite::SqliteCommitCoordinator;

/// One thread's commit boundary. `Memory` is ephemeral; `Sqlite` is durable.
pub(crate) enum HostCommit {
    Memory(MemoryCommitCoordinator),
    Sqlite(SqliteCommitCoordinator),
}

impl HostCommit {
    /// The parked run on `thread`, if any, recovered from committed truth. After a
    /// restart the `Sqlite` variant reads its hydrated projection, so a rebuilt
    /// session can restore its parked position and be resumed.
    pub(crate) fn open_wait_for_thread(&self, thread: &ThreadId) -> Option<(RunId, WaitingTicket)> {
        match self {
            HostCommit::Memory(inner) => {
                let run = inner.committed().latest_run?;
                let ticket = inner.waiting_for(&run.id)?;
                (&ticket.thread_id == thread).then_some((run.id, ticket))
            }
            HostCommit::Sqlite(inner) => inner.open_wait_for_thread(thread),
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
        }
    }
}

#[async_trait::async_trait]
impl Coordinator for HostCommit {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, Error> {
        match self {
            HostCommit::Memory(inner) => inner.commit(commit).await,
            HostCommit::Sqlite(inner) => inner.commit(commit).await,
        }
    }
}

impl ThreadReader for HostCommit {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        match self {
            HostCommit::Memory(inner) => inner.committed_messages(thread_id),
            HostCommit::Sqlite(inner) => inner.committed_messages(thread_id),
        }
    }

    fn waiting_ticket(&self, run_id: &RunId) -> Option<WaitingTicket> {
        match self {
            HostCommit::Memory(inner) => inner.waiting_ticket(run_id),
            HostCommit::Sqlite(inner) => inner.waiting_ticket(run_id),
        }
    }
}

impl RunStore for HostCommit {
    fn get(&self, id: &RunId) -> Option<RunRecord> {
        match self {
            HostCommit::Memory(inner) => inner.get(id),
            HostCommit::Sqlite(inner) => inner.get(id),
        }
    }
}
