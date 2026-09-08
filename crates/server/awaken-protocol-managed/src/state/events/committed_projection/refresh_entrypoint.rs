//! Prefix-stable entrypoint for the warm/cold committed projection.

use super::*;
use crate::state::constants::COMMITTED_PREFIX_READ_CONFLICT;

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
        // One ordinary turn may commit more than four source versions while a
        // reader is in flight. Bound elapsed read work, not the number of valid
        // concurrent commits. Every retry still rebuilds and validates the full
        // candidate; the delay gives the writer time without a database hot loop.
        const READ_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(10);
        let mut attempts = 0_u32;
        let refreshed = tokio::time::timeout(READ_BUDGET, async {
            loop {
                attempts += 1;
                if self.refresh_committed_projection_once(session_id).await? {
                    return Ok(());
                }
                tokio::time::sleep(RETRY_DELAY).await;
            }
        })
        .await;
        if let Ok(result) = refreshed {
            return result;
        }
        tracing::warn!(
            session_id,
            attempts,
            "Managed projection read budget exhausted"
        );
        Err(StateError::Run(RunError::unavailable(
            COMMITTED_PREFIX_READ_CONFLICT,
        )))
    }
}
