// @formal entrypoint background_task.terminal_observer_reads_authoritative_committed_state_not_process_projection
//! Runtime-facing durable authority contracts.
//!
//! The Runtime Host owns execution semantics, but it does not select or open a
//! database. A Coordinator startup injects one implementation of these
//! contracts. Database-less Workers have none and reach committed truth through
//! their remote, claim-fenced transports instead. [`SessionAuthority`] is the
//! one place the Host asks which of the two it is given.

use crate::memory_references::SessionMemoryReferenceEncoder;
use std::sync::Arc;

#[cfg(test)]
use awaken_agent_contract::agent::awaiting::ResumeTicket;
#[cfg(test)]
use awaken_agent_contract::agent::message::Message;
#[cfg(test)]
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord};
#[cfg(test)]
use awaken_agent_contract::agent::state::Command as StateCommand;
#[cfg(test)]
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
#[cfg(test)]
use awaken_agent_contract::thread::read::lifecycle::{
    RunLifecycleCursor, RunLifecycleFeedError, RunLifecyclePage,
};
use awaken_dispatch_erasure::RuntimeAuthority;
#[cfg(test)]
use awaken_dispatch_erasure::{LocalCommitQueries, ProjectedQueries};
use awaken_session_run_delivery::SessionCommitSource;

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

/// Where this Host's Session truth lives, answered once per decision.
///
/// A Coordinator or single-process Host keeps committed truth in its own
/// [`RuntimeAuthority`] ([`LocalAuthority`]); a database-less Worker reaches it
/// upstream through claim-fenced transports ([`UpstreamAuthority`]). Every
/// role-dependent choice the Host makes is one method here, named for that
/// decision, so the Host never asks which role it plays. Role-neutral
/// dependencies (Session Control, the Worker credential resolver) stay outside.
/// Raw commit access uses the shared `SessionCommitSource` role; execution
/// choices remain here, with no second commit-opening interface.
pub(crate) trait SessionAuthority: SessionCommitSource {
    fn coordinator_run_commits(
        &self,
    ) -> Option<
        Arc<
            dyn awaken_dispatch_contract::ClaimedRunCommit<awaken_runtime_contract::runs::RunError>,
        >,
    >;

    /// The durable interrupted-stream checkpoint store, when this process owns
    /// one. A Worker persists checkpoints through its claim-fenced dispatch
    /// transport, so a local store would be a second authority.
    fn stream_checkpoint(
        &self,
        thread: &str,
    ) -> Result<Option<Arc<dyn StreamCheckpointStore>>, crate::SessionExecutionError>;

    /// The claim-fenced commit transport claimed Runs write through; `None`
    /// when they commit through this process's own authority.
    #[cfg(test)]
    fn claimed_commit(
        &self,
    ) -> Result<
        Option<Arc<dyn awaken_dispatch_contract::ClaimedRunCommit>>,
        crate::SessionExecutionError,
    >;

    /// The claim-fenced live-progress publisher; `None` relays progress through
    /// this process's completion registry.
    fn live_progress_publisher(
        &self,
    ) -> Option<Arc<dyn awaken_dispatch_contract::ClaimedStreamPublisher>>;

    /// The Control client that hands out cold terminal-cleanup assignments.
    fn terminal_cleanup_control(
        &self,
    ) -> Option<Arc<dyn awaken_session_execution_contract::SessionTerminalCleanupControl>>;

    /// The durable dispatch store this process owns, if any.
    fn dispatch_binding(&self) -> Option<awaken_dispatch_erasure::DispatchBinding>;

    /// The cross-node dispatch wake this process owns, if any.
    fn dispatch_wake(&self) -> Option<Arc<dyn awaken_dispatch_contract::WakeSignal>>;

    /// Where committed truth lives for this Host. Every role-dependent bool
    /// the Host asks is a projection of this one closed fact, so no impl can
    /// answer them inconsistently.
    fn committed_truth(&self) -> CommittedTruth;

    /// The encoder that turns a fenced Memory operation into the upstream
    /// transport's opaque reference.
    fn memory_reference_encoder(&self) -> Option<Arc<dyn SessionMemoryReferenceEncoder>>;
}

/// Where a Host's committed truth lives. The five role-dependent choices the
/// Host makes all follow from this one fact, and only these two combinations
/// of them occur:
///
/// | choice                              | `Local` | `Upstream` |
/// |-------------------------------------|---------|------------|
/// | `cancels_through_durable_intent`    | yes     | no         |
/// | `observes_terminal_memory_locally`  | yes     | no         |
/// | `scans_committed_terminals`         | yes     | no         |
/// | `hosts_session_application`         | yes     | no         |
/// | `fetches_resources_upstream`        | no      | yes        |
///
/// Any other combination (for example a Worker that scans committed
/// terminals it cannot read, or a Coordinator that fetches its own resources
/// upstream) is not representable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommittedTruth {
    /// A Coordinator or single process: committed truth is this process's
    /// own [`RuntimeAuthority`].
    Local,
    /// A database-less Worker: committed truth is the Coordinator's, reached
    /// only through claim-fenced transports.
    Upstream,
}

