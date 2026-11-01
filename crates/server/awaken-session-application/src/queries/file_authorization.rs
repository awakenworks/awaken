// @formal entrypoint session_resources.external_io_requires_durable_prepared_state
//! Session-owned File observation over the selected read-only repository.

use super::SessionQueries;

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionFileReadAuthorization for SessionQueries {
    async fn session_file_is_frozen_for_worker(
        &self,
        session_id: &awaken_session_types::SessionId,
        workspace_id: &str,
        identity: &awaken_worker_contract::WorkerIdentity,
        file_id: &str,
        observed_at_unix_ms: u64,
    ) -> bool {
        let Ok(owner_scope) = self.repository.owner(session_id).await else {
            return false;
        };
        if owner_scope != workspace_id {
            return false;
        }
        let Ok(session) = self.repository.get(session_id).await else {
            return false;
        };
        let lease_matches = session.realization.as_ref().is_some_and(|lease| {
            lease.owner == identity.worker_id
                && lease.runtime_incarnation == identity.lease_owner()
                && awaken_session_configuration_contract::resource::realization_lease_is_live_at(
                    lease.expires_at_unix_ms,
                    observed_at_unix_ms,
                )
        });
        session.frozen_baseline().is_some()
            && !session.is_terminal()
            && lease_matches
            && session.resources.desired().inputs().iter().any(|input| {
                matches!(
                    &input.source,
                    awaken_session_configuration_contract::resource::ResolvedInputSource::File { file_id: frozen_id }
                        if frozen_id.as_str() == file_id
                )
            })
    }
}
