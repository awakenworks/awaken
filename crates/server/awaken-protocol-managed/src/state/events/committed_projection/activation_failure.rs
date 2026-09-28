//! Terminal Managed projection for durable Session activation failures.

use super::*;

impl ManagedState {
    /// Derive the public terminal error from the Session root's one durable
    /// activation-failure fact. A missing/empty cause violates the application
    /// invariant and fails projection closed instead of inventing public bytes.
    pub(super) fn append_activation_failure_error_projection(
        &self,
        record: &mut SessionRecord,
        persisted: &awaken_session_state_contract::PersistedSession,
        lifecycle_events: &[RunLifecycleEvent],
    ) -> Result<(), StateError> {
        if let Some(fact) =
            awaken_managed_session_events::activation_failure_fact(persisted, lifecycle_events)
                .map_err(projection_error)?
        {
            record.publish_fact_once(&fact, None)?;
        }
        Ok(())
    }
}
