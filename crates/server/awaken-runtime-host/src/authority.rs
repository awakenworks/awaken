//! Runtime-facing durable authority contracts.
//!
//! The Runtime Host owns execution semantics, but it does not select or open a
//! database. A Coordinator composition injects one implementation of these
//! contracts. Database-less Workers leave the authority absent and use their
//! remote, claim-fenced transports instead.

use std::sync::Arc;

use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_agent_contract::thread::commit::coordinator::{Coordinator, OperationCoordinator};
use awaken_agent_contract::thread::read::checkpoint::CheckpointReader;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::lifecycle::{
    RunLifecycleCursor, RunLifecycleFeedError, RunLifecyclePage, checkpoint_lifecycle_events_after,
};
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;

/// One local authoritative commit boundary selected by the Coordinator.
///
/// This composes the canonical commit, committed-view, recovery, and lifecycle
/// ports rather than mirroring their methods in a second facade. It adds only
/// queries whose authoritative implementations may require asynchronous shared
/// storage reads.
#[async_trait::async_trait]
pub trait LocalCommit:
    Coordinator
    + OperationCoordinator
    + CommittedThreadView
    + RunRecoverySource
    + awaken_agent_contract::thread::read::lifecycle::RunLifecycleFeed
    + Send
    + Sync
{
    async fn authoritative_run(&self, run_id: &RunId) -> Result<Option<RunRecord>, String>;

    async fn authoritative_committed_messages(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<Message>, String>;

    async fn open_wait_for_thread(
        &self,
        thread: &ThreadId,
    ) -> Result<Option<(RunId, ResumeTicket)>, String>;
}

/// The only policy seam added by [`LocalCommitAdapter`]. It selects whether
/// asynchronous authoritative reads can use the adapter's committed projection
/// or must query shared durable truth. It never commits or stores data.
#[async_trait::async_trait]
pub trait LocalCommitQueries<S>: Send + Sync {
    async fn authoritative_run(
        &self,
        store: &S,
        run_id: &RunId,
    ) -> Result<Option<RunRecord>, String>;

    async fn authoritative_committed_messages(
        &self,
        store: &S,
        thread_id: &ThreadId,
    ) -> Result<Vec<Message>, String>;

    async fn open_wait_for_thread(
        &self,
        store: &S,
        thread: &ThreadId,
    ) -> Result<Option<(RunId, ResumeTicket)>, String>;

    async fn events_after(
        &self,
        store: &S,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunLifecycleFeedError>;
}

/// One canonical delegation adapter for a Coordinator-owned commit source.
/// Implementations of the commit/read/recovery ports live in `store.rs`, the
/// architecture-approved adapter owner; composition roots only select queries.
pub struct LocalCommitAdapter<S, Q> {
    pub(crate) store: Arc<S>,
    pub(crate) queries: Q,
}

impl<S, Q> LocalCommitAdapter<S, Q> {
    #[must_use]
    pub fn with_queries(store: Arc<S>, queries: Q) -> Self {
        Self { store, queries }
    }
}

/// Query policy for single-process stores whose committed projection is the
/// authoritative read view.
pub struct ProjectedQueries;

impl<S> LocalCommitAdapter<S, ProjectedQueries> {
    #[must_use]
    pub fn projected(store: S) -> Self {
        Self::with_queries(Arc::new(store), ProjectedQueries)
    }
}

#[async_trait::async_trait]
impl<S> LocalCommitQueries<S> for ProjectedQueries
where
    S: CheckpointReader + Send + Sync,
{
    async fn authoritative_run(
        &self,
        store: &S,
        run_id: &RunId,
    ) -> Result<Option<RunRecord>, String> {
        Ok(store.run(run_id))
    }

    async fn authoritative_committed_messages(
        &self,
        store: &S,
        thread_id: &ThreadId,
    ) -> Result<Vec<Message>, String> {
        Ok(store.committed_messages(thread_id))
    }

    async fn open_wait_for_thread(
        &self,
        store: &S,
        thread: &ThreadId,
    ) -> Result<Option<(RunId, ResumeTicket)>, String> {
        let Some(run) = store.latest_run(thread) else {
            return Ok(None);
        };
        let Some(ticket) = store.resume_ticket(&run.id) else {
            return Ok(None);
        };
        Ok((&ticket.thread_id == thread).then_some((run.id, ticket)))
    }

    async fn events_after(
        &self,
        store: &S,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunLifecycleFeedError> {
        checkpoint_lifecycle_events_after(store, cursor, limit)
    }
}

#[async_trait::async_trait]
impl<S, Q> LocalCommit for LocalCommitAdapter<S, Q>
where
    S: Coordinator + OperationCoordinator + CommittedThreadView + RunRecoverySource + Send + Sync,
    Q: LocalCommitQueries<S>,
{
    async fn authoritative_run(&self, run_id: &RunId) -> Result<Option<RunRecord>, String> {
        self.queries
            .authoritative_run(self.store.as_ref(), run_id)
            .await
    }

    async fn authoritative_committed_messages(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<Message>, String> {
        self.queries
            .authoritative_committed_messages(self.store.as_ref(), thread_id)
            .await
    }

    async fn open_wait_for_thread(
        &self,
        thread: &ThreadId,
    ) -> Result<Option<(RunId, ResumeTicket)>, String> {
        self.queries
            .open_wait_for_thread(self.store.as_ref(), thread)
            .await
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
                Arc::new(LocalCommitAdapter::projected(
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
