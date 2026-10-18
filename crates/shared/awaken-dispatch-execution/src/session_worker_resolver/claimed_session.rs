// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal entrypoint session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
// @formal entrypoint session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
// @formal entrypoint session_run.execution_requires_dispatch_work_and_realization_owner
// @formal entrypoint session_run.stale_work_epoch_cannot_observe_authoritative_thread_commit
//! Claimed Worker installation of one already-frozen Session projection.

use super::claim_failure::{SessionRealizationWorkerEffect, session_realization_worker_effect};
use super::{
    Arc, ClaimedExecution, ProjectionAuthority, SessionWorkerResolver, execution_error,
    map_admission_error, terminal_resolution_error,
};

/// Session projection authority is independent from the Run's admission shape.
/// Outcome Runs retain OrdinaryRoot identity, but the Coordinator's subordinate
/// Work receipt still requires the same frozen Session realization before effects.
pub fn has_session_projection_authority(claimed: &awaken_dispatch_contract::Claimed) -> bool {
    claimed.request.session_thread_id.is_some() || claimed.session_work_authority.is_some()
}

/// A co-located dispatch pool has no outbound Control client because its
/// Session application already drives the same Runtime Host. It may consume a
/// claimed Managed Run only after that application has installed the complete
/// frozen projection. Treating the Run's derived execution snapshot as a
/// substitute would create a second Agent-publication authority during restart.
pub const fn local_claimed_projection_is_ready(
    has_frozen_baseline: bool,
    session_dispatch: bool,
) -> bool {
    has_frozen_baseline && session_dispatch
}

pub fn map_claimed_session_control_error(
    run_id: &str,
    error: awaken_session_realization_contract::ClaimedSessionControlError,
) -> crate::Error {
    match session_realization_worker_effect(error.disposition()) {
        SessionRealizationWorkerEffect::Defer => crate::Error::ResolutionNotReady(
            format!("run {run_id} is waiting for its Session Environment Work slot").into(),
        ),
        SessionRealizationWorkerEffect::Relinquish => execution_error(format!(
            "run {run_id} frozen Session resume failed: {error}"
        )),
        SessionRealizationWorkerEffect::Absorb => terminal_resolution_error(format!(
            "run {run_id} frozen Session cannot resume: {error}"
        )),
    }
}

/// Compare the carried Run manifest with the one effect-capable Resource
/// transition authored by Control. Decoding through the projection's canonical
/// transition boundary keeps legacy validation and physical replay rules out of
/// the Worker transport adapter.
pub fn control_projection_matches_dispatched_manifest(
    projection: &awaken_session_realization_contract::FrozenSessionProjection,
    dispatched: &awaken_session_configuration_contract::resource::SessionResourceManifest,
) -> Result<bool, awaken_session_types::RunError> {
    Ok(projection
        .resource_transition(
            awaken_session_realization_contract::FrozenResourceTransitionUse::ApplyEffects,
        )?
        .desired()
        == dispatched)
}

/// A carried manifest is staged under the claim; a staging failure relinquishes it.
async fn install_dispatched_resources(
    installation: &dyn crate::session_execution::ClaimedSessionInstallation,
    session: &str,
    manifest: &awaken_session_configuration_contract::resource::SessionResourceManifest,
    claim: &awaken_dispatch_contract::RunClaim,
) -> Result<(), crate::Error> {
    installation
        .install_dispatched_resources(session, manifest, claim)
        .await
        .map_err(|error| execution_error(error.to_string()))
}