impl CommittedTruth {
    /// Cancelling a Run records the durable cancellation intent (`true`), or
    /// only signals the attempt resident in this process (`false`): a Worker's
    /// transport cannot list, requeue, or cancel the global queue.
    pub(crate) const fn cancels_through_durable_intent(self) -> bool {
        matches!(self, Self::Local)
    }

    /// Terminal Memory extraction is observed in this process after its own
    /// commit (`true`), or by the guarded settlement observer beside the
    /// upstream commit (`false`). Recovery of extraction intents follows the
    /// same owner.
    pub(crate) const fn observes_terminal_memory_locally(self) -> bool {
        matches!(self, Self::Local)
    }

    /// This process can scan every Thread's committed truth to reconcile
    /// dispatch rows whose Run already ended.
    pub(crate) const fn scans_committed_terminals(self) -> bool {
        matches!(self, Self::Local)
    }

    /// The Session application that admits BackgroundTask completion attention
    /// and MCP application input runs in this process.
    pub(crate) const fn hosts_session_application(self) -> bool {
        matches!(self, Self::Local)
    }

    /// Resources (Skill bundles, File content, Memory, Repository transport)
    /// are fetched upstream under a claim or Session realization fence
    /// (`true`), or directly from this process's stores (`false`).
    pub(crate) const fn fetches_resources_upstream(self) -> bool {
        matches!(self, Self::Upstream)
    }
}

/// The authority of a Host that owns committed truth: a Coordinator or a single
/// process. Its selected commit/Dispatch/checkpoint source is mandatory before
/// a Host can be constructed; availability errors remain the source's result.
pub(crate) struct LocalAuthority {
    runtime: Arc<dyn RuntimeAuthority>,
}

impl LocalAuthority {
    pub(crate) fn new(runtime: Arc<dyn RuntimeAuthority>) -> Self {
        Self { runtime }
    }
}

#[async_trait::async_trait]
impl SessionCommitSource for LocalAuthority {
    async fn read_transcript_page(
        &self,
        session: &str,
        request: &awaken_agent_contract::thread::read::transcript::TranscriptPageRequest,
    ) -> Result<
        awaken_agent_contract::thread::read::transcript::TranscriptPage,
        awaken_runtime_contract::runs::RunError,
    > {
        self.runtime.read_transcript_page(session, request).await
    }

    async fn open_for_read(
        &self,
        session: &str,
    ) -> Result<
        Arc<awaken_session_run_delivery::SessionCommit>,
        awaken_runtime_contract::runs::RunError,
    > {
        awaken_session_run_delivery::AuthorityCommits::new(self.runtime.clone())
            .open_for_read(session)
            .await
    }

    async fn durable_thread_exists(
        &self,
        thread: &str,
    ) -> Result<bool, awaken_runtime_contract::runs::RunError> {
        awaken_session_run_delivery::AuthorityCommits::new(self.runtime.clone())
            .durable_thread_exists(thread)
            .await
    }
}

impl SessionAuthority for LocalAuthority {
    fn coordinator_run_commits(
        &self,
    ) -> Option<
        Arc<
            dyn awaken_dispatch_contract::ClaimedRunCommit<awaken_runtime_contract::runs::RunError>,
        >,
    > {
        Some(self.runtime.clone().run_commits())
    }

    fn stream_checkpoint(
        &self,
        thread: &str,
    ) -> Result<Option<Arc<dyn StreamCheckpointStore>>, crate::SessionExecutionError> {
        Ok(Some(self.runtime.stream_checkpoint(thread)?))
    }

    #[cfg(test)]
    fn claimed_commit(
        &self,
    ) -> Result<
        Option<Arc<dyn awaken_dispatch_contract::ClaimedRunCommit>>,
        crate::SessionExecutionError,
    > {
        Ok(None)
    }

    fn live_progress_publisher(
        &self,
    ) -> Option<Arc<dyn awaken_dispatch_contract::ClaimedStreamPublisher>> {
        None
    }

    fn terminal_cleanup_control(
        &self,
    ) -> Option<Arc<dyn awaken_session_execution_contract::SessionTerminalCleanupControl>> {
        None
    }

    fn dispatch_binding(&self) -> Option<awaken_dispatch_erasure::DispatchBinding> {
        Some(awaken_dispatch_erasure::DispatchBinding::coordinator(
            self.runtime.clone(),
        ))
    }

