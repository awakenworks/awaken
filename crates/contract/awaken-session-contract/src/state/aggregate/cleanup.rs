use super::SessionCleanupOperations;

impl SessionCleanupOperations<'_> {
    /// Apply the canonical PersistedSession::ensure_terminal_cleanup_fence law.
    pub fn ensure_terminal_cleanup_fence(&mut self) -> bool {
        self.aggregate.state.ensure_terminal_cleanup_fence()
    }

    /// Apply the canonical PersistedSession::freeze_terminal_cleanup_targets law.
    pub fn freeze_terminal_cleanup_targets(
        &mut self,
        thread_ids: impl IntoIterator<Item = String>,
        delegation_watermark: u64,
        runtime_commit_cursor: u64,
    ) -> Result<bool, crate::state::SessionCleanupError> {
        self.aggregate.state.freeze_terminal_cleanup_targets(
            thread_ids,
            delegation_watermark,
            runtime_commit_cursor,
        )
    }

    /// Apply the canonical PersistedSession::normalize_legacy_terminal_cleanup law.
    pub fn normalize_legacy_terminal_cleanup(
        &mut self,
        release_reason: impl Into<String>,
    ) -> Result<bool, crate::state::SessionCleanupError> {
        self.aggregate
            .state
            .normalize_legacy_terminal_cleanup(release_reason)
    }

    /// Apply the canonical PersistedSession::record_terminal_cleanup_preparation law.
    pub fn record_terminal_cleanup_preparation(
        &mut self,
        workspace_id: &str,
        lease: &crate::state::SessionRealizationLease,
        preparation: crate::state::SessionCleanupPreparation,
        repository_preparation: Option<crate::state::SessionCleanupRepositoryPreparation>,
    ) -> Result<bool, crate::state::SessionCleanupError> {
        self.aggregate.state.record_terminal_cleanup_preparation(
            workspace_id,
            lease,
            preparation,
            repository_preparation,
        )
    }

    /// Apply the canonical PersistedSession::record_terminal_cleanup_disposal law.
    pub fn record_terminal_cleanup_disposal(
        &mut self,
        workspace_id: &str,
        lease: &crate::state::SessionRealizationLease,
        receipt: crate::state::SessionCleanupDisposalReceipt,
        release_reason: impl Into<String>,
    ) -> Result<bool, crate::state::SessionCleanupError> {
        self.aggregate.state.record_terminal_cleanup_disposal(
            workspace_id,
            lease,
            receipt,
            release_reason,
        )
    }
}
