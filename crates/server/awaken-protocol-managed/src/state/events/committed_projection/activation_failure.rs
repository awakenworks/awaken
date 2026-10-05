//! Terminal Managed projection for durable Session activation failures.

use super::*;

impl ManagedState {
    /// Derive the public terminal error from the Session root's one durable
    /// activation-failure fact. A missing/empty cause violates the application
    /// invariant and fails projection closed instead of inventing public bytes.
    pub(super) fn append_activation_failure_error_projection(
        &self,
        record: &mut SessionRecord,
        projection: &awaken_managed_session_events::ActivationFailureProjection,
    ) -> Result<(), StateError> {
        if let awaken_managed_session_events::ActivationFailureProjection::Ready {
            error: Some(fact),
        } = projection
        {
            record.publish_fact_once(fact, None)?;
        }
        Ok(())
    }
}
