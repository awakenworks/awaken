// @formal evidence session_environment.owner_topology_is_exact_and_frozen
// @formal evidence session_environment.source_disposal_requires_a_verified_checkpoint
// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Aggregate-authorized terminal Environment preparation substrate.
//!
//! This private split retains the parent module's sole `SessionEnvironmentOwner`;
//! it only projects exact leases, provider observations, Memory evidence, and
//! checkpoint work into the existing two-stage terminal protocol.

use super::*;
use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_state_contract::{SessionEnvironmentState, SuspendPhase};

impl SharedHost {
    /// Take over a restore that has no aggregate binding yet. The frozen
    /// restore fence is identity evidence only; the live terminal fence is the
    /// sole mutation authority. A hot process-local owner must carry the exact
    /// restore effect in its durable handle. A cold recovery delegates
    /// handle-free marker reconstruction to the same provider terminal seam.
    /// Recover the root Session's exact physical Environment for terminal
    /// harvest without writing an ordinary Environment Adopt receipt. The
    /// terminal command and realization generation are the only authority;
    /// this method publishes an Arc only into the process-local teardown slot.
    /// The result owns its state projection; the complete immutable projection
    /// remains available for original-history queries.
    pub(super) async fn prepare_terminal_environment_under_lifecycle(
        &self,
        thread: &str,
        effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
        mode: BoundEnvironmentPreparationMode,
    ) -> Result<TerminalEnvironmentPreparation, HostError> {
        let realizer = &self.realizer;
        let (assignment, owner, previous_resources, workspace_id, checkpoint_policy) = self
            .installed_sessions
            .read(thread, |slot| {
                let baseline = slot.inputs.baseline.as_ref();
                let r = &slot.realization;
                (
                    r.terminal_cleanup_assignment.clone(),
                    r.environment_owner.clone(),
                    r.previous_resources(),
                    slot.inputs.workspace.clone(),
                    baseline.map(|baseline| baseline.environment.idle_retention.clone()),
                )
            })
            .ok_or_else(|| {
                HostError::unavailable_classified(
                    "session_terminal_cleanup_projection_missing",
                    "terminal cleanup root projection is not installed",
                )
            })?;
        let assignment = assignment.ok_or_else(|| {
            HostError::unavailable_classified(
                "session_terminal_environment_state_missing",
                "terminal cleanup has no frozen aggregate Environment state",
            )
        })?;
        let state = assignment.projection.environment.clone();
        let pending_checkpoint = if matches!(
            &state,
            SessionEnvironmentState::Suspending {
                suspend_phase: SuspendPhase::Uploading,
                ..
            }
        ) {
            let workspace_id = workspace_id.as_deref().ok_or_else(|| {
                HostError::unavailable_classified(
                    "session_terminal_checkpoint_workspace_missing",
                    "terminal checkpoint takeover has no frozen Workspace projection",
                )
            })?;
            let checkpoint_policy = checkpoint_policy.ok_or_else(|| {
                HostError::unavailable_classified(
                    "session_terminal_checkpoint_policy_missing",
                    "terminal checkpoint takeover has no frozen retention policy",
                )
            })?;
            Some(
                state
                    .checkpoint_request(
                        workspace_id,
                        &awaken_session_types::SessionId::from(thread),
                        &checkpoint_policy,
                    )
                    .map_err(|error| HostError::internal(error.to_string()))?
                    .ok_or_else(|| {
                        HostError::internal(
                            "Uploading Environment did not project its checkpoint request",
                        )
                    })?,
            )
        } else {
            None
        };
        awaken_session_realization_contract::validate_terminal_no_create_witness(
            assignment.environment_creation_not_attempted,
            &state,
            matches!(owner, SessionEnvironmentOwner::Vacant),
            assignment
                .projection
                .environment_materialization
                .as_deref()
                .is_some_and(
                    awaken_session_state_contract::SessionEnvironmentMaterialization::has_admitted,
                ),
        )
        .map_err(crate::managed_adapter_error::from_run_error)?;
        // An Adoption Candidate knows a durable binding even when the new effect
        // response is absent. Unknown Create commitment remains protected.
        let unacknowledged = owner.durable_binding().is_none()
            && matches!(
                &owner,
                SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::Candidate(_))
                    | SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                        owned: RetiringEnvironmentOwner::Unbound(_),
                        ..
                    })
            );
        let binding = state.binding().or_else(|| {
            (matches!(state, SessionEnvironmentState::Unmaterialized) && unacknowledged)
                .then(|| owner.physical_binding())
                .flatten()
        });
        if matches!(&state, SessionEnvironmentState::Restoring { .. }) {
            let crate::environment_owner::SessionEnvironmentOwner::Restoring(
                crate::environment_owner::SessionEnvironmentRestoration::Awaiting { request },
            ) = owner
            else {
                return Err(HostError::classified(
                    "session_terminal_restore_owner_conflict",
                    "Restoring aggregate state has no exact process-local restore request",
                ));
            };
            let expected = state
                .restoring_request(
                    workspace_id.as_deref().ok_or_else(|| {
                        HostError::unavailable_classified(
                            "session_terminal_restore_workspace_missing",
                            "Restoring terminal projection has no frozen Workspace",
                        )
                    })?,
                    &awaken_session_types::SessionId::from(thread),
                )
                .ok_or_else(|| {
                    HostError::internal("Restoring state did not project an exact request")
                })?;
            if request != expected {
                return Err(HostError::classified(
                    "session_terminal_restore_request_conflict",
                    "Restoring Runtime request differs from aggregate truth",
                ));
            }
            return Ok(TerminalEnvironmentPreparation {
                state,
                environment: PreparedBoundEnvironment::new(
                    None,
                    None,
                    TerminalEffectMode::RecoverOnlyUnprepared,
                ),
                pending_checkpoint,
            });
        }
        let Some(binding) = binding else {
            if owner.has_local_environment() || owner.durable_binding().is_some() {
                return Err(HostError::classified(
                    "session_terminal_environment_projection_conflict",
                    "terminal aggregate has no binding but Runtime retains an Environment",
                ));
            }
            if assignment.environment_creation_not_attempted
                || matches!(&state, SessionEnvironmentState::Hibernated { .. })
            {
                return Ok(TerminalEnvironmentPreparation {
                    state,
                    environment: PreparedBoundEnvironment::new(
                        None,
                        None,
                        TerminalEffectMode::LiveUnprepared,
                    )
                    .with_definitive_absence(),
                    pending_checkpoint,
                });
            }
            let expected_effect_fence = assignment
                .projection
                .original_create_fence(thread)
                .map_err(crate::host_error::from_run_error)?;
            let provider = realizer.projected_session_environment_provider(thread, None)?;
            let spec = realizer.session_environment_adoption_spec(
                thread,
                provider,
                previous_resources.as_ref(),
            );
            let environment = realizer
                .install_bound_environment_under_lifecycle(
                    TerminalEnvironmentInstallation {
                        thread,
                        binding: None,
                        committed_candidate: None,
                        provider,
                        spec: &spec,
                        handle: None,
                        expected_effect_fence: Some(&expected_effect_fence),
                        effect_fence,
                    },
                    None,
                    mode,
                )
                .await?;
            let durable_handle = environment
                .as_ref()
                .map(|(environment, _)| environment.lifecycle().handle());
            let definitive_absence = environment.is_none();
            let prepared = PreparedBoundEnvironment::new(
                environment,
                durable_handle,
                TerminalEffectMode::LiveUnprepared,
            );
            return Ok(TerminalEnvironmentPreparation {
                state,
                environment: if definitive_absence {
                    prepared.with_definitive_absence()
                } else {
                    prepared
                },
                pending_checkpoint,
            });
        };
        if owner.durable_binding() != Some(binding)
            && !(unacknowledged && owner.physical_binding() == Some(binding))
        {
            return Err(HostError::classified(
                "session_terminal_environment_binding_mismatch",
                "terminal Runtime owner conflicts with the frozen aggregate binding",
            ));
        }
        let environment = realizer
            .prepare_bound_environment_for_effect_under_lifecycle(
                thread,
                binding,
                effect_fence,
                previous_resources.as_ref(),
                mode,
            )
            .await?;
        Ok(TerminalEnvironmentPreparation {
            state,
            environment,
            pending_checkpoint,
        })
    }

    /// Reconcile every copy-backed Memory participant through the one
    /// terminal-v2 authority, then retire the provider's exact Copy guards.
    /// Only `RecoverOnlyAlreadyPrepared` means a durable provider cleanup gate
    /// already proves source effects completed; it performs only the
    /// process-local exact acknowledgement and never reads the Sandbox. An
    /// unprepared unavailable owner is rejected by the pure plan above when it
    /// carries writable Copy evidence.
    pub(super) async fn reconcile_terminal_memory(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
        environment: &crate::session_environment::SessionEnvironment,
        effect_mode: TerminalEffectMode,
        plan: &TerminalMemoryPlan,
    ) -> Result<(), HostError> {
        if effect_mode == TerminalEffectMode::LiveUnprepared && !plan.intents.is_empty() {
            let workspace_id = plan.workspace_id.as_deref().ok_or_else(|| {
                HostError::internal("terminal Memory plan has no frozen Workspace")
            })?;
            let mounter = self
                .realizer
                .environments
                .env
                .memory_mounter
                .clone()
                .ok_or_else(|| HostError::internal("terminal Memory copy has no MemoryMounter"))?;
            for intent in &plan.intents {
                self.realizer.cleanup.effect_fence(effect)?;
                let files = environment
                    .harvest()
                    .list_frozen_mount_files(intent.mount_path())
                    .await
                    .map_err(|error| HostError::unavailable(error.to_string()))?;
                self.realizer.cleanup.effect_fence(effect)?;
                let operation_reference = if self.authority.fetches_resources_upstream() {
                    awaken_resource_contract::MemoryMaterializationReferenceEncoder::<
                        awaken_session_state_contract::SessionTerminalMemoryIntent,
                    >::encode(
                        self.authority
                            .memory_reference_encoder()
                            .as_deref()
                            .ok_or_else(|| {
                                HostError::internal(
                                    "remote terminal Memory encoder is not configured",
                                )
                            })?,
                        workspace_id,
                        intent.memory_store_id(),
                        intent.config_version(),
                        intent.access(),
                        intent,
                    )
                    .map_err(|error| HostError::internal(error.to_string()))?
                } else {
                    intent.memory_store_id().to_owned()
                };
                self.realizer.cleanup.effect_fence(effect)?;
                mounter
                    .reconcile_recovered_copy(
                        &operation_reference,
                        intent.materialization(),
                        &files,
                        crate::managed_input_layout::managed_mount_access(intent.access()),
                    )
                    .await
                    .map_err(|error| HostError::unavailable(error.to_string()))?;
                self.realizer.cleanup.effect_fence(effect)?;
            }
        }
        let Some(materializations) = plan.acknowledged_materializations.as_deref() else {
            return Ok(());
        };
        let effect_fence = self.realizer.cleanup.effect_fence(effect)?;
        let authority = self
            .realizer
            .execution
            .provider_effect_authority(effect.command.session_id.as_str(), &effect_fence)?;
        environment
            .lifecycle()
            .acknowledge_memory_reconciliation(&effect_fence, materializations, &authority)
            .await
            .map_err(|error| HostError::unavailable(error.to_string()))
    }

    /// Complete the checkpoint participant owned by the frozen Environment
    /// phase, then delete its object inside this same terminal effect. A known
    /// reference is deleted directly. Uploading without a root receipt asks the
    /// exact physical Sandbox/marker owner to replay the old immutable `put`
    /// under the new terminal authorization and delete the resulting object.
    /// No application-side checkpoint deletion runs after this command.
    pub(super) async fn cleanup_terminal_checkpoint(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
        state: &awaken_session_state_contract::SessionEnvironmentState,
        pending_checkpoint: Option<&awaken_session_state_contract::SandboxCheckpointRequest>,
        environment: Option<&crate::session_environment::SessionEnvironment>,
    ) -> Result<(), HostError> {
        let checkpoint = state.checkpoint();
        if checkpoint.is_some() && pending_checkpoint.is_some() {
            return Err(HostError::internal(
                "terminal Environment cannot own both a pending and committed checkpoint",
            ));
        }
        if checkpoint.is_none() && pending_checkpoint.is_none() {
            return Ok(());
        }
        let store = self
            .realizer
            .env
            .environment_checkpoint_store
            .as_ref()
            .ok_or_else(|| {
                HostError::unavailable_classified(
                    "session_environment_checkpoint_store_unavailable",
                    "No Session environment checkpoint store is installed",
                )
            })?;
        let terminal_effect_fence = self.realizer.cleanup.effect_fence(effect)?;
        let authority = self.realizer.execution.provider_effect_authority(
            effect.command.session_id.as_str(),
            &terminal_effect_fence,
        )?;
        awaken_provisioning_contract::with_current_sandbox_effect(
            &terminal_effect_fence,
            &authority,
            |_| Ok(()),
        )
        .map_err(|error| HostError::unavailable(error.to_string()))?;
        match (checkpoint, pending_checkpoint) {
            (Some(checkpoint), None) => {
                store
                    .delete(&checkpoint.id)
                    .await
                    .map_err(|error| HostError::unavailable(error.to_string()))?;
            }
            (None, Some(request)) => {
                let environment = environment.ok_or_else(|| {
                    HostError::unavailable_classified(
                        "session_terminal_checkpoint_source_missing",
                        "Uploading terminal checkpoint has no exact source Environment",
                    )
                })?;
                let expected_effect_fence = request
                    .operation
                    .sandbox_effect_fence()
                    .map_err(|error| HostError::internal(error.to_string()))?
                    .ok_or_else(|| {
                        HostError::unavailable_classified(
                            "session_terminal_checkpoint_legacy_unfenced",
                            "legacy checkpoint operation cannot be taken over after response loss",
                        )
                    })?;
                let provider_request =
                    crate::environment_continuation::provisioning_checkpoint_request(request);
                environment
                    .lifecycle()
                    .cleanup_checkpoint_for_terminal(
                        &provider_request,
                        store.as_ref(),
                        &expected_effect_fence,
                        &terminal_effect_fence,
                        &authority,
                    )
                    .await
                    .map_err(|error| HostError::unavailable(error.to_string()))?;
            }
            (Some(_), Some(_)) | (None, None) => {
                unreachable!("checkpoint shape was classified above")
            }
        }
        awaken_provisioning_contract::with_current_sandbox_effect(
            &terminal_effect_fence,
            &authority,
            |_| Ok(()),
        )
        .map_err(|error| HostError::unavailable(error.to_string()))
    }
}

