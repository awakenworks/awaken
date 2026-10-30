// @formal surface requirement=session.lifecycle.session_state
// @formal reason: This controlled mutation joins an exact publication receipt to the root's frozen
// @formal reason: cleanup identity before repository CAS. It records the supplied evidence rather
// @formal reason: than proving physical publication, disposal or absence.
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
