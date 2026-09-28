// @formal entrypoint session.runtime_projection_failed_refresh_stutters_warm_state
// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
//! The source fence of one projection candidate.
//!
//! Session revision does not change for Runtime commits. After the candidate's
//! evidence is read and derived, the root Thread prefix, the coordinated links
//! and every child prefix are read again; any movement discards the candidate
//! so no mixed prefix is published. Consumer: the candidate builder.

use super::candidate_evidence::CandidateEvidence;
use super::*;
use awaken_managed_session_events::same_thread_projection_prefix;

/// A coordinated Session needs a trailing root read that closes over every
/// child snapshot; a root-only Session's validation root is its final fence.
pub(in crate::state::events) const fn projection_validation_needs_trailing_root(
    coordinated_links: usize,
    child_snapshots: usize,
) -> bool {
    coordinated_links != 0 || child_snapshots != 0
}

impl ManagedState {
    /// Whether any source prefix the candidate was built from moved.
    pub(super) async fn candidate_source_moved(
        &self,
        session_id: &str,
        persisted: &awaken_session_state_contract::PersistedSession,
        evidence: &CandidateEvidence,
    ) -> Result<bool, StateError> {
        // Validation owns no published payload. Release these second full
        // snapshots before constructing the candidate's wire events. Select
        // the fresh validation root once and lend that exact prefix to
        // relationship projection. A root-only Session performs no
        // asynchronous child reads afterwards, so this same complete root is
        // also its final fence. A coordinated Session retains the later
        // trailing root read that closes over every child snapshot.
        let validation_root_snapshot = self.recovery_snapshot(session_id, session_id).await?;
        let (final_links, final_child_snapshots) = self
            .coordinated_projection_prefix(session_id, validation_root_snapshot.as_ref())
            .await?;
        if evidence.links != final_links
            || evidence.children.len() != final_child_snapshots.len()
            || evidence.children.iter().any(|(thread_id, initial)| {
                !same_thread_projection_prefix(Some(initial), final_child_snapshots.get(thread_id))
            })
        {
            tracing::debug!(
                session_id,
                reason = "coordinated_prefix",
                "Managed projection retry"
            );
            return Ok(true);
        }
        let final_root_snapshot = if projection_validation_needs_trailing_root(
            final_links.len(),
            final_child_snapshots.len(),
        ) {
            self.recovery_snapshot(session_id, session_id).await?
        } else {
            validation_root_snapshot
        };
        if !same_thread_projection_prefix(evidence.root.as_ref(), final_root_snapshot.as_ref()) {
            tracing::debug!(
                session_id,
                reason = "root_thread_prefix",
                initial_version = ?evidence.root.as_ref().map(|snapshot| snapshot.thread_version),
                final_version = ?final_root_snapshot.as_ref().map(|snapshot| snapshot.thread_version),
                "Managed projection retry"
            );
            return Ok(true);
        }

        let final_persisted = self
            .application
            .session(&awaken_session_types::SessionId::from(session_id))
            .await
            .map_err(StateError::from)?;
        if final_persisted.session_id != persisted.session_id
            || final_persisted.revision != persisted.revision
        {
            tracing::debug!(
                session_id,
                reason = "session_revision",
                initial_revision = ?persisted.revision,
                final_revision = ?final_persisted.revision,
                "Managed projection retry"
            );
            return Ok(true);
        }
        Ok(false)
    }
}
