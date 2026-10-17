// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Two-stage terminal cleanup orchestration and process-local acknowledgement.
//!
//! Durable phase authority remains in the Session aggregate. This private split
//! executes its exact Preparation and Disposal effects and retires only the
//! corresponding Worker projection after Control accepts the receipt.

use super::TerminalEnvironmentPreparation;
use crate::environment_owner::{
    RetiringEnvironmentOwner, SessionEnvironmentOwner, SessionEnvironmentPreparation,
    SessionEnvironmentRetirementCause,
};
use crate::realization_error::RealizationError;
use crate::realization_error::{AuthorityLoss, PendingState, ProjectionConflict};
use crate::session_environment::BoundEnvironmentPreparationMode;
use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_state_contract::SessionCleanupDisposalReceipt;

impl crate::terminal_effects::TerminalEffects {
    /// Complete every source-dependent participant under one exact aggregate
    /// preparation fence without deleting the physical realization. The pure
    /// Memory evidence join precedes MCP, Artifact, Memory, checkpoint, and
    /// provider preparation in that order; any failure retains the substrate
    /// and returns no durable preparation receipt.
    pub async fn prepare_terminal_cleanup_effect<D, DF, B, BF, S>(
        &self,
        peers: crate::peers::Peers<'_>,
        effect: awaken_session_state_contract::SessionTerminalCleanupEffect,
        authorization: awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
        delegated: D,
        quiesce_background: B,
        mcp: (&awaken_session_mcp::McpSessions, &S),
    ) -> Result<awaken_session_state_contract::SessionCleanupPreparation, RealizationError>
    where
        D: FnOnce(String) -> DF + Send,
        DF: std::future::Future<
                Output = Result<
                    Vec<awaken_session_application_contract::DelegatedRun>,
                    RealizationError,
                >,
            > + Send,
        B: FnOnce(String, String) -> BF + Send,
        BF: std::future::Future<Output = bool> + Send,
        S: awaken_session_mcp::McpSessionSlots,
    {
        let cleanup = self;
        let sessions = &cleanup.sessions;
        authorization.verify_for(&effect).map_err(|error| {
            RealizationError::authority_lost(
                AuthorityLoss::CleanupPreparationUnauthorized,
                error.to_string(),
            )
        })?;
        let workspace_id = self
            .sessions
            .frozen(effect.command.session_id.as_str())?
            .workspace
            .clone()
            .ok_or_else(|| {
                RealizationError::pending(
                    PendingState::TerminalCleanupWorkspaceMissing,
                    "terminal cleanup has no installed Workspace projection",
                )
            })?;
        if workspace_id != authorization.workspace_id() {
            return Err(RealizationError::conflict(
                ProjectionConflict::TerminalCleanupWorkspaceMismatch,
                "terminal cleanup authorization belongs to another Workspace",
            ));
        }
        let root_realization = sessions
            .read_realization(effect.command.session_id.as_str(), |r| {
                r.execution.phase.clone()
            })?
            .ok_or_else(|| {
                RealizationError::pending(
                    PendingState::TerminalCleanupProjectionMissing,
                    "terminal cleanup has no installed root projection",
                )
            })?;
        let _root_realization = root_realization.lock().await;
        // A cold child has no lifecycle here, and cleanup never installs one.
        let _lifecycle = sessions
            .lock_lifecycle_if_installed(&effect.command.thread_id)
            .await?;
        let effect_fence = self.effect_fence(&effect)?;

        let root_environment = if effect.command.thread_id == effect.command.session_id {
            let mut prepared = self
                .prepare_terminal_environment_under_lifecycle(
                    peers,
                    &effect.command.thread_id,
                    &effect_fence,
                    BoundEnvironmentPreparationMode::Terminal,
                )
                .await?;
            prepared.environment = prepared
                .environment
                .with_preparation_authorization(&authorization);
            Some(prepared)
        } else {
            None
        };
        let expected_restore = root_environment.as_ref().and_then(|prepared| {
            prepared
                .state
                .restoring_request(&workspace_id, &effect.command.session_id)
        });
        authorization
            .verify_runtime_source(
                expected_restore.as_ref(),
                root_environment.as_ref().is_none_or(|root| {
                    root.environment.permits_live_io() && root.environment.environment.is_some()
                }),
            )
            .map_err(RealizationError::Run)?;
        let memory_plan = root_environment
            .as_ref()
            .map(|root| cleanup.memory_plan(&effect, &root.environment))
            .transpose()?;

        sessions.modify_realization(&effect.command.thread_id, |r| {
            r.effects.root = Some(effect.command.session_id.to_string());
        })?;

        if crate::session_realizations::primary_or_delegated_run_active(
            &effect.command.thread_id,
            |session| {
                Ok(sessions
                    .modify_session(session, |slot| slot.run.has_active_run())?
                    .unwrap_or(false))
            },
            delegated(effect.command.thread_id.to_string()),
        )
        .await?
        {
            return Err(RealizationError::pending(
                PendingState::TerminalEnvironmentNotQuiescent,
                "Primary or delegated work still owns the terminal Environment",
            ));
        }
        if let Some(owned) = sessions
            .read_realization(&effect.command.thread_id, |r| {
                r.environment.owner.terminal_bound_environment()
            })?
            .flatten()
        {
            let generation_id = owned.activity_generation_id();
            if !quiesce_background(
                effect.command.thread_id.to_string(),
                generation_id.to_string(),
            )
            .await
            {
                return Err(RealizationError::pending(
                    PendingState::TerminalBackgroundNotQuiescent,
                    "Shared-Environment background work did not reach a durable boundary",
                ));
            }
            let retained = sessions
                .read_realization(&effect.command.thread_id, |r| {
                    let current = r.environment.owner.terminal_bound_environment();
                    current.is_some_and(|current| current.exact_matches(&owned))
                })?
                .unwrap_or(false);
            if !retained {
                return Err(RealizationError::authority_lost(
                    AuthorityLoss::TerminalOwnerChanged,
                    "terminal Environment owner changed while background work quiesced",
                ));
            }
        }
        self.effect_fence(&effect)?;
        mcp.0
            .drain_projections(mcp.1, &effect.command.thread_id, &[])
            .await
            .map_err(RealizationError::from)
            .map_err(|error| {
                RealizationError::pending(PendingState::TerminalMcpNotQuiescent, error.to_string())
            })?;
        if let Some((environment, _)) = root_environment
            .as_ref()
            .filter(|prepared| prepared.environment.permits_live_io())
            .and_then(|prepared| prepared.environment.environment.as_ref())
        {
            environment
                .lifecycle()
                .stop_bound_processes()
                .await
                .map_err(|error| {
                    RealizationError::pending(
                        PendingState::TerminalHandNotQuiescent,
                        error.to_string(),
                    )
                })?;
        }
        self.effect_fence(&effect)?;
        let artifact_capture_mode = root_environment
            .as_ref()
            .map(TerminalEnvironmentPreparation::artifact_capture_mode)
            .unwrap_or(Some(crate::artifact_harvest::ArtifactCaptureMode::Live));
        let artifact_receipts = if let Some(capture_mode) = artifact_capture_mode {
            let harvester = self.artifacts.clone();
            let fence = Some(
                awaken_session_realization_contract::ResourceOperationFence::Terminal(
                    effect.clone(),
                ),
            );
            // Live root preparation has already moved the exact owner behind
            // the Retiring fence, so the generic Resident lookup cannot recover
            // it. Pass that same prepared Arc through the canonical harvester;
            // receipt-only, child, and environment-absent rows retain the
            // existing mode-owned path.
            let harvested = match (
                capture_mode,
                root_environment
                    .as_ref()
                    .filter(|prepared| prepared.environment.permits_live_io())
                    .and_then(|prepared| prepared.environment.environment.as_ref()),
            ) {
                (crate::artifact_harvest::ArtifactCaptureMode::Live, Some((environment, _))) => {
                    harvester
                        .harvest_with_environment_and_fence(
                            &effect.command.thread_id,
                            fence,
                            environment,
                        )
                        .await
                }
                (capture_mode, _) => {
                    harvester
                        .harvest_with_fence_mode(&effect.command.thread_id, fence, capture_mode)
                        .await
                }
            };
            harvested
                .map_err(|error| {
                    RealizationError::pending(
                        PendingState::TerminalArtifactHarvestPending,
                        error.to_string(),
                    )
                })?
                .receipts
        } else {
            Vec::new()
        };
        let mut provider_prepared_effect_fence = None;
        if let Some(prepared) = root_environment.as_ref() {
            let effect_mode = prepared.environment.effect_mode;
            let environment = prepared.environment.environment.as_ref();
            if let Some((environment, _)) = environment.as_ref() {
                self.effect_fence(&effect)?;
                self.reconcile_terminal_memory(
                    peers,
                    &effect,
                    environment.as_ref(),
                    effect_mode,
                    memory_plan.as_ref().ok_or_else(|| {
                        RealizationError::invariant("terminal root has no pure Memory plan")
                    })?,
                )
                .await?;
            }
            // A committed checkpoint is independent durable state and can be
            // deleted without reading the Sandbox. Upload-response recovery
            // remains on the checkpoint port's existing terminal edge. Neither
            // case is a reason to skip cleanup after a provider Disposing gate.
            self.cleanup_terminal_checkpoint(
                peers,
                &effect,
                &prepared.state,
                prepared.pending_checkpoint.as_ref(),
                environment
                    .as_ref()
                    .map(|(environment, _)| environment.as_ref()),
            )
            .await?;
            if prepared.environment.requires_provider_preparation()
                && let Some((environment, _)) = environment
            {
                let provider_effect_fence = self.effect_fence(&effect)?;
                let authority = peers.authority.provider_effect_authority(
                    effect.command.session_id.as_str(),
                    &provider_effect_fence,
                )?;
                provider_prepared_effect_fence = Some(
                    environment
                        .lifecycle()
                        .prepare_disposal_for_effect(&provider_effect_fence, &authority)
                        .await
                        .map_err(RealizationError::terminal_effect_pending)?,
                );
            }
        }

        // Protect even a no-Environment preparation from reporting success
        // after the terminal owner changed while source effects were in flight.
        let final_effect_fence = self.effect_fence(&effect)?;
        awaken_session_state_contract::SessionCleanupPreparation::try_new(
            &effect,
            provider_prepared_effect_fence.unwrap_or(final_effect_fence),
            artifact_receipts,
        )
        .map_err(|error| {
            RealizationError::pending(
                PendingState::TerminalCleanupProviderPreparationInvalid,
                error.to_string(),
            )
        })
    }

