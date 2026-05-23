//! Commit-coordinator wiring for `AgentRuntime` (ADR-0036).

use std::sync::Arc;

use awaken_contract::contract::commit_coordinator::CommitCoordinator;

use super::AgentRuntime;

impl AgentRuntime {
    /// Wire a `CommitCoordinator` for atomic checkpoint commits across
    /// `ThreadRunStore` and `EventStore` writes (ADR-0036). When set, the
    /// runtime tees durable canonical drafts through the coordinator at
    /// checkpoint cadence instead of letting `ThreadRunStore::checkpoint`
    /// and `EventWriter::append` run in independent transactions.
    #[must_use]
    pub fn with_commit_coordinator(mut self, coordinator: Arc<dyn CommitCoordinator>) -> Self {
        self.commit_coordinator = Some(coordinator);
        self
    }

    /// ADR-0036 D8 convenience: pair an in-memory `ThreadRunStore` with a
    /// matching `MemoryCommitCoordinator` in one call.
    #[must_use]
    pub fn with_in_memory_thread_run_store(self, store: Arc<awaken_stores::InMemoryStore>) -> Self {
        let coord = awaken_stores::MemoryCommitCoordinator::wrap(Arc::clone(&store));
        self.with_thread_run_store(
            store as Arc<dyn awaken_contract::contract::storage::ThreadRunStore>,
        )
        .with_commit_coordinator(coord as Arc<dyn CommitCoordinator>)
    }

    /// Return the wired commit coordinator, if any.
    pub fn commit_coordinator(&self) -> Option<&Arc<dyn CommitCoordinator>> {
        self.commit_coordinator.as_ref()
    }
}
