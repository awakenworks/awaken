//! Read-only queries over the compatible Session DTO's retained provenance.
use crate::state::PersistedSession;

/// A borrowed history view with no execution or mutation capability.
pub struct SessionEventHistory<'a> {
    session: &'a PersistedSession,
}

impl<'a> From<&'a PersistedSession> for SessionEventHistory<'a> {
    fn from(session: &'a PersistedSession) -> Self {
        Self { session }
    }
}

impl SessionEventHistory<'_> {
    /// Highest committed projection anchor retained by any accepted Event.
    #[must_use]
    pub fn projection_cursor(&self) -> u64 {
        self.session
            .event_batches
            .iter()
            .flat_map(|batch| &batch.events)
            .filter_map(|entry| entry.projection_anchor)
            .map(|anchor| anchor.source_commit_cursor)
            .max()
            .unwrap_or_default()
    }

    /// Whether retained User provenance owns this Run. Inspect every batch so
    /// malformed later provenance remains an error even after a match.
    pub fn retains_user_run(
        &self,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<bool, crate::state::SessionEventBatchError> {
        let mut retained = false;
        for batch in &self.session.event_batches {
            retained |= batch.user_run_operation_id(run_id)?.is_some();
        }
        Ok(retained)
    }
}