    fn dispatch_wake(&self) -> Option<Arc<dyn awaken_dispatch_contract::WakeSignal>> {
        self.runtime.dispatch_wake()
    }

    fn committed_truth(&self) -> CommittedTruth {
        CommittedTruth::Local
    }

    fn memory_reference_encoder(&self) -> Option<Arc<dyn SessionMemoryReferenceEncoder>> {
        None
    }
}

/// The authority of a database-less Worker: committed truth lives in the
/// Coordinator, reached only through the claim-fenced transports the Worker
/// composition installs here. A missing transport fails closed where it is
/// used; no transport grants the Worker a local write authority.
#[derive(Default)]
pub struct UpstreamAuthority {
    dispatch: Option<awaken_dispatch_erasure::DispatchBinding>,
    stream_publisher: Option<Arc<dyn awaken_dispatch_contract::ClaimedStreamPublisher>>,
    terminal_cleanup_control:
        Option<Arc<dyn awaken_session_execution_contract::SessionTerminalCleanupControl>>,
    memory_reference_encoder: Option<Arc<dyn SessionMemoryReferenceEncoder>>,
}

impl UpstreamAuthority {
    /// Required executable-Worker wiring, checked before Host construction.
    /// These are connection selections, not evidence that any remote effect
    /// was authorized or completed; actual calls still verify their fences.
    pub(crate) fn validate_worker_connections(&self) -> Result<(), crate::SessionExecutionError> {
        let required = [
            (self.dispatch.is_some(), "upstream dispatch transport"),
            (
                self.stream_publisher.is_some(),
                "claim-fenced stream publisher",
            ),
            (
                self.terminal_cleanup_control.is_some(),
                "Session terminal cleanup control",
            ),
            (
                self.memory_reference_encoder.is_some(),
                "fenced Memory reference encoder",
            ),
        ];
        if let Some((_, name)) = required.into_iter().find(|(present, _)| !present) {
            return Err(crate::SessionExecutionError::bad_request(format!(
                "Worker construction requires {name}",
            )));
        }
        Ok(())
    }

    /// The Worker's sole queue transport, selected with its other upstream ports.
    #[must_use]
    pub fn with_dispatch_binding(
        mut self,
        binding: awaken_dispatch_erasure::DispatchBinding,
    ) -> Self {
        self.dispatch = Some(binding);
        self
    }

    /// The registered Worker's claim-fenced live-progress publisher.
    #[must_use]
    pub fn with_stream_publisher(
        mut self,
        publisher: Arc<dyn awaken_dispatch_contract::ClaimedStreamPublisher>,
    ) -> Self {
        self.stream_publisher = Some(publisher);
        self
    }

    /// The Control client that hands this Worker cold terminal-cleanup
    /// assignments.
    #[must_use]
    pub fn with_terminal_cleanup_control(
        mut self,
        control: Arc<dyn awaken_session_execution_contract::SessionTerminalCleanupControl>,
    ) -> Self {
        self.terminal_cleanup_control = Some(control);
        self
    }

    /// The Resource transport's opaque Memory reference encoder. The Host
    /// neither owns nor inspects the wire representation.
    #[must_use]
    pub fn with_memory_reference_encoder<E>(mut self, encoder: Arc<E>) -> Self
    where
        E: awaken_resource_contract::MemoryMaterializationReferenceEncoder<
                awaken_worker_contract::RunClaim,
            > + awaken_resource_contract::MemoryMaterializationReferenceEncoder<
                awaken_session_execution_contract::SessionResourceMaterializationIntent,
            > + awaken_resource_contract::MemoryMaterializationReferenceEncoder<
                awaken_session_contract::SessionTerminalMemoryIntent,
            > + 'static,
    {
        self.memory_reference_encoder = Some(encoder);
        self
    }
}

#[async_trait::async_trait]
impl SessionCommitSource for UpstreamAuthority {
    async fn open_for_read(
        &self,
        _session: &str,
    ) -> Result<
        Arc<awaken_session_run_delivery::SessionCommit>,
        awaken_runtime_contract::runs::RunError,
    > {
        Ok(Arc::new(
            awaken_session_run_delivery::SessionCommit::Remote(
                awaken_session_run_delivery::WorkerRecoveryCommit::new(),
            ),
        ))
    }

    async fn durable_thread_exists(
        &self,
        _thread: &str,
    ) -> Result<bool, awaken_runtime_contract::runs::RunError> {
        Ok(false)
    }
}