impl crate::terminal_cleanup::TerminalCleanup {
    /// Revalidate the one aggregate-owned terminal assignment cached in the
    /// root Session slot. Preparation and disposal both consume this decision;
    /// each then lowers its own closed effect type exactly once.
    pub(crate) fn authorize_lease(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &session_resource::SessionRealizationLease,
    ) -> Result<session_resource::SessionRealizationLease, HostError> {
        self
            .sessions
            .read_realization(session_id.as_str(), |r| {
                let current = r.authority.lease().ok_or_else(|| {
                    HostError::unavailable_classified(
                        "session_terminal_cleanup_projection_missing",
                        "terminal cleanup has no installed realization generation",
                    )
                })?;
                if !awaken_session_realization_contract::realization_lease_generation_authorizes(current, lease) {
                    return Err(HostError::unavailable_classified(
                        "session_terminal_cleanup_generation_stale",
                        "terminal cleanup realization generation was replaced",
                    ));
                }
                if r.authority.remaining_for(lease).is_none() {
                    return Err(HostError::unavailable_classified(
                        "session_terminal_cleanup_effect_expired",
                        "terminal cleanup generation has no current live renewal at its physical boundary",
                    ));
                }
                Ok(current.clone())
            })
            .unwrap_or_else(|| Err(
                HostError::unavailable_classified(
                    "session_terminal_cleanup_projection_missing",
                    "terminal cleanup has no installed realization generation",
                )
            ))
    }

