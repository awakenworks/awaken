// @formal entrypoint background_task.terminal_observer_reads_authoritative_committed_state_not_process_projection
//! Runtime-facing durable authority contracts.
//!
//! The Runtime Host owns execution semantics, but it does not select or open a
//! database. A Coordinator startup injects one implementation of these
//! contracts. Database-less Workers leave the authority absent and use their
//! remote, claim-fenced transports instead.

use std::sync::Arc;

use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord};
use awaken_agent_contract::agent::state::Command as StateCommand;
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
/// This configures the canonical commit, committed-view, recovery, and lifecycle
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

    /// Read the latest Run for one logical Thread from the authority selected by
    /// this adapter. Shared stores must not answer from a process-local
    /// compatibility projection.
    async fn authoritative_latest_run(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Option<RunRecord>, String>;

    async fn authoritative_committed_messages(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<Message>, String>;

    /// Read the complete committed state from the selected authority. Shared
    /// stores must bypass a process-local projection because peer commits can
    /// advance usage, tool, and lifecycle cells after this process starts.
    async fn authoritative_committed_state(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<StateCommand>, String>;

    async fn authoritative_open_wait_for_thread(
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

    async fn authoritative_latest_run(
        &self,
        store: &S,
        thread_id: &ThreadId,
    ) -> Result<Option<RunRecord>, String>;

    async fn authoritative_committed_messages(
        &self,
        store: &S,
        thread_id: &ThreadId,
    ) -> Result<Vec<Message>, String>;

    async fn authoritative_committed_state(
        &self,
        store: &S,
        thread_id: &ThreadId,
    ) -> Result<Vec<StateCommand>, String>;

    async fn authoritative_open_wait_for_thread(
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
/// architecture-approved adapter owner; process startups only select queries.
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
    S: CheckpointReader + CommittedThreadView + Send + Sync,
{
    async fn authoritative_run(
        &self,
        store: &S,
        run_id: &RunId,
    ) -> Result<Option<RunRecord>, String> {
        Ok(store.run(run_id))
    }

    async fn authoritative_latest_run(
        &self,
        store: &S,
        thread_id: &ThreadId,
    ) -> Result<Option<RunRecord>, String> {
        Ok(store.latest_run(thread_id))
    }

    async fn authoritative_committed_messages(
        &self,
        store: &S,
        thread_id: &ThreadId,
    ) -> Result<Vec<Message>, String> {
        Ok(store.committed_messages(thread_id))
    }

    async fn authoritative_committed_state(
        &self,
        store: &S,
        thread_id: &ThreadId,
    ) -> Result<Vec<StateCommand>, String> {
        Ok(store.committed_state(thread_id))
    }

    async fn authoritative_open_wait_for_thread(
        &self,
        store: &S,
        thread: &ThreadId,
    ) -> Result<Option<(RunId, ResumeTicket)>, String> {
        Ok(CommittedThreadView::open_wait_for_thread(store, thread))
    }

    async fn events_after(
        &self,
        store: &S,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunLifecycleFeedError> {
        checkpoint_lifecycle_events_after(store, cursor, limit).await
    }
}

/// Test query policy for making committed State diverge from the underlying
/// compatibility projection without constructing another store or observer.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct ScriptedStateQueries {
    state: Arc<std::sync::Mutex<Result<Vec<StateCommand>, String>>>,
    latest_run_reads: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
impl ScriptedStateQueries {
    pub(crate) fn new(state: Result<Vec<StateCommand>, String>) -> Self {
        Self {
            state: Arc::new(std::sync::Mutex::new(state)),
            latest_run_reads: Default::default(),
        }
    }

    pub(crate) fn replace(&self, state: Result<Vec<StateCommand>, String>) {
        *self
            .state
            .lock()
            .expect("scripted authoritative state mutex poisoned") = state;
    }

    pub(crate) fn latest_run_reads(&self) -> usize {
        self.latest_run_reads
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl LocalCommitQueries<awaken_store_inmem::MemoryCommitCoordinator> for ScriptedStateQueries {
    async fn authoritative_run(
        &self,
        store: &awaken_store_inmem::MemoryCommitCoordinator,
        run_id: &RunId,
    ) -> Result<Option<RunRecord>, String> {
        ProjectedQueries.authoritative_run(store, run_id).await
    }

    async fn authoritative_latest_run(
        &self,
        store: &awaken_store_inmem::MemoryCommitCoordinator,
        thread_id: &ThreadId,
    ) -> Result<Option<RunRecord>, String> {
        self.latest_run_reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ProjectedQueries
            .authoritative_latest_run(store, thread_id)
            .await
    }

    async fn authoritative_committed_messages(
        &self,
        store: &awaken_store_inmem::MemoryCommitCoordinator,
        thread_id: &ThreadId,
    ) -> Result<Vec<Message>, String> {
        ProjectedQueries
            .authoritative_committed_messages(store, thread_id)
            .await
    }

    async fn authoritative_committed_state(
        &self,
        _store: &awaken_store_inmem::MemoryCommitCoordinator,
        _thread_id: &ThreadId,
    ) -> Result<Vec<StateCommand>, String> {
        self.state
            .lock()
            .expect("scripted authoritative state mutex poisoned")
            .clone()
    }

    async fn authoritative_open_wait_for_thread(
        &self,
        store: &awaken_store_inmem::MemoryCommitCoordinator,
        thread_id: &ThreadId,
    ) -> Result<Option<(RunId, ResumeTicket)>, String> {
        ProjectedQueries
            .authoritative_open_wait_for_thread(store, thread_id)
            .await
    }

    async fn events_after(
        &self,
        store: &awaken_store_inmem::MemoryCommitCoordinator,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunLifecycleFeedError> {
        ProjectedQueries.events_after(store, cursor, limit).await
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

    async fn authoritative_latest_run(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Option<RunRecord>, String> {
        self.queries
            .authoritative_latest_run(self.store.as_ref(), thread_id)
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

    async fn authoritative_committed_state(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<StateCommand>, String> {
        self.queries
            .authoritative_committed_state(self.store.as_ref(), thread_id)
            .await
    }

    async fn authoritative_open_wait_for_thread(
        &self,
        thread: &ThreadId,
    ) -> Result<Option<(RunId, ResumeTicket)>, String> {
        self.queries
            .authoritative_open_wait_for_thread(self.store.as_ref(), thread)
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

    fn dispatch_store(&self) -> Arc<awaken_dispatch::AnyDispatchStore>;

    fn dispatch_wake(&self) -> Option<Arc<dyn awaken_dispatch::WakeSignal>>;

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
    /// One clock for the dispatch store and every Thread's commit store.
    clock: Arc<dyn awaken_lease_contract::WallClock>,
    commits: std::sync::Mutex<std::collections::HashMap<String, Arc<dyn LocalCommit>>>,
    checkpoints:
        std::sync::Mutex<std::collections::HashMap<String, Arc<dyn StreamCheckpointStore>>>,
    dispatch: Arc<awaken_dispatch::AnyDispatchStore>,
}

#[cfg(any(test, feature = "test-support"))]
impl Default for EphemeralRuntimeAuthority {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl EphemeralRuntimeAuthority {
    /// Seed the existing test authority with a deliberately stale query adapter.
    #[cfg(test)]
    pub(crate) fn with_commit(self, thread: &str, commit: Arc<dyn LocalCommit>) -> Self {
        self.commits
            .lock()
            .unwrap()
            .insert(thread.to_owned(), commit);
        self
    }

    #[must_use]
    pub fn new() -> Self {
        // The ephemeral authority is the Coordinator-side durable owner used by
        // scenario/integration compositions. Preserve its atomic Session
        // continuation role when erasing the concrete Memory store; the
        // Worker-only constructor intentionally omits that role.
        let clock = awaken_lease_contract::system_clock();
        let dispatch = awaken_dispatch::AnyDispatchStore::from_durable_dispatch(Arc::new(
            awaken_dispatch::MemoryDispatchStore::new(clock.clone()),
        ));
        Self {
            clock,
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
                    awaken_store_inmem::MemoryCommitCoordinator::new(self.clock.clone()),
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

    fn dispatch_store(&self) -> Arc<awaken_dispatch::AnyDispatchStore> {
        self.dispatch.clone()
    }

    fn dispatch_wake(&self) -> Option<Arc<dyn awaken_dispatch::WakeSignal>> {
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
