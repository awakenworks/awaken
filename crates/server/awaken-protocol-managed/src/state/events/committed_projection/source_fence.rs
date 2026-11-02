// @formal entrypoint session.runtime_projection_failed_refresh_stutters_warm_state
// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
//! Delegate the candidate source fence to the Session application read owner.

use super::candidate_evidence::CandidateEvidence;
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;
use awaken_session_execution_contract::SessionProjectionPrefix;

impl ManagedState {
    pub(super) async fn candidate_source_moved(
        &self,
        session_id: &str,
        persisted: &awaken_session_contract::PersistedSession,
        evidence: &CandidateEvidence,
    ) -> Result<bool, StateError> {
        if evidence.outcome_query_incomplete {
            tracing::debug!(
                session_id,
                reason = "outcome_terminal_query_root_mismatch",
                "Managed projection retry"
            );
            return Ok(true);
        }
        self.roles
            .projection
            .projection_prefix_moved(
                persisted,
                SessionProjectionPrefix {
                    root: evidence.root.as_ref(),
                    links: &evidence.links,
                    children: &evidence.children,
                },
            )
            .await
            .map_err(StateError::Run)
    }
}
