// @formal surface obligation=background_task.terminal_observer_reads_authoritative_committed_state_not_process_projection
// @formal surface obligation=remote_worker.committed_truth_is_coordinator_owned
// @formal reason: The original backend-neutral commit/read/recovery delegation over one selected store
// @formal reason: and query policy; this module opens no backend and adds no durable authority or decision.
//! Local commit roles delegated to the one store selected by process startup.

use std::sync::Arc;

use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord};
use awaken_agent_contract::agent::state::Command as StateCommand;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator, Error, OperationCoordinator,
};
use awaken_agent_contract::thread::commit::operation::{CommitOperation, CommitReceipt};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::checkpoint::CheckpointReader;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::lifecycle::{
    RunLifecycleCursor, RunLifecycleFeed, RunLifecycleFeedError, RunLifecyclePage,
    checkpoint_lifecycle_events_after,
};
use awaken_agent_contract::thread::read::recovery::{
    RecoveryError, RunRecoverySnapshot, RunRecoverySource,
};
use awaken_agent_contract::thread::read::transcript::{TranscriptPage, TranscriptPageRequest};

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

    async fn authoritative_transcript_page(
        &self,
        request: &TranscriptPageRequest,
    ) -> Result<TranscriptPage, String>;

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
pub trait LocalCommitQueries<S: Sync>: Send + Sync {
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

    async fn authoritative_transcript_page(
        &self,
        store: &S,
        request: &TranscriptPageRequest,
    ) -> Result<TranscriptPage, String> {
        let messages = self
            .authoritative_committed_messages(store, &request.thread_id)
            .await?;
        request
            .page_from_append_only(messages)
            .map_err(|error| error.to_string())
    }

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
/// The commit/read/recovery port implementations live with this adapter;
/// process startups only select the existing store and query policy.
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
        Self::projected_shared(Arc::new(store))
    }

    /// Share one store with its other read roles using the same canonical
    /// projection query policy rather than copying it into the composition.
    #[must_use]
    pub fn projected_shared(store: Arc<S>) -> Self {
        Self::with_queries(store, ProjectedQueries)
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

    async fn authoritative_transcript_page(
        &self,
        request: &TranscriptPageRequest,
    ) -> Result<TranscriptPage, String> {
        self.queries
            .authoritative_transcript_page(self.store.as_ref(), request)
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

// All delegation for a Coordinator-owned commit source is centralized here so
// startup adapters cannot become parallel implementations of persistence
// authority (ADR-0065/0066 architecture fitness).
#[async_trait::async_trait]
impl<S, Q> Coordinator for LocalCommitAdapter<S, Q>
where
    S: Coordinator + Send + Sync,
    Q: Send + Sync,
{
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, Error> {
        self.store.as_ref().commit(commit).await
    }
}

#[async_trait::async_trait]
impl<S, Q> OperationCoordinator for LocalCommitAdapter<S, Q>
where
    S: OperationCoordinator + Send + Sync,
    Q: Send + Sync,
{
    async fn commit_operation(&self, operation: CommitOperation) -> Result<CommitReceipt, Error> {
        self.store.as_ref().commit_operation(operation).await
    }
}

impl<S, Q> CommittedThreadView for LocalCommitAdapter<S, Q>
where
    S: CommittedThreadView,
    Q: Send + Sync,
{
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        self.store.as_ref().committed_messages(thread_id)
    }

    fn run(&self, run_id: &RunId) -> Option<RunRecord> {
        self.store.as_ref().run(run_id)
    }

    fn latest_run(&self, thread_id: &ThreadId) -> Option<RunRecord> {
        self.store.as_ref().latest_run(thread_id)
    }

    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket> {
        self.store.as_ref().resume_ticket(run_id)
    }

    fn open_wait_for_thread(&self, thread_id: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        CommittedThreadView::open_wait_for_thread(self.store.as_ref(), thread_id)
    }

    fn committed_state(
        &self,
        thread_id: &ThreadId,
    ) -> Vec<awaken_agent_contract::agent::state::Command> {
        self.store.as_ref().committed_state(thread_id)
    }
}

#[async_trait::async_trait]
impl<S, Q> RunRecoverySource for LocalCommitAdapter<S, Q>
where
    S: RunRecoverySource + Send + Sync,
    Q: Send + Sync,
{
    async fn recovery_snapshot(
        &self,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        self.store
            .as_ref()
            .recovery_snapshot(thread_id, claimed_run_id)
            .await
    }
}

#[async_trait::async_trait]
impl<S, Q> RunLifecycleFeed for LocalCommitAdapter<S, Q>
where
    S: Send + Sync,
    Q: LocalCommitQueries<S>,
{
    async fn events_after(
        &self,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunLifecycleFeedError> {
        self.queries
            .events_after(self.store.as_ref(), cursor, limit)
            .await
    }
}
