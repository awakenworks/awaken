//! Aggregate-authorized terminal Environment preparation substrate.
//!
//! This private split retains the parent module's sole `SessionEnvironmentOwner`;
//! it only projects exact leases, provider observations, Memory evidence, and
//! checkpoint work into the existing two-stage terminal protocol.

use super::*;

impl SharedHost {
    /// Read back the one exact terminal Preparation owner without relabeling it
    /// to the later provider-disposal operation. The aggregate's Disposal
    /// authorization supplies that destructive fence separately.
    fn retained_terminal_preparation_owner(
        &self,
        thread: &str,
        environment: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> bool {
        self.session_slots
            .read(thread, |slot| match &slot.environment_owner {
                SessionEnvironmentOwner::Retiring(retirement)
                    if matches!(
                        retirement.cause,
                        SessionEnvironmentRetirementCause::Terminal { .. }
                    ) =>
                {
                    let current = retirement.owned.environment();
                    Arc::ptr_eq(&current, environment) && current.handle() == environment.handle()
                }
                _ => false,
            })
            .unwrap_or(false)
    }

    /// Revalidate the one aggregate-owned terminal assignment cached in the
    /// root Session slot. Preparation and disposal both consume this decision;
    /// each then lowers its own closed effect type exactly once.
    pub(crate) fn authorize_terminal_cleanup_lease(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<awaken_session_contract::SessionRealizationLease, HostError> {
        let current = self
            .session_slots
            .read(session_id, |slot| slot.realization_lease.clone())
            .flatten()
            .ok_or_else(|| {
                HostError::unavailable_classified(
                    "session_terminal_cleanup_projection_missing",
                    "terminal cleanup has no installed realization generation",
                )
            })?;
        if !awaken_session_contract::realization_lease_generation_authorizes(&current, lease) {
            return Err(HostError::unavailable_classified(
                "session_terminal_cleanup_generation_stale",
                "terminal cleanup realization generation was replaced",
            ));
        }
        let now_unix_ms = crate::terminal_repository_publication::runtime_unix_now_ms();
        if !awaken_session_contract::realization_lease_is_live_at(
            current.expires_at_unix_ms,
            now_unix_ms,
        ) {
            return Err(HostError::unavailable_classified(
                "session_terminal_cleanup_effect_expired",
                "terminal cleanup generation has no current live renewal at its physical boundary",
            ));
        }
        Ok(current)
    }

    /// Project the one preparation fence after the shared terminal assignment
    /// check above. Physical adapters repeat expiry and incarnation checks at
    /// their own effect boundaries.
    pub(in crate::host) fn terminal_cleanup_effect_fence(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupEffect,
    ) -> Result<awaken_provisioning_contract::SandboxEffectFence, HostError> {
        self.authorize_terminal_cleanup_lease(&effect.command.session_id, &effect.lease)?
            .sandbox_effect_fence(effect.operation_id())
            .map_err(|error| HostError::internal(error.to_string()))
    }

    /// Lower the aggregate's durable preparation predecessor and current
    /// successor lease into the sole provider-neutral destructive authority.
    pub(in crate::host) fn terminal_cleanup_disposal_authorization(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<awaken_provisioning_contract::SandboxDisposalAuthorization, HostError> {
        let current =
            self.authorize_terminal_cleanup_lease(&effect.command.session_id, &effect.lease)?;
        let authorization = effect
            .sandbox_disposal_authorization_for_current_generation(&current)
            .map_err(|error| {
                HostError::unavailable_classified(
                    "session_terminal_cleanup_disposal_unauthorized",
                    error.to_string(),
                )
            })?;
        authorization
            .effect_fence()
            .validate_live_at(crate::terminal_repository_publication::runtime_unix_now_ms())
            .map_err(|error| {
                HostError::unavailable_classified(
                    "session_terminal_cleanup_disposal_expired",
                    error.to_string(),
                )
            })?;
        Ok(authorization)
    }

    /// Re-open one exact terminal substrate into the process-local teardown
    /// slot. This helper deliberately writes no ordinary Environment receipt:
    /// the surrounding terminal command is the sole durable effect authority.
    async fn install_bound_environment_under_lifecycle(
        &self,
        installation: TerminalEnvironmentInstallation<'_>,
        resident: Option<Arc<crate::session_environment::SessionEnvironment>>,
        mode: BoundEnvironmentPreparationMode,
    ) -> Result<Option<(Arc<crate::session_environment::SessionEnvironment>, bool)>, HostError>
    {
        let TerminalEnvironmentInstallation {
            thread,
            binding,
            provider,
            spec,
            handle,
            expected_effect_fence,
            effect_fence,
        } = installation;
        let adopted = resident.is_none();
        let environment = match resident {
            Some(environment) => environment,
            None => match mode {
                BoundEnvironmentPreparationMode::LiveSource => Arc::new(
                    provider
                        .adopt_effective_for_effect(
                            spec,
                            handle.ok_or_else(|| {
                                HostError::internal(
                                    "live bound Environment recovery has no durable handle",
                                )
                            })?,
                            Some(effect_fence),
                        )
                        .await
                        .map_err(|error| {
                            HostError::unavailable_classified(
                                "session_live_environment_adoption_indeterminate",
                                error.to_string(),
                            )
                        })?,
                ),
                BoundEnvironmentPreparationMode::Terminal
                | BoundEnvironmentPreparationMode::TerminalDisposal => {
                    let Some(environment) = provider
                        .prepare_terminal_effective_for_effect(
                            spec,
                            handle,
                            expected_effect_fence,
                            effect_fence,
                        )
                        .await
                        .map_err(|error| {
                            HostError::unavailable_classified(
                                "session_terminal_environment_preparation_indeterminate",
                                error.to_string(),
                            )
                        })?
                    else {
                        return Ok(None);
                    };
                    Arc::new(environment)
                }
            },
        };
        match mode {
            BoundEnvironmentPreparationMode::LiveSource => {
                if adopted {
                    let candidate =
                        self.begin_session_environment_adoption(thread, environment.clone())?;
                    let crate::session_slot::UnboundSessionEnvironmentOrigin::DurableAdoption(
                        identity,
                    ) = &candidate.origin
                    else {
                        return Err(HostError::internal(
                            "live source adoption has no durable Environment identity",
                        ));
                    };
                    self.publish_prepared_session_environment(
                        thread,
                        &candidate,
                        identity.clone(),
                    )?;
                } else {
                    let still_resident = self
                        .session_slots
                        .read(thread, |slot| {
                            slot.environment_owner.resident().is_some_and(|current| {
                                Arc::ptr_eq(&current, &environment)
                                    && current.handle() == environment.handle()
                            })
                        })
                        .unwrap_or(false);
                    if !still_resident {
                        return Err(HostError::unavailable_classified(
                            "session_live_environment_owner_not_resident",
                            "live source recovery found a non-resident Environment owner",
                        ));
                    }
                }
            }
            BoundEnvironmentPreparationMode::Terminal
            | BoundEnvironmentPreparationMode::TerminalDisposal => {
                let cause = SessionEnvironmentRetirementCause::Terminal {
                    effect_id: effect_fence.operation_id.clone(),
                };
                if adopted {
                    let Some(binding) = binding else {
                        return Err(HostError::internal(
                            "terminal bound Environment adoption has no durable binding",
                        ));
                    };
                    let (identity, pending_binding) =
                        self.pending_environment_adoption(thread).ok_or_else(|| {
                            HostError::internal(
                                "terminal bound Environment has no pending durable owner",
                            )
                        })?;
                    if pending_binding != binding {
                        return Err(HostError::internal(
                            "terminal bound Environment changed its durable binding",
                        ));
                    }
                    self.session_slots.update(thread, |slot| {
                        slot.environment_owner.begin_retirement_with_adopted(
                            &identity,
                            binding,
                            environment.clone(),
                            cause,
                        )
                    })?;
                } else {
                    let retained_terminal_preparation = mode
                        == BoundEnvironmentPreparationMode::TerminalDisposal
                        && self.retained_terminal_preparation_owner(thread, &environment);
                    if !retained_terminal_preparation {
                        self.retire_current_environment(
                            thread,
                            cause,
                            RetirementSelection::Exact(&environment),
                        )?
                        .ok_or_else(|| {
                            HostError::unavailable_classified(
                                "session_terminal_environment_owner_changed",
                                "terminal Environment lost its exact owner fence",
                            )
                        })?;
                    }
                }
                self.session_slots.update(thread, |slot| {
                    slot.environment_resource_reconciliation =
                        crate::session_slot::EnvironmentResourceReconciliation::None;
                });
            }
        }
        Ok(Some((environment, adopted)))
    }

    /// Resolve one durable binding into the process-local teardown owner under
    /// an already-authorized physical effect. Terminal cleanup and continuation
    /// source disposal share this path so provider observation, exact-absence
    /// proof, terminal physical disposal, and slot publication cannot drift.
    pub(crate) async fn prepare_bound_environment_for_effect_under_lifecycle(
        &self,
        thread: &str,
        binding: &str,
        effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
        resolved_resources: Option<&awaken_session_contract::ResolvedSessionResources>,
        mode: BoundEnvironmentPreparationMode,
    ) -> Result<PreparedBoundEnvironment, HostError> {
        let provider = self.projected_session_environment_provider(thread, None)?;
        let (handle, spec, resident, candidate) = self.validated_session_environment_adoption(
            thread,
            binding,
            provider,
            resolved_resources,
        )?;
        if candidate.is_some() {
            return Err(HostError::unavailable_classified(
                "session_terminal_unpublished_candidate",
                "terminal preparation cannot consume an unpublished Environment Candidate",
            ));
        }
        let observation = provider
            .observe_effective_for_effect(&spec, &handle, effect_fence)
            .await
            .map_err(|error| {
                HostError::unavailable_classified(
                    "session_environment_effect_observation_indeterminate",
                    error.to_string(),
                )
            })?;
        if mode == BoundEnvironmentPreparationMode::LiveSource {
            match &observation {
                awaken_provisioning_contract::SandboxObservation::Ready => {}
                awaken_provisioning_contract::SandboxObservation::Provisioning => {
                    return Err(HostError::unavailable_classified(
                        "session_environment_live_source_provisioning",
                        "Session Environment live source is still provisioning",
                    ));
                }
                observation
                @ (awaken_provisioning_contract::SandboxObservation::DefinitivelyUnavailable {
                    ..
                }
                | awaken_provisioning_contract::SandboxObservation::Terminal { .. }
                | awaken_provisioning_contract::SandboxObservation::Disposing { .. }) => {
                    provider
                        .validate_closed_observation(&handle, observation)
                        .map_err(|error| HostError::internal(error.to_string()))?;
                    return Err(HostError::unavailable_classified(
                        "session_environment_live_source_not_ready",
                        "Session Environment live source is already closed",
                    ));
                }
                awaken_provisioning_contract::SandboxObservation::Incompatible { reason } => {
                    return Err(HostError::classified(
                        "session_environment_effect_incompatible",
                        reason,
                    ));
                }
            }
        }
        let (resident, effect_mode) = match observation {
            awaken_provisioning_contract::SandboxObservation::Ready => {
                (resident, TerminalEffectMode::LiveUnprepared)
            }
            awaken_provisioning_contract::SandboxObservation::Provisioning => {
                return Err(HostError::unavailable_classified(
                    "session_environment_effect_provisioning",
                    "Session Environment is still provisioning",
                ));
            }
            observation
            @ awaken_provisioning_contract::SandboxObservation::DefinitivelyUnavailable {
                ..
            } => {
                provider
                    .validate_closed_observation(&handle, &observation)
                    .map_err(|error| HostError::internal(error.to_string()))?;
                if let Some(environment) = resident {
                    let retained_terminal_preparation = mode
                        == BoundEnvironmentPreparationMode::TerminalDisposal
                        && self.retained_terminal_preparation_owner(thread, &environment);
                    if !retained_terminal_preparation {
                        self.retire_current_environment(
                            thread,
                            SessionEnvironmentRetirementCause::Terminal {
                                effect_id: effect_fence.operation_id.clone(),
                            },
                            RetirementSelection::Exact(&environment),
                        )?
                        .ok_or_else(|| {
                            HostError::unavailable_classified(
                                "session_environment_effect_fence_lost",
                                "lost the exact unavailable Environment owner",
                            )
                        })?;
                    }
                    // Provider-proved absence closes physical I/O, but the
                    // exact terminal Retiring owner remains the process-local
                    // response-loss fence until aggregate acknowledgement.
                    // Ordinary readers cannot access it, and Disposal retries
                    // can re-observe the same durable handle without inventing
                    // an Awaiting/Vacant completion fact.
                }
                // Exact primary absence is not necessarily complete absence:
                // the provider alone can interpret typed auxiliary evidence in
                // the durable handle. Re-enter the same terminal-preparation
                // seam used by live/terminal observations; it returns `None`
                // for total absence or a non-executable exact cleanup owner.
                (None, TerminalEffectMode::RecoverOnlyUnprepared)
            }
            observation @ awaken_provisioning_contract::SandboxObservation::Terminal { .. } => {
                provider
                    .validate_closed_observation(&handle, &observation)
                    .map_err(|error| HostError::internal(error.to_string()))?;
                (resident, TerminalEffectMode::LiveUnprepared)
            }
            observation @ awaken_provisioning_contract::SandboxObservation::Disposing { .. } => {
                provider
                    .validate_closed_observation(&handle, &observation)
                    .map_err(|error| HostError::internal(error.to_string()))?;
                // Disposing is durable proof that every source-durability
                // participant required by the preceding operation crossed the
                // provider cleanup gate. It does not fabricate a terminal-scoped
                // Artifact association; the terminal fence below may recover an
                // existing receipt or correctly recover none. Reconstruct only
                // the physical cleanup owner and never reuse a live-I/O wrapper.
                (resident, TerminalEffectMode::RecoverOnlyAlreadyPrepared)
            }
            awaken_provisioning_contract::SandboxObservation::Incompatible { reason } => {
                return Err(HostError::classified(
                    "session_environment_effect_incompatible",
                    reason,
                ));
            }
        };
        let environment = self
            .install_bound_environment_under_lifecycle(
                TerminalEnvironmentInstallation {
                    thread,
                    binding: Some(binding),
                    provider,
                    spec: &spec,
                    handle: Some(&handle),
                    expected_effect_fence: None,
                    effect_fence,
                },
                resident,
                mode,
            )
            .await?;
        Ok(PreparedBoundEnvironment::new(
            environment,
            Some(handle),
            effect_mode,
        ))
    }

    /// Take over a restore that has no aggregate binding yet. The frozen
    /// restore fence is identity evidence only; the live terminal fence is the
    /// sole mutation authority. A hot process-local owner must carry the exact
    /// restore effect in its durable handle. A cold recovery delegates
    /// handle-free marker reconstruction to the same provider terminal seam.
    /// Recover the root Session's exact physical Environment for terminal
    /// harvest without writing an ordinary Environment Adopt receipt. The
    /// terminal command and realization generation are the only authority;
    /// this method publishes an Arc only into the process-local teardown slot.
    pub(super) async fn prepare_terminal_environment_under_lifecycle(
        &self,
        thread: &str,
        effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
        mode: BoundEnvironmentPreparationMode,
    ) -> Result<TerminalEnvironmentPreparation, HostError> {
        let (state, owner, previous_resources, workspace_id, checkpoint_policy) = self
            .session_slots
            .read(thread, |slot| {
                (
                    slot.terminal_environment_state.clone(),
                    slot.environment_owner.clone(),
                    slot.resource_transition
                        .as_ref()
                        .map(|transition| transition.previous().resources.clone()),
                    slot.workspace.clone(),
                    slot.baseline
                        .as_ref()
                        .map(|baseline| baseline.environment.idle_retention.clone()),
                )
            })
            .ok_or_else(|| {
                HostError::unavailable_classified(
                    "session_terminal_cleanup_projection_missing",
                    "terminal cleanup root projection is not installed",
                )
            })?;
        let state = state.ok_or_else(|| {
            HostError::unavailable_classified(
                "session_terminal_environment_state_missing",
                "terminal cleanup has no frozen aggregate Environment state",
            )
        })?;
        let pending_checkpoint = if matches!(
            &state,
            awaken_session_contract::SessionEnvironmentState::Suspending {
                suspend_phase: awaken_session_contract::SuspendPhase::Uploading,
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
                    .checkpoint_request(workspace_id, thread, &checkpoint_policy)
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
        let binding = match &state {
            awaken_session_contract::SessionEnvironmentState::Resident { binding, .. } => {
                Some(binding.as_str())
            }
            awaken_session_contract::SessionEnvironmentState::Suspending {
                source_binding, ..
            } => Some(source_binding.as_str()),
            awaken_session_contract::SessionEnvironmentState::Unmaterialized
            | awaken_session_contract::SessionEnvironmentState::Hibernated { .. } => None,
            awaken_session_contract::SessionEnvironmentState::Restoring { .. } => None,
        };
        if matches!(
            &state,
            awaken_session_contract::SessionEnvironmentState::Restoring { .. }
        ) {
            let crate::session_slot::SessionEnvironmentOwner::Restoring(
                crate::session_slot::SessionEnvironmentRestoration::Awaiting { request },
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
                    thread,
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
            return Ok(TerminalEnvironmentPreparation {
                state,
                environment: PreparedBoundEnvironment::new(
                    None,
                    None,
                    TerminalEffectMode::LiveUnprepared,
                ),
                pending_checkpoint,
            });
        };
        if owner.durable_binding() != Some(binding) {
            return Err(HostError::classified(
                "session_terminal_environment_binding_mismatch",
                "terminal Runtime owner conflicts with the frozen aggregate binding",
            ));
        }
        let environment = self
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

    /// Join frozen inputs with the durable handle's optional evidence before
    /// any externally visible terminal effect. `None` remains legacy/unknown;
    /// the contract owner rejects it for writable Memory instead of allowing a
    /// caller to reinterpret it as an explicit empty current set.
    pub(super) fn terminal_memory_plan(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupEffect,
        prepared: &PreparedBoundEnvironment,
    ) -> Result<TerminalMemoryPlan, HostError> {
        let transition = self
            .session_slots
            .read(&effect.command.thread_id, |slot| {
                slot.resource_transition.clone()
            })
            .flatten()
            .ok_or_else(|| {
                HostError::internal("terminal Memory validation has no frozen Resource transition")
            })?;
        let materializations = match prepared.durable_handle() {
            Some(handle) => handle
                .memory_materializations()
                .map_err(|error| HostError::internal(error.to_string()))?,
            None => None,
        };
        let (intents, acknowledged_materializations) =
            awaken_session_contract::terminal_memory_reconciliation_intents_from_materializations(
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

    /// Reconcile every copy-backed Memory participant through the one
    /// terminal-v2 authority, then retire the provider's exact Copy guards.
    /// Only `RecoverOnlyAlreadyPrepared` means a durable provider cleanup gate
    /// already proves source effects completed; it performs only the
    /// process-local exact acknowledgement and never reads the Sandbox. An
    /// unprepared unavailable owner is rejected by the pure plan above when it
    /// carries writable Copy evidence.
    pub(super) async fn reconcile_terminal_memory(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupEffect,
        environment: &crate::session_environment::SessionEnvironment,
        effect_mode: TerminalEffectMode,
        plan: &TerminalMemoryPlan,
    ) -> Result<(), HostError> {
        if effect_mode == TerminalEffectMode::LiveUnprepared && !plan.intents.is_empty() {
            let workspace_id = plan.workspace_id.as_deref().ok_or_else(|| {
                HostError::internal("terminal Memory plan has no frozen Workspace")
            })?;
            let mounter = self
                .memory_mounter()
                .ok_or_else(|| HostError::internal("terminal Memory copy has no MemoryMounter"))?;
            for intent in &plan.intents {
                self.terminal_cleanup_effect_fence(effect)?;
                let files = environment
                    .list_frozen_mount_files(intent.mount_path())
                    .await
                    .map_err(|error| HostError::unavailable(error.to_string()))?;
                self.terminal_cleanup_effect_fence(effect)?;
                let operation_reference = match &self.upstream {
                    Some(_) => awaken_resource_contract::MemoryMaterializationReferenceEncoder::<
                        awaken_session_contract::SessionTerminalMemoryIntent,
                    >::encode(
                        self.memory_reference_encoder
                            .as_ref()
                            .ok_or_else(|| {
                                HostError::internal(
                                    "remote terminal Memory encoder is not configured",
                                )
                            })?
                            .as_ref(),
                        workspace_id,
                        intent.memory_store_id(),
                        intent.config_version(),
                        intent.access(),
                        intent,
                    )
                    .map_err(|error| HostError::internal(error.to_string()))?,
                    None => intent.memory_store_id().to_owned(),
                };
                self.terminal_cleanup_effect_fence(effect)?;
                mounter
                    .reconcile_recovered_copy(
                        &operation_reference,
                        intent.materialization(),
                        &files,
                        crate::managed_resource_projection::managed_mount_access(intent.access()),
                    )
                    .await
                    .map_err(|error| HostError::unavailable(error.to_string()))?;
                self.terminal_cleanup_effect_fence(effect)?;
            }
        }
        let Some(materializations) = plan.acknowledged_materializations.as_deref() else {
            return Ok(());
        };
        let effect_fence = self.terminal_cleanup_effect_fence(effect)?;
        environment
            .acknowledge_memory_reconciliation(&effect_fence, materializations)
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
        effect: &awaken_session_contract::SessionTerminalCleanupEffect,
        state: &awaken_session_contract::SessionEnvironmentState,
        pending_checkpoint: Option<&awaken_session_contract::SandboxCheckpointRequest>,
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
        let store = self.environment_checkpoint_store.as_ref().ok_or_else(|| {
            HostError::unavailable_classified(
                "session_environment_checkpoint_store_unavailable",
                "No Session environment checkpoint store is installed",
            )
        })?;
        match (checkpoint, pending_checkpoint) {
            (Some(checkpoint), None) => {
                self.terminal_cleanup_effect_fence(effect)?;
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
                let terminal_effect_fence = self.terminal_cleanup_effect_fence(effect)?;
                environment
                    .cleanup_checkpoint_for_terminal(
                        &provider_request,
                        store.as_ref(),
                        &expected_effect_fence,
                        &terminal_effect_fence,
                    )
                    .await
                    .map_err(|error| HostError::unavailable(error.to_string()))?;
            }
            (Some(_), Some(_)) | (None, None) => {
                unreachable!("checkpoint shape was classified above")
            }
        }
        self.terminal_cleanup_effect_fence(effect)?;
        Ok(())
    }
}
