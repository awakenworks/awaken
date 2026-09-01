//! Prefix-stable entrypoint for the warm/cold committed projection.

use super::*;

impl ManagedState {
    /// Refresh this process's disposable event projection from the Runtime's one
    /// durable transcript and Run lifecycle feed. A Session cache hit is not proof
    /// that it contains commits accepted through another protocol or Coordinator.
    pub(crate) async fn refresh_committed_events(
        &self,
        session_id: &str,
    ) -> Result<(), StateError> {
        self.ensure_session_record(session_id).await?;
        self.refresh_committed_projection(session_id).await
    }

    /// Rehydration and ordinary reads share this seqlock-style retry. A changed
    /// Session or committed Thread prefix discards the mixed read instead of
    /// publishing a projection assembled from incomparable source versions.
    pub(in crate::state) async fn refresh_committed_projection(
        &self,
        session_id: &str,
    ) -> Result<(), StateError> {
        const PREFIX_RETRIES: usize = 4;
        for _ in 0..PREFIX_RETRIES {
            if self.refresh_committed_projection_once(session_id).await? {
                return Ok(());
            }
        }
        Err(StateError::Run(RunError::unavailable(
            "Managed projection sources changed while reading one committed prefix",
        )))
    }
}
