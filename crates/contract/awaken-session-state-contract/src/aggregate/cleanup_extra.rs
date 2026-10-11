use super::{SessionCleanupError, SessionCleanupOperations, SessionRepositoryPublicationReceipt};

impl SessionCleanupOperations<'_> {
    /// Join the exact publication receipt to this root's verified frozen cleanup.
    pub fn record_repository_publication_receipt(
        &mut self,
        receipt: SessionRepositoryPublicationReceipt,
    ) -> Result<bool, SessionCleanupError> {
        let state = &mut self.aggregate.state;
        state.terminal_cleanup.verify_for(&state.session_id)?;
        state
            .terminal_cleanup
            .record_repository_publication_receipt(&state.session_id, receipt)
    }
}
