// @formal entrypoint session_resources.install_replacement_requires_newer_or_unattempted_authority_amendment
// @formal entrypoint session_resources.terminal_assignment_retains_desired_generation
//! Complete frozen projection installation into the original installed Sessions.
//!
//! The registry owns correlated publication of frozen, realization and Run facts.
//! Physical validation, staging and retirement borrow the existing realization
//! executor and MCP lifecycle; this operation retains no execution capabilities.

use std::sync::Arc;

use awaken_dispatch_contract::RunClaim;
use awaken_session_configuration_contract::environment_projection;
use awaken_session_mcp::McpSessions;
use awaken_session_realization::peers::EnvironmentPeer as _;
use awaken_session_realization::realization_error::{AuthorityLoss, RealizationError};
use awaken_session_realization_contract::{
    CanonicalSessionProjection, FrozenProjectionReceipt, FrozenSessionProjection,
    ObservedSessionRealizationLease,
};
use awaken_session_types::SessionRuntimePlacement;

use crate::realization_executor::RealizationExecutor;
use crate::resource_staging::ResourcePreparation;

use super::InstalledSessions;
use super::resource_transition::{
    FrozenResourceProjectionMode, FrozenResourceTransitionInstallDecision,
    frozen_resource_transition_install_decision,
};

impl InstalledSessions {
    /// Install one complete frozen projection through its original physical
    /// collaborators; the registry remains the sole correlated-state owner.
    pub(crate) async fn install_frozen_session_projection(
        self: &Arc<Self>,
        physical: (&RealizationExecutor, &McpSessions),
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        claim: Option<&RunClaim>,
        synchronize_resources: bool,
        realization_lease: Option<
            awaken_session_realization_contract::ObservedSessionRealizationLease,
        >,
    ) -> Result<(), crate::SessionExecutionError> {
        self.install_frozen_session_projection_with_resource_authority(
            physical,
            thread,
            projection,
            claim,
            if synchronize_resources {
                FrozenResourceProjectionMode::Synchronize
            } else {
                FrozenResourceProjectionMode::ValidateInstalled
            },
            realization_lease,
        )
        .await
    }

    /// Install the Coordinator's complete frozen dispatch projection. This is
    /// the sole caller authorized to amend an unattempted same-revision Resource
    /// generation; Worker and local realization callers use the fenced method
    /// above.
    pub(crate) async fn install_dispatch_frozen_session_projection(
        self: &Arc<Self>,
        physical: (&RealizationExecutor, &McpSessions),
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
    ) -> Result<(), crate::SessionExecutionError> {
        self.install_frozen_session_projection_with_resource_authority(
            physical,
            thread,
            projection,
            None,
            FrozenResourceProjectionMode::Dispatch,
            None,
        )
        .await
    }

