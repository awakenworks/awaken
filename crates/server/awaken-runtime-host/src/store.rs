//! The per-thread commit boundary injected by Coordinator or projected by Worker.
//!
//! A session's commit coordinator is the source of committed truth. Runtime Host
//! distinguishes only an injected local authority from a database-less Worker's
//! recovery projection; backend selection and durable layout belong exclusively
//! to Coordinator.

use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator, Error, OperationCoordinator,
};
use awaken_agent_contract::thread::commit::operation::{CommitOperation, CommitReceipt};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::lifecycle::{
    RunLifecycleCursor, RunLifecycleFeed, RunLifecycleFeedError, RunLifecyclePage,
};
use awaken_agent_contract::thread::read::recovery::{
    RecoveryError, RunRecoverySnapshot, RunRecoverySource,
};
use awaken_agent_contract::thread::read::run_store::RunStore;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use std::sync::Arc;

/// One thread's commit boundary: either a `Local` read+write store (an
/// interchangeable memory/sqlite/fs/postgres backend behind `Arc<dyn HostStore>`,
/// chosen at the composition root) or the `Remote` Worker boundary backed by a
/// non-authoritative recovery projection. The four local backends are polymorphic
/// — the enum only discriminates local authoritative reads from projected remote
/// reads, not the backend.
pub(crate) enum HostCommit {
    /// One of the interchangeable local backends (memory / sqlite / fs / postgres):
    /// polymorphic implementations of the same read+write store, chosen once at the
    /// composition root behind a trait object — no per-backend dispatch here.
    Local(Arc<dyn crate::LocalCommit>),
    /// The database-independent Worker's read boundary. Authoritative writes go
    /// through the attempt's claim-fenced operation coordinator; reads use the
    /// [`awaken_run_ingress::RecoveryProjection`]. It deliberately is not a
    /// [`HostStore`]: the projection is an execution cache, never authoritative
    /// storage or an alternate commit path.
    Remote(RemoteHostCommit),
}

pub(crate) struct RemoteHostCommit {
    projection: Arc<awaken_run_ingress::RecoveryProjection>,
}

impl RemoteHostCommit {
    pub(crate) fn new() -> Self {
        Self {
            projection: Arc::new(awaken_run_ingress::RecoveryProjection::new()),
        }
    }
}

impl HostCommit {
    pub(crate) async fn authoritative_run(
        &self,
        run_id: &RunId,
    ) -> Result<Option<RunRecord>, String> {
        match self {
            HostCommit::Local(store) => store.authoritative_run(run_id).await,
            HostCommit::Remote(remote) => Ok(remote.projection.get(run_id)),
        }
    }

    pub(crate) async fn authoritative_committed_messages(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<Message>, String> {
        match self {
            HostCommit::Local(store) => store.authoritative_committed_messages(thread_id).await,
            HostCommit::Remote(remote) => Ok(remote.projection.committed_messages(thread_id)),
        }
    }

    /// Latest committed run from authoritative local storage or the remote
    /// Worker's non-authoritative recovery projection.
    pub(crate) fn latest_run(&self, thread: &ThreadId) -> Option<RunRecord> {
        match self {
            HostCommit::Local(store) => store.latest_run(thread),
            HostCommit::Remote(remote) => remote.projection.current().and_then(|snapshot| {
                snapshot
                    .latest_run_id
                    .and_then(|latest| snapshot.runs.into_iter().find(|run| run.id == latest))
            }),
        }
    }

    /// The awaiting run on `thread`, if any, recovered from committed truth. After a
    /// restart the durable variants read their hydrated projection, so a rebuilt
    /// session can restore its awaiting position and be resumed.
    pub(crate) async fn open_wait_for_thread(
        &self,
        thread: &ThreadId,
    ) -> Result<Option<(RunId, ResumeTicket)>, String> {
        match self {
            HostCommit::Local(store) => store.open_wait_for_thread(thread).await,
            HostCommit::Remote(remote) => Ok(remote.projection.current().and_then(|snapshot| {
                snapshot
                    .resume_tickets
                    .into_iter()
                    .find(|entry| entry.ticket.thread_id == *thread)
                    .map(|entry| (entry.run_id, entry.ticket))
            })),
        }
    }

    pub(crate) fn recovery_projection(
        &self,
    ) -> Option<Arc<awaken_run_ingress::RecoveryProjection>> {
        match self {
            HostCommit::Local(_) => None,
            HostCommit::Remote(remote) => Some(remote.projection.clone()),
        }
    }
}

#[async_trait::async_trait]
impl RunLifecycleFeed for HostCommit {
    async fn events_after(
        &self,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunLifecycleFeedError> {
        match self {
            HostCommit::Local(store) => store.events_after(cursor, limit).await,
            HostCommit::Remote(_) => Err(RunLifecycleFeedError::Rejected(
                "remote Worker recovery projection is not committed lifecycle authority".into(),
            )),
        }
    }
}

#[async_trait::async_trait]
impl Coordinator for HostCommit {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, Error> {
        match self {
            HostCommit::Local(store) => store.commit(commit).await,
            HostCommit::Remote(_) => Err(Error::Rejected(
                "a remote Worker must commit through its claim-fenced operation coordinator"
                    .to_string(),
            )),
        }
    }
}

#[async_trait::async_trait]
impl OperationCoordinator for HostCommit {
    async fn commit_operation(&self, operation: CommitOperation) -> Result<CommitReceipt, Error> {
        match self {
            HostCommit::Local(store) => store.commit_operation(operation).await,
            HostCommit::Remote(_) => Err(Error::Rejected(
                "a remote Worker cannot coordinate authoritative commit operations".to_string(),
            )),
        }
    }
}

impl ThreadReader for HostCommit {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        match self {
            HostCommit::Local(store) => store.committed_messages(thread_id),
            HostCommit::Remote(remote) => remote.projection.committed_messages(thread_id),
        }
    }

    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket> {
        match self {
            HostCommit::Local(store) => store.resume_ticket(run_id),
            HostCommit::Remote(remote) => remote.projection.resume_ticket(run_id),
        }
    }

    fn run_state(&self, run_id: &RunId) -> Option<RunState> {
        match self {
            HostCommit::Local(store) => store.run_state(run_id),
            HostCommit::Remote(remote) => remote.projection.run_state(run_id),
        }
    }

    fn committed_state(
        &self,
        thread_id: &ThreadId,
    ) -> Vec<awaken_agent_contract::agent::state::Command> {
        match self {
            HostCommit::Local(store) => store.committed_state(thread_id),
            HostCommit::Remote(remote) => remote.projection.committed_state(thread_id),
        }
    }
}

impl RunStore for HostCommit {
    fn get(&self, id: &RunId) -> Option<RunRecord> {
        match self {
            HostCommit::Local(store) => store.get(id),
            HostCommit::Remote(remote) => remote.projection.get(id),
        }
    }
}

#[async_trait::async_trait]
impl RunRecoverySource for HostCommit {
    async fn recovery_snapshot(
        &self,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        match self {
            HostCommit::Local(store) => store.recovery_snapshot(thread_id, claimed_run_id).await,
            HostCommit::Remote(remote) => remote
                .projection
                .current()
                .filter(|snapshot| {
                    &snapshot.thread_id == thread_id && &snapshot.claimed_run_id == claimed_run_id
                })
                .ok_or_else(|| {
                    RecoveryError::Rejected(
                        "remote recovery projection is not loaded for this claim".to_string(),
                    )
                }),
        }
    }
}