    /// Execute only the aggregate-wide destructive half after every exact
    /// preparation is durable. Effect-free provider observation/reconstruction
    /// may recover the physical owner; Hand, Artifact, Memory, checkpoint, and
    /// provider preparation are forbidden on this path.
    pub async fn dispose_terminal_cleanup_effect(
        &self,
        peers: crate::peers::Peers<'_>,
        effect: awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<SessionCleanupDisposalReceipt, RealizationError> {
        let environments = peers.environments;
        let realizer = environments;
        let sessions = &self.sessions;
        let session_id = &effect.command.session_id;
        let root_realization = sessions
            .read_realization(session_id.as_str(), |r| r.execution.phase.clone())?
            .ok_or_else(|| {
                RealizationError::pending(
                    PendingState::TerminalCleanupProjectionMissing,
                    "terminal cleanup has no installed root projection",
                )
            })?;
        let _root_realization = root_realization.lock().await;
        self.disposal_authorization(&effect)?;
        let _lifecycle = sessions.lock_lifecycle(session_id.as_str()).await?;
        let authorization = self.disposal_authorization(&effect)?;
        let prepared_environment = if effect.command.restore_target.is_none() {
            // Disposal decision table TD0. Causes: C1 the aggregate has
            // durably admitted one exact Preparation; C2 the process-local
            // terminal owner is retained, cold, or provider-proved absent; C3
            // the current successor fence is live. Effects: E1 re-observe the
            // exact physical identity with zero Artifact/Memory/checkpoint/Hand
            // work; E2 retain an existing terminal Retiring owner, reconstruct
            // a cold owner, or preserve exact absence; E3 only the typed
            // Disposal authorization may delete. Rules: retained=>E1+E3;
            // cold=>E1+E2+E3; absent=>E1+E2 without a guessed delete.
            Some(
                self.prepare_terminal_environment_under_lifecycle(
                    peers,
                    session_id.as_str(),
                    authorization.effect_fence(),
                    BoundEnvironmentPreparationMode::TerminalDisposal,
                )
                .await?,
            )
        } else {
            None
        };
        let provider_proved_absence = prepared_environment
            .as_ref()
            .is_some_and(|prepared| prepared.environment.environment.is_none());
        let owner = sessions
            .read_realization(session_id.as_str(), |r| r.environment.owner.clone())?
            .ok_or_else(|| {
                RealizationError::pending(
                    PendingState::TerminalCleanupProjectionMissing,
                    "terminal disposal lost its Environment owner projection",
                )
            })?;
        match (owner, effect.command.restore_target.as_ref()) {
            (
                SessionEnvironmentOwner::Restoring(
                    crate::environment_owner::SessionEnvironmentRestoration::Awaiting { request },
                ),
                Some(expected),
            ) if &request == expected => {
                let provider =
                    realizer.projected_session_environment_provider(session_id.as_str(), None)?;
                let previous_resources = sessions
                    .read_realization(session_id.as_str(), |r| r.previous_resources())?
                    .flatten();
                let spec = realizer.session_environment_adoption_spec(
                    peers,
                    session_id.as_str(),
                    provider,
                    previous_resources.as_ref(),
                )?;
                let authority = peers
                    .authority
                    .provider_effect_authority(session_id.as_str(), authorization.effect_fence())?;
                provider
                    .terminal()
                    .dispose_restored(&spec, expected, &authorization, &authority)
                    .await
                    .map_err(RealizationError::terminal_effect_pending)?;
                awaken_provisioning_contract::with_current_sandbox_effect(
                    authorization.effect_fence(),
                    &authority,
                    |_| Ok(()),
                )
                .map_err(RealizationError::terminal_effect_pending)?;
                // Keep the exact Restoring request as the response-loss fence.
                // The provider port treats proved absence as success, so an
                // identical Disposal can replay until aggregate acknowledgement
                // retires the whole terminal projection.
            }
            (SessionEnvironmentOwner::Restoring(_), _) | (_, Some(_)) => {
                return Err(RealizationError::authority_lost(
                    AuthorityLoss::RestoreTargetChanged,
                    "terminal disposal does not match its exact Restoring owner",
                ));
            }
            (SessionEnvironmentOwner::Retiring(retirement), None) => {
                // Bound continuation takeover may inherit provider predecessor A
                // while its terminal owner is T. Unbound reconstruction has no
                // such durable continuation and must name its exact terminal
                // Preparation or this cold Disposal; a foreign cause is not proof.
                if !matches!(
                    &retirement.cause,
                    SessionEnvironmentRetirementCause::Terminal { effect_id }
                        if matches!(retirement.owned, RetiringEnvironmentOwner::Bound(_))
                            || effect_id == authorization.prepared_effect_fence().operation_id()
                            || effect_id == authorization.effect_fence().operation_id()
                ) {
                    return Err(RealizationError::authority_lost(
                        AuthorityLoss::TerminalPreparationChanged,
                        "terminal disposal found another retirement operation",
                    ));
                }
                // Preparation may own an Unbound Candidate whose ordinary
                // Create response never arrived. The separately authorized
                // aggregate Disposal, not a forged Bound identity, permits
                // deletion of that same exact terminal physical owner.
                if !provider_proved_absence {
                    let environment = retirement.owned.environment();
                    let exact_authorization = self.disposal_authorization(&effect)?;
                    if exact_authorization != authorization {
                        return Err(RealizationError::authority_lost(
                            AuthorityLoss::CleanupDisposalChanged,
                            "terminal disposal authorization changed under its lifecycle lock",
                        ));
                    }
                    let authority = peers.authority.provider_effect_authority(
                        effect.command.session_id.as_str(),
                        authorization.effect_fence(),
                    )?;
                    environment
                        .lifecycle()
                        .dispose_for_effect(&authorization, &authority)
                        .await
                        .map_err(RealizationError::terminal_effect_pending)?;
                }
            }
            (
                SessionEnvironmentOwner::Vacant
                | SessionEnvironmentOwner::Preparing(
                    SessionEnvironmentPreparation::AwaitingAdoption { .. },
                ),
                None,
            ) => {
                // Preparation already proved that this exact durable owner has
                // no remaining physical realization. The aggregate receipt,
                // not a guessed delete, is the terminal boundary.
            }
            (SessionEnvironmentOwner::Resident(_), None)
            | (
                SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::Candidate(_)),
                None,
            ) => {
                return Err(RealizationError::pending(
                    PendingState::TerminalEnvironmentNotPrepared,
                    "physical disposal requires the exact retained Preparation owner",
                ));
            }
        }
        sessions.end_terminal_disposal(session_id.as_str())?;
        self.disposal_authorization(&effect)?;
        Ok(awaken_session_state_contract::SessionCleanupDisposalReceipt::new(&effect.command))
    }

