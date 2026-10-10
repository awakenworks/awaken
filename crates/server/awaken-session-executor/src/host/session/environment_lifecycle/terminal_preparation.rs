// @formal evidence session_environment.owner_topology_is_exact_and_frozen
// @formal evidence session_environment.source_disposal_requires_a_verified_checkpoint
// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
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

    /// Revalidate the one aggregate-owned terminal assignment cached in the
    /// root Session slot. Preparation and disposal both consume this decision;
    /// each then lowers its own closed effect type exactly once.
    pub(crate) fn authorize_terminal_cleanup_lease(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<awaken_session_resource_contract::SessionRealizationLease, HostError> {
        self
            .session_slots
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

    /// Project the one preparation fence after the shared terminal assignment
    /// check above. Physical adapters repeat expiry and incarnation checks at
    /// their own effect boundaries.
    pub(in crate::host) fn terminal_cleanup_effect_fence(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) -> Result<awaken_provisioning_contract::SandboxEffectFence, HostError> {
        self.authorize_terminal_cleanup_lease(&effect.command.session_id, &effect.lease)?
            .sandbox_effect_fence(effect.operation_id())
            .map_err(|error| HostError::internal(error.to_string()))
    }

    /// Lower the aggregate's durable preparation predecessor and current
    /// successor lease into the sole provider-neutral destructive authority.
    pub(in crate::host) fn terminal_cleanup_disposal_authorization(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<awaken_provisioning_contract::SandboxDisposalAuthorization, HostError> {
        let current =
            self.authorize_terminal_cleanup_lease(&effect.command.session_id, &effect.lease)?;
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
            committed_candidate,
            provider,
            spec,
            handle,
            expected_effect_fence,
            effect_fence,
        } = installation;
        let mut adopted = resident.is_none();
        let authority = self.session_slots.provider_effect_authority(
            thread,
            effect_fence,
            self.execution_authority.as_ref(),
        )?;
        let mut environment = match resident {
            Some(environment) => environment,
            None => match mode {
                BoundEnvironmentPreparationMode::LiveSource
                | BoundEnvironmentPreparationMode::TerminalRepositoryPublication => Arc::new(
                    provider
                        .adoption()
                        .adopt_effective_for_effect(
                            spec,
                            handle.ok_or_else(|| {
                                HostError::internal(
                                    "live bound Environment recovery has no durable handle",
                                )
                            })?,
                            crate::session_environment::ProviderEffect::Fenced {
                                fence: effect_fence,
                                authority: &authority,
                            },
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
                        .terminal()
                        .prepare_terminal_effective_for_effect(
                            spec,
                            handle,
                            expected_effect_fence,
                            effect_fence,
                            &authority,
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
        // Revocation closes the old Hand permanently, not the aggregate's exact
        // physical source. Terminal Repository publication already holds its
        // current effect authority and a Ready observation. Reuse the ordinary
        // fresh-wrapper mechanism, but retire its Candidate immediately under
        // the existing Store identity: never publish a Resident or an Adopt
        // receipt merely to finish this terminal command.
        if mode == BoundEnvironmentPreparationMode::TerminalRepositoryPublication
            && let Some(retiring) = self
                .session_slots
                .read_realization(thread, |r| match &r.environment_owner {
                    SessionEnvironmentOwner::Retiring(retiring)
                        if retiring.cause
                            == SessionEnvironmentRetirementCause::RealizationRevocation
                            && matches!(&retiring.owned,
                                RetiringEnvironmentOwner::Bound(owned)
                                    if matches!(&owned.identity,
                                        BoundSessionEnvironmentIdentity::Durable { .. })
                                    && binding == Some(owned.binding.as_str())
                                    && Arc::ptr_eq(&owned.environment, &environment)) =>
                    {
                        Some(retiring.clone())
                    }
                    _ => None,
                })
                .flatten()
        {
            let admit = || {
                awaken_provisioning_contract::with_current_sandbox_effect(
                    effect_fence,
                    &authority,
                    |_| Ok(()),
                )
                .map_err(|error| {
                    HostError::unavailable_classified(
                        "session_terminal_repository_rebind_authority_lost",
                        error.to_string(),
                    )
                })
            };
            admit()?;
            let fresh = Arc::new(
                provider
                    .adoption()
                    .rebind_retained(
                        spec,
                        &environment.lifecycle().handle(),
                        &environment,
                        crate::session_environment::ProviderEffect::Fenced {
                            fence: effect_fence,
                            authority: &authority,
                        },
                    )
                    .await
                    .map_err(|error| {
                        HostError::unavailable_classified(
                            "session_terminal_repository_source_rebind_indeterminate",
                            error.to_string(),
                        )
                    })?,
            );
            admit()?;
            self.session_slots.update_realization(thread, |r| {
                r.environment_owner
                    .prepare_retiring_realization_adoption(&retiring, fresh.clone())?;
                let SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::Candidate(
                    candidate,
                )) = &r.environment_owner
                else {
                    return Err(HostError::internal(
                        "terminal source rebind lost its Candidate",
                    ));
                };
                let candidate = candidate.clone();
                r.environment_owner
                    .begin_retirement_with_committed_candidate(
                        &candidate,
                        SessionEnvironmentRetirementCause::Terminal {
                            effect_id: effect_fence.operation_id().to_owned(),
                        },
                    )?;
                Ok::<_, HostError>(())
            })?;
            environment = fresh;
            adopted = false;
        }
        if adopted
            && !mode.requires_live_source()
            && let Some(retiring) = self
                .session_slots
                .read_realization(thread, |r| match &r.environment_owner {
                    SessionEnvironmentOwner::Retiring(retiring) => Some(retiring.clone()),
                    _ => None,
                })
                .flatten()
        {
            // Primary absence still requires the Provider's auxiliary/total
            // absence decision. A reconstructed descriptor is not permission
            // to replace the retained owner and lose in-memory participants.
            // Keep its exact Arc; only a genuinely cold owner is installed.
            if !matches!(
                retiring.cause,
                SessionEnvironmentRetirementCause::Terminal { .. }
            ) || binding != Some(retiring.owned.binding())
                || environment.lifecycle().handle()
                    != retiring.owned.environment().lifecycle().handle()
            {
                return Err(HostError::unavailable_classified(
                    "session_terminal_environment_owner_changed",
                    "terminal reconstruction conflicts with the exact retained owner",
                ));
            }
            environment = retiring.owned.environment();
            adopted = false;
        }
        match mode {
            BoundEnvironmentPreparationMode::LiveSource
            | BoundEnvironmentPreparationMode::TerminalRepositoryPublication => {
                if adopted {
                    let candidate =
                        self.begin_session_environment_adoption(thread, environment.clone())?;
                    let crate::environment_owner::UnboundSessionEnvironmentOrigin::AdoptionFrom(
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
                        .read_realization(thread, |r| {
                            r.environment_owner.resident().is_some_and(|current| {
                                Arc::ptr_eq(&current, &environment)
                                    && current.lifecycle().handle()
                                        == environment.lifecycle().handle()
                            })
                        })
                        .unwrap_or(false);
                    // A response-lost terminal preparation keeps the bound Arc
                    // behind the existing retirement fence. Only the terminal
                    // Repository role can read that same Ready source; generic
                    // live adoption cannot reopen it for tools/checkpoint work.
                    let retained_terminal = mode
                        == BoundEnvironmentPreparationMode::TerminalRepositoryPublication
                        && self.retained_terminal_preparation_owner(thread, &environment);
                    if !still_resident && !retained_terminal {
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
                    effect_id: effect_fence.operation_id().to_owned(),
                };
                if let Some(candidate) = committed_candidate {
                    if !Arc::ptr_eq(&candidate.environment, &environment)
                        || candidate.environment.lifecycle().handle()
                            != environment.lifecycle().handle()
                    {
                        return Err(HostError::internal(
                            "terminal committed Candidate changed its exact Environment",
                        ));
                    }
                    self.session_slots.update_realization(thread, |r| {
                        r.environment_owner
                            .begin_retirement_with_committed_candidate(candidate, cause)
                    })?;
                } else if adopted {
                    if let Some(binding) = binding {
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
                        self.session_slots.update_realization(thread, |r| {
                            r.environment_owner.begin_retirement_with_adopted(
                                &identity,
                                binding,
                                environment.clone(),
                                cause,
                            )
                        })?;
                    } else {
                        // A provider may prove and reconstruct an incomplete
                        // pre-publication realization by the terminal Session
                        // scope. Keep that exact current handle as the existing
                        // Unbound retirement owner; it gains no durable binding
                        // and can only be disposed by this terminal fence.
                        self.begin_session_environment_preparation(thread, environment.clone())?;
                        self.retire_current_environment(
                            thread,
                            cause,
                            RetirementSelection::Exact(&environment),
                        )?
                        .ok_or_else(|| {
                            HostError::unavailable_classified(
                                "session_terminal_environment_owner_changed",
                                "terminal unbound Environment lost its exact owner fence",
                            )
                        })?;
                    }
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
                self.session_slots.update_realization(thread, |r| {
                    r.environment_resource_reconciliation =
                        crate::staged_resources::EnvironmentResourceReconciliation::None;
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
        resolved_resources: Option<&awaken_session_resource_contract::ResolvedSessionResources>,
        mode: BoundEnvironmentPreparationMode,
    ) -> Result<PreparedBoundEnvironment, HostError> {
        if mode.requires_live_source()
            && self
                .session_slots
                .read_realization(thread, |r| {
                    matches!(
                        &r.environment_owner,
                        SessionEnvironmentOwner::Preparing(
                            SessionEnvironmentPreparation::Candidate(_)
                        ) | SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                            owned: RetiringEnvironmentOwner::Unbound(_),
                            ..
                        })
                    )
                })
                .unwrap_or(false)
        {
            return Err(HostError::unavailable_classified(
                "session_terminal_unpublished_candidate",
                "live source preparation cannot consume an unpublished Environment Candidate",
            ));
        }
        let provider = self.projected_session_environment_provider(thread, None)?;
        let (handle, spec, resident, candidate) = self.validated_session_environment_adoption(
            thread,
            binding,
            provider,
            resolved_resources,
            None,
        )?;
        let (resident, committed_candidate) = match candidate {
            Some(candidate) => (
                Some(candidate.environment.clone()),
                candidate.durable_identity().is_some().then_some(candidate),
            ),
            None => (resident, None),
        };
        let observation = provider
            .adoption()
            .observe_effective(
                &spec,
                &handle,
                resident.as_deref(),
                awaken_provisioning_contract::SandboxObservationBasis::Effect {
                    spec: &spec,
                    effect_fence,
                },
                if mode.requires_live_source() {
                    awaken_session_environment_contract::ObservationPurpose::Recovery
                } else {
                    awaken_session_environment_contract::ObservationPurpose::TerminalCleanup
                },
            )
            .await
            .map_err(|error| {
                HostError::unavailable_classified(
                    "session_environment_effect_observation_indeterminate",
                    error.to_string(),
                )
            })?;
        if mode.requires_live_source() {
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
                        .adoption().validate_closed_observation(&handle, observation)
                        .map_err(|error| HostError::internal(error.to_string()))?;
                    if mode == BoundEnvironmentPreparationMode::TerminalRepositoryPublication
                        && matches!(
                            observation,
                            awaken_provisioning_contract::SandboxObservation::DefinitivelyUnavailable { .. }
                                | awaken_provisioning_contract::SandboxObservation::Terminal { .. }
                        )
                    {
                        return Ok(PreparedBoundEnvironment::closed_publication_source(
                            handle,
                            observation.clone(),
                        ));
                    }
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
        let (resident, effect_mode, committed_candidate) = match observation {
            awaken_provisioning_contract::SandboxObservation::Ready => {
                (
                    resident,
                    TerminalEffectMode::LiveUnprepared,
                    committed_candidate,
                )
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
                    .adoption().validate_closed_observation(&handle, &observation)
                    .map_err(|error| HostError::internal(error.to_string()))?;
                if let Some(environment) = resident {
                    let cause = SessionEnvironmentRetirementCause::Terminal {
                        effect_id: effect_fence.operation_id().to_owned(),
                    };
                    if let Some(candidate) = committed_candidate.as_ref() {
                        self.session_slots.update_realization(thread, |r| {
                            r.environment_owner
                                .begin_retirement_with_committed_candidate(candidate, cause)
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
                                    "session_environment_effect_fence_lost",
                                    "lost the exact unavailable Environment owner",
                                )
                            })?;
                        }
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
                (None, TerminalEffectMode::RecoverOnlyUnprepared, None)
            }
            observation @ awaken_provisioning_contract::SandboxObservation::Terminal { .. } => {
                provider
                    .adoption().validate_closed_observation(&handle, &observation)
                    .map_err(|error| HostError::internal(error.to_string()))?;
                (
                    resident,
                    TerminalEffectMode::for_terminal_source(
                        provider.terminal().terminal_data_is_readable(),
                    ),
                    committed_candidate,
                )
            }
            observation @ awaken_provisioning_contract::SandboxObservation::Disposing { .. } => {
                provider
                    .adoption().validate_closed_observation(&handle, &observation)
                    .map_err(|error| HostError::internal(error.to_string()))?;
                // Disposing is durable proof that every source-durability
                // participant required by the preceding operation crossed the
                // provider cleanup gate. It does not fabricate a terminal-scoped
                // Artifact association; the terminal fence below may recover an
                // existing receipt or correctly recover none. Reconstruct only
                // the physical cleanup owner and never reuse a live-I/O wrapper.
                (
                    resident,
                    TerminalEffectMode::RecoverOnlyAlreadyPrepared,
                    committed_candidate,
                )
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
                    committed_candidate: committed_candidate.as_ref(),
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
        let (
            state,
            owner,
            previous_resources,
            workspace_id,
            checkpoint_policy,
            realization_lease,
            environment_fingerprint,
        ) = self
            .session_slots
            .read(thread, |slot| {
                let baseline = slot.frozen.baseline.as_ref();
                let r = &slot.realization;
                (
                    r.terminal_environment_state.clone(),
                    r.environment_owner.clone(),
                    r.previous_resources(),
                    slot.frozen.workspace.clone(),
                    baseline.map(|baseline| baseline.environment.idle_retention.clone()),
                    r.authority.lease().cloned(),
                    baseline.map(|baseline| baseline.environment.config_fingerprint.0.clone()),
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
            awaken_session_state_contract::SessionEnvironmentState::Suspending {
                suspend_phase: awaken_session_state_contract::SuspendPhase::Uploading,
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
        // An Adoption Candidate already knows a durable binding even when its
        // new effect response is absent. Only unknown Create commitment can
        // coexist with Unmaterialized; reuse the owner's provenance decision.
        let unacknowledged = owner.durable_binding().is_none()
            && matches!(
                &owner,
                SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::Candidate(_))
                    | SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                        owned: RetiringEnvironmentOwner::Unbound(_),
                        ..
                    })
            );
        let binding = match &state {
            awaken_session_state_contract::SessionEnvironmentState::Resident {
                binding, ..
            } => Some(binding.as_str()),
            awaken_session_state_contract::SessionEnvironmentState::Suspending {
                source_binding,
                ..
            } => Some(source_binding.as_str()),
            // The frozen terminal assignment, not a fabricated Create/Adopt
            // receipt, may take over a locally retained pre-commit root.
            awaken_session_state_contract::SessionEnvironmentState::Unmaterialized
                if unacknowledged =>
            {
                owner.physical_binding()
            }
            awaken_session_state_contract::SessionEnvironmentState::Unmaterialized
            | awaken_session_state_contract::SessionEnvironmentState::Hibernated { .. } => None,
            awaken_session_state_contract::SessionEnvironmentState::Restoring { .. } => None,
        };
        if matches!(
            &state,
            awaken_session_state_contract::SessionEnvironmentState::Restoring { .. }
        ) {
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
            if matches!(
                &state,
                awaken_session_state_contract::SessionEnvironmentState::Hibernated { .. }
            ) {
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
            let realization_lease = realization_lease.ok_or_else(|| {
                HostError::unavailable_classified(
                    "session_terminal_unmaterialized_lease_missing",
                    "unmaterialized terminal Environment has no realization lease",
                )
            })?;
            let environment_fingerprint = environment_fingerprint.ok_or_else(|| {
                HostError::unavailable_classified(
                    "session_terminal_unmaterialized_fingerprint_missing",
                    "unmaterialized terminal Environment has no frozen fingerprint",
                )
            })?;
            let create_intent = awaken_session_state_contract::SessionEnvironmentEffectIntent::new(
                thread,
                awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
                Some(realization_lease.clone()),
            )
            .for_environment(environment_fingerprint);
            let expected_effect_fence = realization_lease
                .sandbox_effect_fence(create_intent.effect_id())
                .map_err(|error| HostError::internal(error.to_string()))?;
            let provider = self.projected_session_environment_provider(thread, None)?;
            let spec = self.session_environment_adoption_spec(
                thread,
                provider,
                previous_resources.as_ref(),
            );
            let environment = self
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
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
        prepared: &PreparedBoundEnvironment,
    ) -> Result<TerminalMemoryPlan, HostError> {
        let transition = self
            .session_slots
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
                .memory_mounter()
                .ok_or_else(|| HostError::internal("terminal Memory copy has no MemoryMounter"))?;
            for intent in &plan.intents {
                self.terminal_cleanup_effect_fence(effect)?;
                let files = environment
                    .harvest()
                    .list_frozen_mount_files(intent.mount_path())
                    .await
                    .map_err(|error| HostError::unavailable(error.to_string()))?;
                self.terminal_cleanup_effect_fence(effect)?;
                let operation_reference = if self.authority.fetches_resources_upstream() {
                    awaken_resource_contract::MemoryMaterializationReferenceEncoder::<
                        awaken_session_state_contract::SessionTerminalMemoryIntent,
                    >::encode(
                        self.authority.memory_reference_encoder().ok_or_else(|| {
                            HostError::internal("remote terminal Memory encoder is not configured")
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
                self.terminal_cleanup_effect_fence(effect)?;
                mounter
                    .reconcile_recovered_copy(
                        &operation_reference,
                        intent.materialization(),
                        &files,
                        crate::managed_input_layout::managed_mount_access(intent.access()),
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
        let authority = self.session_slots.provider_effect_authority(
            effect.command.session_id.as_str(),
            &effect_fence,
            self.execution_authority.as_ref(),
        )?;
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
        let terminal_effect_fence = self.terminal_cleanup_effect_fence(effect)?;
        let authority = self.session_slots.provider_effect_authority(
            effect.command.session_id.as_str(),
            &terminal_effect_fence,
            self.execution_authority.as_ref(),
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