    async fn install_frozen_session_projection_with_resource_authority(
        self: &Arc<Self>,
        physical: (&RealizationExecutor, &McpSessions),
        thread: &str,
        projection: FrozenSessionProjection,
        claim: Option<&RunClaim>,
        resource_mode: FrozenResourceProjectionMode,
        realization_lease: Option<ObservedSessionRealizationLease>,
    ) -> Result<(), crate::SessionExecutionError> {
        let realization = physical.0;
        let exec = realization.environments();
        let sessions = self.realizations();
        if projection.baseline.fingerprint.0.trim().is_empty() {
            return Err(crate::SessionExecutionError::internal(
                "frozen Session baseline fingerprint must not be empty",
            ));
        }
        let has_mcp_projection = projection.mcp.iter().any(|attachment| {
            !matches!(
                attachment.state,
                crate::session_mcp::McpAttachmentState::Removed
                    | crate::session_mcp::McpAttachmentState::Failed
            )
        });
        let resource_transition = projection
            .resource_transition(resource_mode.transition_use())
            .map_err(crate::session_execution_error::from_run_error)?;
        let baseline = projection.baseline.clone();
        let init = projection.session_init();
        let environment_projection = environment_projection::project_environment(&init.environment);
        // Complete projection installation owns the Resource suffix lock from
        // the first comparison through publication of every correlated fact.
        // Desired-only staging and physical reconciliation therefore cannot
        // interleave a different resources/skills/manifest generation between
        // preflight and the baseline/transition/lease update below.
        let resource_projection = self
            .lock_frozen_projection_installation(realization, thread)
            .await?;
        let receipt = FrozenProjectionReceipt::from_projection(&projection)
            .map_err(crate::realization_error::RealizationError::from)?;
        sessions.update_realization(thread, |r| r.validate_projection_receipt(&receipt))??;
        let (publication, model_candidate) =
            exec.publication().resolve_canonical_session_projection(
                &projection.workspace_id,
                CanonicalSessionProjection::Baseline(&projection.baseline),
                projection.agent_publication.clone(),
                Some(projection.resources.skills()),
            )?;
        let frozen_inputs = sessions.frozen(thread)?;
        frozen_inputs
            .validate_publication(publication.as_ref())
            .map_err(|_| {
                crate::SessionExecutionError::internal(
                    "Session realization cannot replace its immutable Agent publication",
                )
            })?;
        frozen_inputs.validate_environment(thread, &environment_projection)?;
        // Validate the prospective complete layout before publishing any part of
        // a cold projection. The baseline is not resident yet on first install,
        // so a validator that reads only the current installed Session would miss a Repository
        // nested below one of its frozen mounts and could load Skills first. A
        // Worker-owned Dispatch is projection-only on the Coordinator: it uses
        // the same structural kernel without resolving a trusted-host provider
        // that only the claimed Worker may own. Every realization/cleanup path
        // and every Local dispatch retains the exact provider-effective check.
        if resource_mode == FrozenResourceProjectionMode::Dispatch
            && projection.baseline.runtime_placement == SessionRuntimePlacement::Worker
        {
            realization.staging().validate_structural_resource_layout(
                thread,
                &projection.resources,
                Some(&baseline.mounts),
                Some(&baseline.env),
                Some(&environment_projection),
            )?;
        } else {
            let provisioning = model_candidate.as_ref().map_or(
                &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                |candidate| candidate.provisioning(),
            );
            let provider = exec.session_environment_provider_for_thread(thread, provisioning)?;
            realization.staging().validate_resource_layout(
                thread,
                &projection.resources,
                provider,
                Some(&baseline.mounts),
                Some(&baseline.env),
                Some(&environment_projection),
            )?;
        }

        let baseline_to_install = if let Some(existing) = frozen_inputs.baseline.as_ref() {
            if existing.fingerprint != baseline.fingerprint {
                return Err(crate::SessionExecutionError::internal(format!(
                    "thread {thread} is already bound to a different frozen Session baseline"
                )));
            }
            None
        } else {
            let is_realized = self
                .read(thread, |installed| {
                    installed
                        .run
                        .context
                        .as_ref()
                        .is_some_and(|context| !context.is_coordination_envelope())
                        || installed
                            .realization
                            .environment()
                            .owner
                            .has_local_environment()
                })?
                .unwrap_or(false);
            if is_realized {
                return Err(crate::SessionExecutionError::internal(format!(
                    "thread {thread} was realized before its frozen Session baseline"
                )));
            }
            Some(baseline)
        };

        if baseline_to_install.is_some()
            && matches!(
                resource_mode,
                FrozenResourceProjectionMode::ValidateInstalled
            )
            && projection.resource_revision > 0
        {
            return Err(crate::SessionExecutionError::internal(format!(
                "thread {thread} cannot cold-materialize frozen Session Resources during lease-only renewal"
            )));
        }
        let transition_install = sessions
            .read_realization(thread, |r| {
                frozen_resource_transition_install_decision(
                    r.staging().transition.as_ref(),
                    &resource_transition,
                    resource_mode.allows_unattempted_amendment(),
                )
            })?
            .unwrap_or(FrozenResourceTransitionInstallDecision::Stage);
        if transition_install == FrozenResourceTransitionInstallDecision::Reject {
            let existing = sessions
                .read_realization(thread, |r| r.staging().transition.clone())?
                .flatten();
            let existing_revision = existing
                .as_ref()
                .map(|transition| transition.desired().revision);
            let incoming_previous_matches = existing
                .as_ref()
                .is_some_and(|existing| existing.desired() == resource_transition.previous());
            return Err(crate::SessionExecutionError::internal(format!(
                "thread {thread} cannot replace its current Session Resource projection \
                 (mode {resource_mode:?}, existing desired revision {existing_revision:?}, \
                 incoming {}->{}, previous matches existing desired: {incoming_previous_matches})",
                resource_transition.previous().revision,
                resource_transition.desired().revision,
            )));
        }
        // A child may commit Resource revision R before the parent resumes;
        // the frozen transition can then be R->R even though this Worker's
        // revoked physical wrapper still holds R-1. The root receipt and the
        // owner kernel, rather than the transition delta alone, govern whether
        // that exact monotonic reservation may be re-adopted.
        let pending_resource_reservation =
            resource_mode.allows_revoked_owner_reservation(projection.resource_revision);
        // Resource installation owns generation equality and fencing. Claimed
        // replay revalidates the exact aggregate transition. Requirements are
        // staged here, but only the physical transition body may publish the
        // active manifest.
        #[cfg(target_os = "linux")]
        if let Some(proof) = realization_lease.as_ref() {
            // Reject a foreign durable owner BEFORE retiring a valid execution
            // object. Publication still revalidates this same canonical kernel.
            self.install_with(thread, |installed| {
                let newer = installed
                    .realization
                    .validate_projection_receipt(&receipt)?;
                installed.realization.environment().owner.after_projection(
                    thread,
                    &projection.workspace_id,
                    &projection.environment,
                    newer,
                    pending_resource_reservation,
                )?;
                Ok::<_, crate::SessionExecutionError>(())
            })??;
            let mcp = (physical.1, self.as_ref());
            exec.retire_replaced_scoped_execution(realization.peers(), thread, proof, mcp)
                .await?;
        }
        let preparation = if resource_mode.synchronizes() && projection.resource_revision > 0 {
            realization_lease
                .as_ref()
                .map(|lease| {
                    ResourcePreparation::begin(realization.authority(), thread, lease.clone())
                })
                .transpose()?
        } else {
            None
        };
        if resource_mode.synchronizes() && projection.resource_revision > 0 {
            let stage = realization
                .staging()
                .stage_prevalidated_resource_transition_under_resource_projection(
                    resource_projection.held(),
                    realization.peers(),
                    thread,
                    &resource_transition,
                    claim,
                    realization_lease.as_ref().map(|proof| &proof.lease),
                    projection.environment.binding().is_some(),
                );
            tokio::select! {
                biased;
                error = async {
                    match &preparation {
                        Some(preparation) => preparation.authority_ended().await,
                        None => std::future::pending().await,
                    }
                } => return Err(error.into()),
                result = stage => result.map_err(crate::session_execution_error::from_run_error)?,
            }
        }
        if let Some(preparation) = &preparation {
            preparation.validate()?;
        }
        realization.authority().authorize_worker_effect(|| {
            self.install_with(thread, |installed| {
                // Revalidate fallible coordinates and stage the existing owner
                // transition under the final installed Session lock. No correlated field is
                // published before the time proof/preparation is admitted.
                installed
                    .inputs
                    .validate_publication(publication.as_ref())?;
                installed
                    .inputs
                    .validate_environment(thread, &environment_projection)?;
                let newer = installed
                    .realization
                    .validate_projection_receipt(&receipt)?;
                let owner = installed.realization.environment().owner.after_projection(
                    thread,
                    &projection.workspace_id,
                    &projection.environment,
                    newer,
                    pending_resource_reservation,
                )?;
                if let Some(preparation) = &preparation {
                    preparation.finish(&mut installed.realization)?;
                } else if let Some(lease) = realization_lease {
                    if !installed.realization.install_lease(lease) {
                        return Err(crate::SessionExecutionError::internal(
                            "Session realization time proof is not current",
                        ));
                    }
                    installed.realization.execution().changed.notify_waiters();
                }
                // This suffix is infallible and holds both Worker authority and
                // the installed Session lock: revoke/replacement cannot interleave a partial
                // publication between the check above and these writes.
                if let Some(claim) = claim {
                    installed.run.dispatch_claim = Some(claim.clone());
                }
                installed.realization.publish_frozen_projection(
                    owner,
                    &projection.environment,
                    installed.run.context.is_some(),
                    receipt,
                    has_mcp_projection,
                    resource_transition,
                );
                // A cold Coordinator may have opened a metadata-only
                // reservation envelope before this complete projection.
                // Rebuild it against the newly frozen baseline on its next
                // read; it has no executable Environment to preserve.
                if baseline_to_install.is_some() {
                    installed.run.drop_coordination_envelope();
                }
                // One complete next value replaces the frozen inputs. The
                // validated publication and an installed baseline are kept.
                installed.replace_frozen_with(|next| {
                    next.lower_session_init(&init, environment_projection);
                    if let Some(publication) = publication {
                        next.published_snapshot = Some(publication);
                    }
                    if let Some(baseline) = baseline_to_install {
                        next.baseline = Some(baseline);
                    }
                    next.request_context = projection.request_context.clone();
                });
                // The complete frozen projection is the cold Worker's first
                // observation of Session admission. Publish the existing execution
                // marker in the same atomic installed Session update as its baseline and
                // transition; local admission may set it earlier, while revocation
                // remains authorized to clear it with the rest of live execution
                // state.
                installed.run.session_dispatch = true;
                Ok::<_, crate::SessionExecutionError>(())
            })??;
            Ok(())
        })
    }