    /// Retire only a prepared child after Control durably accepts its receipt.
    /// The root owns the shared Environment and must remain until disposal.
    pub async fn acknowledge_terminal_cleanup_preparation(
        &self,
        mcp: &awaken_session_mcp::McpSessions,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) {
        if effect.command.thread_id == effect.command.session_id {
            return;
        }
        // Retirement only forgets a local projection Control already settled;
        // a denied retirement leaves it for the next acknowledgement replay.
        if let Err(error) = self
            .retire_terminal_projection_for_generation(
                mcp,
                effect.command.session_id.as_str(),
                &effect.lease,
                Some(&effect.command.thread_id),
            )
            .await
        {
            tracing::warn!(%error, "terminal cleanup child projection retained");
        }
    }

    /// Forget the exact root teardown projection only after Control durably
    /// accepts the physical-disposal receipt. This edge performs no I/O.
    pub async fn acknowledge_terminal_cleanup_disposal(
        &self,
        mcp: &awaken_session_mcp::McpSessions,
        effect: &awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) {
        if let Err(error) = self
            .retire_terminal_projection_for_generation(
                mcp,
                effect.command.session_id.as_str(),
                &effect.lease,
                None,
            )
            .await
        {
            tracing::warn!(%error, "terminal cleanup root projection retained");
        }
    }