impl SessionWorkerResolver {
    /// Install the frozen Session projection under the authenticated Run claim.
    /// Session authoring is complete before WorkQueue dispatch; the Worker can
    /// realize committed truth but cannot contribute another desired-state input.
    pub(super) async fn install_claimed_session_projection(
        &self,
        execution: &ClaimedExecution,
        claimed: &awaken_dispatch_contract::Claimed,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        dispatched_resources: Option<
            &awaken_session_configuration_contract::resource::SessionResourceManifest,
        >,
    ) -> Result<(), crate::Error> {
        let claim = awaken_dispatch_contract::RunClaim::from(&claimed.lease);
        let installation = execution.installation.as_ref();
        // Read the prior exact claim/lease during the original atomic admission;
        // never relabel a predecessor before Control has accepted this claim.
        let (realization, continuation) = installation
            .acquire_claimed_realization(&thread_id.0, &claim)
            .await
            .map_err(map_admission_error)?;
        if !has_session_projection_authority(claimed) {
            if let Some(manifest) = dispatched_resources {
                install_dispatched_resources(installation, &thread_id.0, manifest, &claim).await?;
            } else if let Some(scope) = claimed.request.execution_scope.as_ref() {
                // Model-only ordinary Runs still belong to the frozen execution
                // Workspace; they need no fabricated Resource manifest to carry it.
                if !installation
                    .bind_execution_workspace(&thread_id.0, scope.0.as_str())
                    .map_err(map_admission_error)?
                {
                    return Err(execution_error(
                        "claimed Thread Workspace conflicts with its dispatch",
                    ));
                }
            }
            return Ok(());
        }
        let control = match &execution.projection {
            ProjectionAuthority::RemoteControl(control) => control.as_ref(),
            ProjectionAuthority::CoLocated => {
                let projection_ready = installation
                    .observe_projection(&thread_id.0)
                    .map_err(map_admission_error)?
                    .is_some_and(|observed| {
                        local_claimed_projection_is_ready(
                            observed.has_frozen_baseline,
                            observed.session_dispatch,
                        )
                    });
                if !projection_ready {
                    return Err(crate::Error::ResolutionNotReady(
                        format!(
                            "run {} is waiting for its co-located frozen Session projection",
                            claimed.lease.run_id().0
                        )
                        .into(),
                    ));
                }
                if let Some(manifest) = dispatched_resources {
                    install_dispatched_resources(installation, &thread_id.0, manifest, &claim)
                        .await?;
                }
                return Ok(());
            }
        };

        // Cause graph: concurrent attempts may address one Session, but only the
        // admitted Work/Run owner may advance its realization lease. Serializing
        // before the first Control mutation prevents a later entrant from fencing a
        // slow earlier effect between directive creation and installation.
        let dispatch: Arc<dyn awaken_dispatch_contract::DispatchClaimOwnership> =
            self.queue.clone();
        let ownership = crate::claim_bound_ownership_verifier(dispatch, claim.clone());
        ownership.verify_current().await.map_err(|error| {
            execution_error(format!(
                "run {} lost ownership before Session realization: {error}",
                claimed.lease.run_id().0
            ))
        })?;
        let request_started = awaken_authority_clock::authority_proof_started()
            .map_err(|error| execution_error(error.to_string()))?;
        let directive = control
            .resume_frozen(
                &claim,
                &thread_id.0,
                claimed.session_work_authority.as_ref(),
                continuation.as_ref(),
            )
            .await
            .map_err(|error| map_claimed_session_control_error(&claimed.lease.run_id().0, error))?
            .ok_or_else(|| {
                execution_error("WorkQueue dispatched a Session whose creation was not finalized")
            })?;
        if let Some(receipt) = claimed.session_work_authority.as_ref()
            && !receipt.binds_realization(
                &thread_id.0,
                &directive.authority.lease.runtime_incarnation,
                directive.projection.baseline.environment.self_hosted,
                &directive.projection.baseline.environment.environment_id,
            )
        {
            return Err(execution_error(
                "Session realization directive identity does not match claimed Work authority",
            ));
        }
        ownership.verify_current().await.map_err(|error| {
            execution_error(format!(
                "run {} lost ownership during frozen Session resume: {error}",
                claimed.lease.run_id().0
            ))
        })?;
        if let Some(dispatched) = dispatched_resources {
            let exact = control_projection_matches_dispatched_manifest(
                &directive.projection,
                dispatched,
            )
            .map_err(|error| {
                execution_error(format!(
                    "Control resume projection has no effect-capable Resource transition: {error}"
                ))
            })?;
            if !exact {
                // The claimed envelope is immutable. Relinquishing would claim
                // the same incompatible Run forever; use the existing fenced
                // terminal commit instead of replacing its resources on retry.
                return Err(terminal_resolution_error(
                    "Control resume projection conflicts with the claimed resource snapshot",
                ));
            }
        }
        execution
            .preparation
            .drive_claimed_projection(
                control,
                &awaken_session_types::SessionId::from(&thread_id.0),
                directive,
                claimed,
                request_started,
                &realization,
            )
            .await
    }
}