impl SessionAuthority for UpstreamAuthority {
    fn coordinator_run_commits(
        &self,
    ) -> Option<
        Arc<
            dyn awaken_dispatch_contract::ClaimedRunCommit<awaken_runtime_contract::runs::RunError>,
        >,
    > {
        None
    }

    fn stream_checkpoint(
        &self,
        _thread: &str,
    ) -> Result<Option<Arc<dyn StreamCheckpointStore>>, crate::SessionExecutionError> {
        Ok(None)
    }

    #[cfg(test)]
    fn claimed_commit(
        &self,
    ) -> Result<
        Option<Arc<dyn awaken_dispatch_contract::ClaimedRunCommit>>,
        crate::SessionExecutionError,
    > {
        self.dispatch
            .as_ref()
            .and_then(|binding| match binding.placement() {
                awaken_dispatch_erasure::DispatchTopology::Remote { commit, .. } => {
                    Some(commit.clone())
                }
                awaken_dispatch_erasure::DispatchTopology::Local(_)
                | awaken_dispatch_erasure::DispatchTopology::Coordinator { .. } => None,
            })
            .map(Some)
            .ok_or_else(|| {
                crate::SessionExecutionError::internal(
                    "database-less Worker requires an injected claimed-commit transport",
                )
            })
    }

    fn live_progress_publisher(
        &self,
    ) -> Option<Arc<dyn awaken_dispatch_contract::ClaimedStreamPublisher>> {
        self.stream_publisher.clone()
    }

    fn terminal_cleanup_control(
        &self,
    ) -> Option<Arc<dyn awaken_session_execution_contract::SessionTerminalCleanupControl>> {
        self.terminal_cleanup_control.clone()
    }

    fn dispatch_binding(&self) -> Option<awaken_dispatch_erasure::DispatchBinding> {
        self.dispatch.clone()
    }

    fn dispatch_wake(&self) -> Option<Arc<dyn awaken_dispatch_contract::WakeSignal>> {
        None
    }

    fn committed_truth(&self) -> CommittedTruth {
        CommittedTruth::Upstream
    }

    fn memory_reference_encoder(&self) -> Option<Arc<dyn SessionMemoryReferenceEncoder>> {
        self.memory_reference_encoder.clone()
    }
}

#[cfg(test)]
mod worker_connection_tests {
    #[test]
    fn executable_worker_requires_every_existing_upstream_connection() {
        // C1 dispatch binding (queue + checkpoint + credentials + claim commit);
        // C2 stream publisher; C3 terminal cleanup; C4 fenced Memory encoder.
        // R1 all present; R2-R5 each connection absent => refusal, zero effects.
        // A binding with a missing claim commit is unrepresentable: the erasure
        // crate's compile-fail cases cover that construction boundary.
        let selected = crate::host::test_upstream_authority();
        assert!(selected.validate_worker_connections().is_ok(), "R1");
        for missing in 0..4 {
            let mut selected = crate::host::test_upstream_authority();
            let expected = match missing {
                0 => {
                    selected.dispatch = None;
                    "dispatch transport"
                }
                1 => {
                    selected.stream_publisher = None;
                    "claim-fenced stream"
                }
                2 => {
                    selected.terminal_cleanup_control = None;
                    "terminal cleanup"
                }
                3 => {
                    selected.memory_reference_encoder = None;
                    "Memory reference"
                }
                _ => {
                    selected.dispatch = None;
                    "dispatch transport"
                }
            };
            assert!(
                selected
                    .validate_worker_connections()
                    .unwrap_err()
                    .to_string()
                    .contains(expected),
                "R{}",
                missing + 2
            );
        }
    }
}

#[cfg(test)]
mod committed_truth_tests {
    use super::{CommittedTruth, SessionAuthority as _};

    #[test]
    fn every_role_choice_is_one_projection_of_committed_truth() {
        // | truth    | durable cancel | local Memory | scan terminals | application | upstream resources |
        // |----------|----------------|--------------|----------------|-------------|--------------------|
        // | Local    | yes            | yes          | yes            | yes         | no                 |
        // | Upstream | no             | no           | no             | no          | yes                |
        let row = |truth: CommittedTruth| {
            [
                truth.cancels_through_durable_intent(),
                truth.observes_terminal_memory_locally(),
                truth.scans_committed_terminals(),
                truth.hosts_session_application(),
                truth.fetches_resources_upstream(),
            ]
        };
        assert_eq!(row(CommittedTruth::Local), [true, true, true, true, false]);
        assert_eq!(
            row(CommittedTruth::Upstream),
            [false, false, false, false, true]
        );
        assert_eq!(
            crate::host::test_upstream_authority().committed_truth(),
            CommittedTruth::Upstream,
            "a database-less Worker reaches committed truth upstream"
        );
    }
}

#[cfg(test)]
mod commit_source_tests;