    /// Retire a retained terminal projection after Control reports that the
    /// durable cleanup operation is already complete. This is the response-loss
    /// readback of the same process-local acknowledgement above, not a second
    /// cleanup command or receipt source.
    pub async fn retire_completed_terminal_cleanup_projection(
        &self,
        mcp: &awaken_session_mcp::McpSessions,
        session_id: &str,
        lease: &session_resource::SessionRealizationLease,
    ) -> Result<bool, RealizationError> {
        let realizations = self.sessions.clone();
        let projected_terminal = realizations
            .read_realization(session_id, |r| r.effects.assignment.is_some())?
            .unwrap_or(false);
        if !projected_terminal {
            return Ok(false);
        }
        self.retire_terminal_projection_for_generation(mcp, session_id, lease, None)
            .await
    }

    async fn retire_terminal_projection_for_generation(
        &self,
        mcp: &awaken_session_mcp::McpSessions,
        session_id: &str,
        lease: &session_resource::SessionRealizationLease,
        target: Option<&str>,
    ) -> Result<bool, RealizationError> {
        let realizations = self.sessions.clone();
        let Some(root_realization) =
            realizations.read_realization(session_id, |r| r.execution.phase.clone())?
        else {
            return Ok(false);
        };
        let _root_realization = root_realization.lock().await;
        let authorized = realizations
            .read_realization(session_id, |r| {
                r.execution.authority.lease().is_some_and(|current| {
                    awaken_session_realization_contract::realization_lease_generation_authorizes(
                        current, lease,
                    )
                })
            })?
            .unwrap_or(false);
        if !authorized {
            return Ok(false);
        }

        let mut targets = Vec::new();
        if target.is_none() || target == Some(session_id) {
            for thread_id in self.sessions.session_ids()? {
                if thread_id == session_id
                    || realizations
                        .read_realization(&thread_id, |r| {
                            r.effects.root.as_deref() == Some(session_id)
                        })?
                        .unwrap_or(false)
                {
                    targets.push(thread_id);
                }
            }
        } else {
            targets.extend(target.map(str::to_owned));
        }
        // Remove the root last so its realization generation remains available
        // as the exact fence while every child projection is retired.
        targets.sort_by_key(|thread_id| thread_id == session_id);
        let mut removed = false;
        for thread_id in targets {
            let target_lifecycle =
                realizations.read_realization(&thread_id, |r| r.environment.lifecycle.clone())?;
            let _target_lifecycle = match target_lifecycle {
                Some(lifecycle) => Some(lifecycle.lock_owned().await),
                None => None,
            };
            let authorized = realizations
                .read_realization(session_id, |r| {
                    r.execution.authority.lease().is_some_and(|current| {
                        awaken_session_realization_contract::realization_lease_generation_authorizes(
                            current, lease,
                        )
                    })
                })?
                .unwrap_or(false);
            if !authorized {
                return Ok(removed);
            }
            let belongs_to_operation = thread_id == session_id
                || realizations
                    .read_realization(&thread_id, |r| {
                        r.effects.root.as_deref() == Some(session_id)
                    })?
                    .unwrap_or(false);
            if !belongs_to_operation {
                continue;
            }
            mcp.remove_relay_routes(&thread_id);
            removed |= self.sessions.remove_session(&thread_id)?;
        }
        Ok(removed)
    }
}
