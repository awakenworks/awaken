// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Worker Runtime's existing realization entry moved from the adjacent parent module. It
// @formal reason: obtains the Session's realization permit through the canonical installed Session accessor, then
// @formal reason: delegates the directive and effects to the application-owned phase driver; it creates no
// @formal reason: second phase authority. This classifies the production call path against Session
// @formal reason: lifecycle, not as a direct proof of scheduler or persistence behavior.
//! The single entry from Worker authority into the Session realization driver.

use awaken_session_realization::worker_projection::{
    SessionProjectionExecution, WorkerMcpEffects, WorkerProjectionSynchronizer,
};

pub(crate) async fn realize_session(
    host: &crate::RunAssembly,
    control: &dyn awaken_session_execution_contract::SessionRealizationControl,
    session_id: &awaken_session_types::SessionId,
    directive: awaken_session_execution_contract::SessionRealizationDirective,
    claim: Option<&awaken_dispatch_contract::RunClaim>,
    published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    rebuild_unavailable_environment: bool,
) -> Result<(), awaken_session_execution_contract::SessionRealizationDriveError> {
    let realization = host
        .sessions()
        .acquire_realization(session_id.as_str())
        .await
        .map_err(awaken_session_types::RunError::from)?;
    let synchronizer = WorkerProjectionSynchronizer::for_directive(
        host,
        &directive,
        claim,
        published_snapshot,
        rebuild_unavailable_environment,
        None,
    );
    drive_session_realization_with_synchronizer(
        control,
        directive,
        synchronizer,
        awaken_authority_clock::authority_proof_started().map_err(|error| {
            awaken_session_execution_contract::SessionRealizationDriveError::Effect(
                awaken_session_types::RunError::unavailable(error.to_string()).into(),
            )
        })?,
        &realization,
    )
    .await
}

/// Drive one already-serialized directive under the Session's realization
/// lock so no caller can create a parallel phase driver.
pub(crate) async fn drive_session_realization(
    host: &crate::RunAssembly,
    control: &dyn awaken_session_execution_contract::SessionRealizationControl,
    session_id: &awaken_session_types::SessionId,
    directive: awaken_session_execution_contract::SessionRealizationDirective,
    claimed: Option<&awaken_dispatch_contract::Claimed>,
    request_started: awaken_lease_contract::authority::AuthorityProofStarted,
    realization: &awaken_session_execution_contract::SessionRealizationPermit,
) -> Result<(), awaken_session_execution_contract::SessionRealizationDriveError> {
    let claim = claimed.map(|claimed| awaken_dispatch_contract::RunClaim::from(&claimed.lease));
    let synchronizer = WorkerProjectionSynchronizer::for_claimed(
        host,
        session_id.as_str(),
        &directive,
        claimed,
        claim.as_ref(),
    );
    drive_session_realization_with_synchronizer(
        control,
        directive,
        synchronizer,
        request_started,
        realization,
    )
    .await
}

/// Sole Runtime-to-contract effect path. The production entry and the
/// test-only projection seam differ only in how they construct this adapter.
pub(crate) async fn drive_session_realization_with_synchronizer(
    control: &dyn awaken_session_execution_contract::SessionRealizationControl,
    directive: awaken_session_execution_contract::SessionRealizationDirective,
    synchronizer: WorkerProjectionSynchronizer<'_, crate::RunAssembly>,
    request_started: awaken_lease_contract::authority::AuthorityProofStarted,
    realization: &awaken_session_execution_contract::SessionRealizationPermit,
) -> Result<(), awaken_session_execution_contract::SessionRealizationDriveError> {
    let host = synchronizer.execution;
    awaken_session_realization_driver::drive_session_realization(
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
