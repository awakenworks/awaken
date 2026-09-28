//! The single entry from Worker authority into the Session realization driver.

use super::*;

impl HostWorkerResolver {
    #[cfg(test)]
    pub(crate) async fn realize_session(
        host: &SharedHost,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        session_id: &awaken_session_types::SessionId,
        directive: awaken_session_realization_contract::SessionRealizationDirective,
        claim: Option<&awaken_dispatch::RunClaim>,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
        rebuild_unavailable_environment: bool,
    ) -> Result<(), awaken_dispatch::Error> {
        let realization = host
            .session_slots
            .acquire_realization(session_id.as_str())
            .await;
        let synchronizer = WorkerProjectionSynchronizer::for_directive(
            host,
            &directive,
            claim,
            published_snapshot,
            rebuild_unavailable_environment,
            None,
        );
        Self::drive_session_realization_with_synchronizer(
            control,
            directive,
            synchronizer,
            awaken_authority_clock::authority_proof_started()
                .map_err(|error| Self::execution_error(error.to_string()))?,
            &realization,
        )
        .await
        .map_err(Self::map_session_realization_drive_error)
    }

    /// Drive one already-serialized directive under the Session's realization
    /// lock so no caller can create a parallel phase driver.
    pub(crate) async fn drive_session_realization(
        host: &SharedHost,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        session_id: &awaken_session_types::SessionId,
        directive: awaken_session_realization_contract::SessionRealizationDirective,
        claimed: Option<&awaken_dispatch::Claimed>,
        request_started: awaken_runtime_contract::authority_lease::AuthorityProofStarted,
        realization: &crate::session_slot::SessionRealizationPermit,
    ) -> Result<(), awaken_dispatch::Error> {
        let claim = claimed.map(|claimed| awaken_dispatch::RunClaim::from(&claimed.lease));
        let synchronizer = WorkerProjectionSynchronizer::for_claimed(
            host,
            session_id.as_str(),
            &directive,
            claimed,
            claim.as_ref(),
        );
        Self::drive_session_realization_with_synchronizer(
            control,
            directive,
            synchronizer,
            request_started,
            realization,
        )
        .await
        .map_err(Self::map_session_realization_drive_error)
    }

    /// Sole Runtime-to-contract effect path. The production entry and the
    /// test-only projection seam differ only in how they construct this adapter.
    pub(super) async fn drive_session_realization_with_synchronizer(
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        directive: awaken_session_realization_contract::SessionRealizationDirective,
        synchronizer: WorkerProjectionSynchronizer<'_>,
        request_started: awaken_runtime_contract::authority_lease::AuthorityProofStarted,
        realization: &crate::session_slot::SessionRealizationPermit,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationDriveError> {
        let host = synchronizer.host;
        awaken_session_application::drive_session_realization(
            realization,
            synchronizer.claim.map(|claim| claim.run_id.clone()),
            control,
            &synchronizer,
            &WorkerMcpEffects(host),
            directive,
            request_started,
        )
        .await
    }
}