    /// Enter the original install path and require the physical executor to
    /// observe this same current registration before any of its effects.
    /// The phase Arc is admission identity, not another lease or source owner.
    async fn lock_frozen_projection_installation(
        self: &Arc<Self>,
        realization: &RealizationExecutor,
        thread: &str,
    ) -> Result<crate::session_realizations::ResourceProjectionGuard, RealizationError> {
        self.install(thread)?;
        let resource_projection = self.realizations().lock_resource_projection(thread).await?;
        let observed = realization
            .sessions()
            .installed_projection(thread)?
            .ok_or_else(|| RealizationError::session_absent(thread))?;
        // A decorated source may remove or replace the actual registration
        // after returning its lookup. Re-read the canonical registry last.
        let current = self
            .read(thread, |installed| {
                installed.realization.execution().phase.clone()
            })?
            .ok_or_else(|| RealizationError::session_absent(thread))?;
        if !Arc::ptr_eq(&current, &observed.installation) {
            return Err(RealizationError::authority_lost(
                AuthorityLoss::GenerationChanged,
                "projection installer and realization executor observe different Session registrations",
            ));
        }
        Ok(resource_projection)
    }

    /// Terminal effect fixtures supply an already-received assignment. Actual
    /// transport-latency tests call the production entrance with their explicit
    /// request-start observation instead of this convenience adapter.
    #[cfg(test)]
    pub(crate) async fn install_test_terminal_cleanup_projection(
        self: &Arc<Self>,
        physical: (&RealizationExecutor, &McpSessions),
        assignment: &awaken_session_realization_contract::SessionTerminalCleanupAssignment,
    ) -> Result<(), crate::SessionExecutionError> {
        let started = awaken_authority_clock::authority_proof_started().unwrap();
        self.install_terminal_cleanup_projection(
            physical,
            assignment,
            assignment.authority.clone().observed(started),
        )
        .await
    }

    /// Install one aggregate-frozen terminal assignment without entering the
    /// active realization driver. Cleanup I/O remains owned by the terminal
    /// effect path.
    pub(crate) async fn install_terminal_cleanup_projection(
        self: &Arc<Self>,
        physical: (&RealizationExecutor, &McpSessions),
        assignment: &awaken_session_realization_contract::SessionTerminalCleanupAssignment,
        proof: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), crate::SessionExecutionError> {
        let realizations = self.realizations();
        if proof.lease != assignment.authority.lease {
            return Err(crate::SessionExecutionError::internal(
                "terminal assignment time proof names another lease",
            ));
        }
        let _realization = self
            .acquire_realization_with(assignment.session_id.as_str(), |_| ())
            .await?
            .0;
        self.install_frozen_session_projection_with_resource_authority(
            physical,
            assignment.session_id.as_str(),
            assignment.projection.clone(),
            None,
            FrozenResourceProjectionMode::Terminal,
            Some(proof),
        )
        .await?;
        realizations.update_realization(assignment.session_id.as_str(), |r| {
            r.assign_terminal_cleanup(assignment.clone());
        })?;
        Ok(())
    }
}
