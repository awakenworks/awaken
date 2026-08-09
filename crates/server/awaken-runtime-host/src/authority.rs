//! Runtime-facing durable authority contracts.
//!
//! The Runtime Host owns execution semantics, but it does not select or open a
//! database. A Coordinator composition injects one implementation of these
//! contracts. Database-less Workers leave the authority absent and use their
//! remote, claim-fenced transports instead.

use std::sync::Arc;

use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::state::Command;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_agent_contract::thread::commit::coordinator::Error;
use awaken_agent_contract::thread::commit::coordinator::{Coordinator, OperationCoordinator};
use awaken_agent_contract::thread::commit::operation::{CommitOperation, CommitReceipt};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::checkpoint::CheckpointReader;
use awaken_agent_contract::thread::read::lifecycle::{
    RunLifecycleCursor, RunLifecycleFeedError, RunLifecyclePage, checkpoint_lifecycle_events_after,
};
use awaken_agent_contract::thread::read::recovery::{
    RecoveryError, RunRecoverySnapshot, RunRecoverySource,
};
use awaken_agent_contract::thread::read::run_store::RunStore;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;

/// One local authoritative commit boundary selected by the Coordinator.
///
/// This deliberately mirrors only the operations the Runtime Host consumes. It
/// avoids exposing a concrete SQLite/FS/Postgres type or a backend-selection
/// enum across the bounded-context boundary.
#[async_trait::async_trait]
pub trait LocalCommit: Send + Sync {
    async fn authoritative_run(&self, run_id: &RunId) -> Result<Option<RunRecord>, String>;

