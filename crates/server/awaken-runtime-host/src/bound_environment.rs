// @formal evidence session_environment.owner_topology_is_exact_and_frozen
// @formal evidence session_environment.source_disposal_requires_a_verified_checkpoint
// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Recover the exact bound Environment for one authorized physical effect.
//!
//! Provider observation, adoption and terminal reconstruction share the original
//! Session slot and lifecycle guard. This executor owns no additional state.

use crate::environment_owner::{
    BoundSessionEnvironmentIdentity, RetiringEnvironmentOwner, RetiringSessionEnvironment,
    SessionEnvironmentOwner, SessionEnvironmentPreparation, SessionEnvironmentRetirementCause,
};
use crate::host_error::HostError;
use crate::realization_executor::RealizationExecutor;
use crate::session_environment::{
    BoundEnvironmentPreparationMode, PreparedBoundEnvironment, TerminalEffectMode,
    TerminalEnvironmentInstallation,
};
use awaken_session_configuration_contract::resource as session_resource;
use std::sync::Arc;

impl RealizationExecutor {
    /// Re-open one exact terminal substrate into the process-local teardown
    /// slot. This helper deliberately writes no ordinary Environment receipt:
    /// the surrounding terminal command is the sole durable effect authority.
    pub(crate) async fn install_bound_environment_under_lifecycle(
        &self,
        installation: TerminalEnvironmentInstallation<'_>,
        resident: Option<Arc<crate::session_environment::SessionEnvironment>>,
        mode: BoundEnvironmentPreparationMode,
    ) -> Result<Option<(Arc<crate::session_environment::SessionEnvironment>, bool)>, HostError>
    {
        let sessions = &self.environments.sessions;
        let cleanup = &self.cleanup;
        let exec = &self.environments;
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
        let authority = self
            .execution
            .provider_effect_authority(thread, effect_fence)?;
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
            && let Some(retiring) = sessions
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
            sessions.update_realization(thread, |r| {
                r.environment_owner.prepare_retiring_realization_adoption(
                    &retiring,
                    fresh.clone(),
                    None,
                )?;
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
            && let Some(retiring) = sessions
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
                        exec.begin_session_environment_adoption(thread, environment.clone(), None)?;
                    let crate::environment_owner::UnboundSessionEnvironmentOrigin::AdoptionFrom(
                        identity,
                    ) = &candidate.origin
                    else {
                        return Err(HostError::internal(
                            "live source adoption has no durable Environment identity",
                        ));
                    };
                    exec.publish_prepared_session_environment(
                        thread,
                        &candidate,
                        identity.clone(),
                    )?;
                } else {
                    let still_resident = sessions
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
                        && cleanup.retained_preparation_owner(thread, &environment);
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
                    sessions.update_realization(thread, |r| {
                        r.environment_owner
                            .begin_retirement_with_committed_candidate(candidate, cause)
                    })?;
                } else if adopted {
                    if let Some(binding) = binding {
                        let (identity, pending_binding) =
                            exec.pending_environment_adoption(thread).ok_or_else(|| {
                                HostError::internal(
                                    "terminal bound Environment has no pending durable owner",
                                )
                            })?;
                        if pending_binding != binding {
                            return Err(HostError::internal(
                                "terminal bound Environment changed its durable binding",
                            ));
                        }
                        sessions.update_realization(thread, |r| {
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
                        exec.begin_session_environment_preparation(
                            thread,
                            environment.clone(),
                            None,
                        )?;
                        exec.retire_exact_environment(
                            thread,
                            cause,
                            &environment,
                            "session_terminal_environment_owner_changed",
                            "terminal unbound Environment lost its exact owner fence",
                        )?;
                    }
                } else {
                    let retained_terminal_preparation = mode
                        == BoundEnvironmentPreparationMode::TerminalDisposal
                        && cleanup.retained_preparation_owner(thread, &environment);
                    if !retained_terminal_preparation {
                        exec.retire_exact_environment(
                            thread,
                            cause,
                            &environment,
                            "session_terminal_environment_owner_changed",
                            "terminal Environment lost its exact owner fence",
                        )?;
                    }
                }
                sessions.update_realization(thread, |r| {
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
        resolved_resources: Option<&session_resource::ResolvedSessionResources>,
        mode: BoundEnvironmentPreparationMode,
    ) -> Result<PreparedBoundEnvironment, HostError> {
        let realizations = self.sessions.clone();
        let exec = self;
        if mode.requires_live_source()
            && realizations
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
        let (handle, spec, resident, candidate) = exec.validated_session_environment_adoption(
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
                        realizations.update_realization(thread, |r| {
                            r.environment_owner
                                .begin_retirement_with_committed_candidate(candidate, cause)
                        })?;
                    } else {
                        let retained_terminal_preparation = mode
                            == BoundEnvironmentPreparationMode::TerminalDisposal
                            && self.cleanup.retained_preparation_owner(thread, &environment);
                        if !retained_terminal_preparation {
                            exec.retire_exact_environment(thread, cause, &environment, "session_environment_effect_fence_lost", "lost the exact unavailable Environment owner")?;
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
}