    /// Read back the one exact terminal Preparation owner without relabeling it
    /// to the later provider-disposal operation. The aggregate's Disposal
    /// authorization supplies that destructive fence separately.
    pub(crate) fn retained_preparation_owner(
        &self,
        thread: &str,
        environment: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> bool {
        self.sessions
            .read_realization(thread, |r| match &r.environment_owner {
                SessionEnvironmentOwner::Retiring(retirement)
                    if matches!(
                        retirement.cause,
                        SessionEnvironmentRetirementCause::Terminal { .. }
                    ) =>
                {
                    let current = retirement.owned.environment();
                    Arc::ptr_eq(&current, environment)
                        && current.lifecycle().handle() == environment.lifecycle().handle()
                }
                _ => false,
            })
            .unwrap_or(false)
    }

    /// Lower the aggregate's durable preparation predecessor and current
    /// successor lease into the sole provider-neutral destructive authority.
    pub(in crate::host) fn disposal_authorization(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<awaken_provisioning_contract::SandboxDisposalAuthorization, HostError> {
        let current = self.authorize_lease(&effect.command.session_id, &effect.lease)?;
        // The same slot proof already authorized preparation and disposal.
        // Lowering preserves its Control-clock fence bytes; comparing them to
        // Worker UTC would invent a second, incompatible expiry decision.
        // This projection is not a grant for a later asynchronous effect.
        effect
            .sandbox_disposal_authorization_for_current_generation(&current)
            .map_err(|error| {
                HostError::unavailable_classified(
                    "session_terminal_cleanup_disposal_unauthorized",
                    error.to_string(),
                )
            })
    }

    /// Project the one preparation fence after the shared terminal assignment
    /// check above. Physical adapters repeat expiry and incarnation checks at
    /// their own effect boundaries.
    pub(in crate::host) fn effect_fence(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) -> Result<awaken_provisioning_contract::SandboxEffectFence, HostError> {
        self.authorize_lease(&effect.command.session_id, &effect.lease)?
            .sandbox_effect_fence(effect.operation_id())
            .map_err(|error| HostError::internal(error.to_string()))
    }

    /// Join frozen inputs with the durable handle's optional evidence before
    /// any externally visible terminal effect. `None` remains legacy/unknown;
    /// the contract owner rejects it for writable Memory instead of allowing a
    /// caller to reinterpret it as an explicit empty current set.
    pub(super) fn memory_plan(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
        prepared: &PreparedBoundEnvironment,
    ) -> Result<TerminalMemoryPlan, HostError> {
        let transition = self
            .sessions
            .read_realization(&effect.command.thread_id, |r| r.resource_transition.clone())
            .flatten()
            .ok_or_else(|| {
                HostError::internal("terminal Memory validation has no frozen Resource transition")
            })?;
        let materializations = prepared
            .memory_materializations()
            .map_err(|error| HostError::internal(error.to_string()))?;
        let (intents, acknowledged_materializations) =
            awaken_session_state_contract::terminal_memory_reconciliation_intents_from_materializations(
                transition.previous().resources.inputs(),
                materializations,
                effect,
            )
            .map_err(|error| {
                HostError::unavailable_classified(
                    "session_terminal_memory_evidence_unreconciled",
                    error.to_string(),
                )
            })?;
        if !intents.is_empty()
            && !prepared.source_effects_are_already_prepared()
            && (prepared.environment.is_none() || !prepared.permits_live_io())
        {
            return Err(HostError::unavailable_classified(
                "session_terminal_memory_source_absent",
                "writable terminal Memory has no readable source or durable provider preparation",
            ));
        }
        Ok(TerminalMemoryPlan {
            intents,
            acknowledged_materializations: acknowledged_materializations.map(<[_]>::to_vec),
            workspace_id: Some(transition.previous().workspace_id.clone()),
        })
    }
}