    async fn authoritative_committed_messages(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<Message>, String>;

    fn latest_run(&self, thread: &ThreadId) -> Option<RunRecord>;

    async fn open_wait_for_thread(
        &self,
        thread: &ThreadId,
    ) -> Result<Option<(RunId, ResumeTicket)>, String>;

    async fn events_after(
        &self,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunLifecycleFeedError>;

    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, Error>;

    async fn commit_operation(&self, operation: CommitOperation) -> Result<CommitReceipt, Error>;

    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message>;
    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket>;
    fn run_state(&self, run_id: &RunId) -> Option<RunState>;
    fn committed_state(&self, thread_id: &ThreadId) -> Vec<Command>;
    fn get(&self, id: &RunId) -> Option<RunRecord>;

    async fn recovery_snapshot(
        &self,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError>;
}

/// Adapter for single-process commit implementations whose hydrated projection
/// is authoritative. Durable multi-replica backends implement [`LocalCommit`]
/// directly so their reads can query shared truth.
pub struct ProjectedLocalCommit<T>(pub T);

#[async_trait::async_trait]
impl<T> LocalCommit for ProjectedLocalCommit<T>
where
    T: Coordinator
        + OperationCoordinator
        + CheckpointReader
        + RunStore
        + RunRecoverySource
        + Send
        + Sync,
{
    async fn authoritative_run(&self, run_id: &RunId) -> Result<Option<RunRecord>, String> {
        Ok(RunStore::get(&self.0, run_id))
    }

    async fn authoritative_committed_messages(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<Message>, String> {
        Ok(ThreadReader::committed_messages(&self.0, thread_id))
    }

    fn latest_run(&self, thread: &ThreadId) -> Option<RunRecord> {
        CheckpointReader::latest_run(&self.0, thread)
    }

    async fn open_wait_for_thread(
        &self,
        thread: &ThreadId,
    ) -> Result<Option<(RunId, ResumeTicket)>, String> {
        let Some(run) = CheckpointReader::latest_run(&self.0, thread) else {
            return Ok(None);
        };
        let Some(ticket) = ThreadReader::resume_ticket(&self.0, &run.id) else {
            return Ok(None);
        };
        Ok((&ticket.thread_id == thread).then_some((run.id, ticket)))
    }

    async fn events_after(
        &self,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunLifecycleFeedError> {
        checkpoint_lifecycle_events_after(&self.0, cursor, limit)
    }

    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, Error> {
        Coordinator::commit(&self.0, commit).await
    }

    async fn commit_operation(&self, operation: CommitOperation) -> Result<CommitReceipt, Error> {
        OperationCoordinator::commit_operation(&self.0, operation).await
    }

    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        ThreadReader::committed_messages(&self.0, thread_id)
    }

    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket> {
        ThreadReader::resume_ticket(&self.0, run_id)
    }

    fn run_state(&self, run_id: &RunId) -> Option<RunState> {
        ThreadReader::run_state(&self.0, run_id)
    }

    fn committed_state(&self, thread_id: &ThreadId) -> Vec<Command> {
        ThreadReader::committed_state(&self.0, thread_id)
    }

    fn get(&self, id: &RunId) -> Option<RunRecord> {
        RunStore::get(&self.0, id)
    }

    async fn recovery_snapshot(
        &self,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        RunRecoverySource::recovery_snapshot(&self.0, thread_id, claimed_run_id).await
    }
}

/// The complete durable capability injected into one Coordinator Runtime Host.
#[async_trait::async_trait]
pub trait RuntimeAuthority: Send + Sync {
    async fn open_commit(
        &self,
        thread: &str,
    ) -> Result<Arc<dyn LocalCommit>, RuntimeAuthorityError>;

    async fn durable_thread_exists(&self, thread: &str) -> Result<bool, RuntimeAuthorityError>;

    fn dispatch_store(&self) -> Arc<awaken_run_ingress::AnyDispatchStore>;

    fn dispatch_wake(&self) -> Option<Arc<dyn awaken_run_ingress::WakeSignal>>;

    fn stream_checkpoint(
        &self,
        thread: &str,
    ) -> Result<Arc<dyn StreamCheckpointStore>, RuntimeAuthorityError>;
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeAuthorityError {
    #[error("runtime authority unavailable: {0}")]
    Unavailable(String),
    #[error("runtime authority is corrupt: {0}")]
    Corrupt(String),
    #[error("runtime authority is misconfigured: {0}")]
    Misconfigured(String),
}

impl RuntimeAuthorityError {
    pub fn unavailable(error: impl Into<String>) -> Self {
        Self::Unavailable(error.into())
    }

    pub fn misconfigured(error: impl Into<String>) -> Self {
        Self::Misconfigured(error.into())
    }
}

/// Process-local authority used only by unit tests and scenario fixtures.
///
/// This is the sole non-durable reference adapter for the runtime boundary. It
/// keeps product construction fail-closed while allowing tests to exercise the
/// same injected contract without opening SQL or filesystem stores here.
#[cfg(any(test, feature = "test-support"))]
pub struct EphemeralRuntimeAuthority {
    commits: std::sync::Mutex<std::collections::HashMap<String, Arc<dyn LocalCommit>>>,
    checkpoints:
        std::sync::Mutex<std::collections::HashMap<String, Arc<dyn StreamCheckpointStore>>>,
    dispatch: Arc<awaken_run_ingress::AnyDispatchStore>,
}

#[cfg(any(test, feature = "test-support"))]
impl Default for EphemeralRuntimeAuthority {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl EphemeralRuntimeAuthority {
    #[must_use]
    pub fn new() -> Self {
        let dispatch = awaken_run_ingress::AnyDispatchStore::from_dispatch(Arc::new(
            awaken_run_ingress::MemoryDispatchStore::new(),
        ));
        Self {
            commits: std::sync::Mutex::new(std::collections::HashMap::new()),
            checkpoints: std::sync::Mutex::new(std::collections::HashMap::new()),
            dispatch: Arc::new(dispatch),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait::async_trait]
impl RuntimeAuthority for EphemeralRuntimeAuthority {
    async fn open_commit(
        &self,
        thread: &str,
    ) -> Result<Arc<dyn LocalCommit>, RuntimeAuthorityError> {
        let mut commits = self.commits.lock().map_err(|_| {
            RuntimeAuthorityError::Corrupt("ephemeral commit authority lock poisoned".to_owned())
        })?;
        Ok(commits
            .entry(thread.to_owned())
            .or_insert_with(|| {
                Arc::new(ProjectedLocalCommit(
                    awaken_store_inmem::MemoryCommitCoordinator::new(),
                )) as Arc<dyn LocalCommit>
            })
            .clone())
    }

    async fn durable_thread_exists(&self, thread: &str) -> Result<bool, RuntimeAuthorityError> {
        self.commits
            .lock()
            .map(|commits| commits.contains_key(thread))
            .map_err(|_| {
                RuntimeAuthorityError::Corrupt(
                    "ephemeral commit authority lock poisoned".to_owned(),
                )
            })
    }

    fn dispatch_store(&self) -> Arc<awaken_run_ingress::AnyDispatchStore> {
        self.dispatch.clone()
    }

    fn dispatch_wake(&self) -> Option<Arc<dyn awaken_run_ingress::WakeSignal>> {
        None
    }

    fn stream_checkpoint(
        &self,
        thread: &str,
    ) -> Result<Arc<dyn StreamCheckpointStore>, RuntimeAuthorityError> {
        let mut checkpoints = self.checkpoints.lock().map_err(|_| {
            RuntimeAuthorityError::Corrupt(
                "ephemeral checkpoint authority lock poisoned".to_owned(),
            )
        })?;
        Ok(checkpoints
            .entry(thread.to_owned())
            .or_insert_with(|| {
                Arc::new(awaken_store_inmem::MemoryStreamCheckpointStore::new())
                    as Arc<dyn StreamCheckpointStore>
            })
            .clone())
    }
}

/// Coordinator-side factory for one exact credential revision's challenge
/// recovery. Runtime carries only the resulting MCP transport refresher and
/// never sees a Credential repository or Secret Store.
pub trait CredentialRefreshFactory: Send + Sync {
    fn refresher(
        &self,
        credential_id: awaken_credential_contract::CredentialSourceId,
        access: awaken_runtime_contract::CredentialRefreshAccess,
    ) -> Arc<dyn awaken_ext_mcp::CredentialRefresher>;

    fn bearer_reloader(
        &self,
        credential_id: awaken_credential_contract::CredentialSourceId,
        credential_revision: u64,
    ) -> Arc<dyn awaken_ext_mcp::CredentialRefresher>;
}
