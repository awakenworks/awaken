//! Two-stage terminal cleanup orchestration and process-local acknowledgement.
//!
//! Durable phase authority remains in the Session aggregate. This private split
//! executes its exact Preparation and Disposal effects and retires only the
//! corresponding Worker projection after Control accepts the receipt.

use super::*;

impl SharedHost {
    /// Complete every source-dependent participant under one exact aggregate
    /// preparation fence without deleting the physical realization. The pure
    /// Memory evidence join precedes MCP, Artifact, Memory, checkpoint, and
    /// provider preparation in that order; any failure retains the substrate
    /// and returns no durable preparation receipt.
    pub(crate) async fn prepare_terminal_cleanup_effect(
        &self,
        effect: awaken_session_contract::SessionTerminalCleanupEffect,
        authorization: awaken_session_contract::SessionTerminalCleanupPreparationAuthorization,
    ) -> Result<awaken_session_contract::SessionCleanupPreparation, HostError> {
        authorization.verify_for(&effect).map_err(|error| {
            HostError::unavailable_classified(
                "session_terminal_cleanup_preparation_unauthorized",
                error.to_string(),
            )
        })?;
        let workspace_id = self
            .registered_thread_workspace(&effect.command.session_id)
            .ok_or_else(|| {
                HostError::unavailable_classified(
                    "session_terminal_cleanup_workspace_missing",
                    "terminal cleanup has no installed Workspace projection",
                )
            })?;
        if workspace_id != authorization.workspace_id() {
            return Err(HostError::classified(
                "session_terminal_cleanup_workspace_mismatch",
                "terminal cleanup authorization belongs to another Workspace",
            ));
        }
        let root_realization = self
            .session_slots
            .read(&effect.command.session_id, |slot| slot.realization.clone())
            .ok_or_else(|| {
                HostError::unavailable_classified(
                    "session_terminal_cleanup_projection_missing",
                    "terminal cleanup has no installed root projection",
                )
            })?;
        let _root_realization = root_realization.lock().await;
        let lifecycle = self
            .session_slots
            .update(&effect.command.thread_id, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        let effect_fence = self.terminal_cleanup_effect_fence(&effect)?;

        let root_environment = if effect.command.thread_id == effect.command.session_id {
            let mut prepared = self
                .prepare_terminal_environment_under_lifecycle(
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
        if let Some(prepared) = root_environment.as_ref() {
            let expected_restore = prepared
                .state
                .restoring_request(&workspace_id, &effect.command.session_id);
            if expected_restore.as_ref() != effect.command.restore_target.as_ref() {
                return Err(HostError::unavailable_classified(
                    "session_terminal_restore_target_mismatch",
                    "terminal preparation does not carry the aggregate's exact restore target",
                ));
            }
        } else if effect.command.restore_target.is_some() {
            return Err(HostError::internal(
                "a child terminal cleanup command cannot own a restore target",
            ));
        }
        let memory_plan = root_environment
            .as_ref()
            .map(|prepared| self.terminal_memory_plan(&effect, &prepared.environment))
            .transpose()?;

        self.session_slots
            .update(&effect.command.thread_id, |slot| {
                slot.terminal_cleanup_root = Some(effect.command.session_id.clone());
            });

        let primary_active = self
            .session_slots
            .read(&effect.command.thread_id, |slot| {
                slot.runtime.as_ref().is_some_and(|runtime| {
                    runtime
                        .active_run
                        .lock()
                        .expect("active run mutex poisoned")
                        .is_some()
                })
            })
            .unwrap_or(false);
        let delegated = self
            .delegated_runs_under_lifecycle(&effect.command.thread_id)
            .await?;
        let delegated_active = delegated.iter().any(|run| {
            run.status.occupies_parallel_slot()
                && self
                    .session_slots
                    .read(&run.run_id.0, |slot| {
                        slot.runtime.as_ref().is_some_and(|runtime| {
                            runtime
                                .active_run
                                .lock()
                                .expect("active run mutex poisoned")
                                .is_some()
                        })
                    })
                    .unwrap_or(false)
        });
        if primary_active || delegated_active {
            return Err(HostError::unavailable_classified(
                "session_terminal_environment_not_quiescent",
                "Primary or delegated work still owns the terminal Environment",
            ));
        }
        if let Some(owned) = self
            .session_slots
            .read(&effect.command.thread_id, |slot| {
                slot.environment_owner.terminal_bound_environment()
            })
            .flatten()
        {
            let generation_id = owned.activity_generation_id();
            if !self
                .memory
                .background()
                .quiesce_shared_environment(
                    &effect.command.thread_id,
                    &generation_id,
                    std::time::Duration::from_secs(30),
                )
                .await
            {
                return Err(HostError::unavailable_classified(
                    "session_terminal_background_not_quiescent",
                    "Shared-Environment background work did not reach a durable boundary",
                ));
            }
            let retained = self
                .session_slots
                .read(&effect.command.thread_id, |slot| {
                    slot.environment_owner
                        .terminal_bound_environment()
                        .is_some_and(|current| current.exact_matches(&owned))
                })
                .unwrap_or(false);
            if !retained {
                return Err(HostError::unavailable_classified(
                    "session_terminal_environment_owner_changed",
                    "terminal Environment owner changed while background work quiesced",
                ));
            }
        }
        self.terminal_cleanup_effect_fence(&effect)?;
        self.drain_mcp_projections(&effect.command.thread_id, &[])
            .await
            .map_err(|error| {
                HostError::unavailable_classified(
                    "session_terminal_mcp_not_quiescent",
                    error.to_string(),
                )
            })?;
        if let Some((environment, _)) = root_environment
            .as_ref()
            .filter(|prepared| prepared.environment.permits_live_io())
            .and_then(|prepared| prepared.environment.environment.as_ref())
        {
            environment.stop_bound_processes().await.map_err(|error| {
                HostError::unavailable_classified(
                    "session_terminal_hand_not_quiescent",
                    error.to_string(),
                )
            })?;
        }
        self.terminal_cleanup_effect_fence(&effect)?;
        let artifact_capture_mode = root_environment
            .as_ref()
            .map(TerminalEnvironmentPreparation::artifact_capture_mode)
            .unwrap_or(Some(crate::provisioning::ArtifactCaptureMode::Live));
        let artifact_receipts = if let Some(capture_mode) = artifact_capture_mode {
            let harvester = self.artifact_harvester();
            let fence = Some(awaken_run_ingress::ArtifactPublicationFence::Terminal(
                effect.clone(),
            ));
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
                (crate::provisioning::ArtifactCaptureMode::Live, Some((environment, _))) => {
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
                    HostError::unavailable_classified(
                        "session_terminal_artifact_harvest_pending",
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
                self.terminal_cleanup_effect_fence(&effect)?;
                self.reconcile_terminal_memory(
                    &effect,
                    environment.as_ref(),
                    effect_mode,
                    memory_plan.as_ref().ok_or_else(|| {
                        HostError::internal("terminal root has no pure Memory plan")
                    })?,
                )
                .await?;
            }
            // A committed checkpoint is independent durable state and can be
            // deleted without reading the Sandbox. Upload-response recovery
            // remains on the checkpoint port's existing terminal edge. Neither
            // case is a reason to skip cleanup after a provider Disposing gate.
            self.cleanup_terminal_checkpoint(
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
                let provider_effect_fence = self.terminal_cleanup_effect_fence(&effect)?;
                provider_prepared_effect_fence = Some(
                    environment
                        .prepare_disposal_for_effect(&provider_effect_fence)
                        .await
                        .map_err(|error| HostError::unavailable(error.to_string()))?,
                );
            }
        }

        // Protect even a no-Environment preparation from reporting success
        // after the terminal owner changed while source effects were in flight.
        let final_effect_fence = self.terminal_cleanup_effect_fence(&effect)?;
        awaken_session_contract::SessionCleanupPreparation::try_new(
            &effect,
            provider_prepared_effect_fence.unwrap_or(final_effect_fence),
            artifact_receipts,
        )
        .map_err(|error| {
            HostError::unavailable_classified(
                "session_terminal_cleanup_provider_preparation_invalid",
                error.to_string(),
            )
        })
    }

    /// Execute only the aggregate-wide destructive half after every exact
    /// preparation is durable. Effect-free provider observation/reconstruction
    /// may recover the physical owner; Hand, Artifact, Memory, checkpoint, and
    /// provider preparation are forbidden on this path.
    pub(crate) async fn dispose_terminal_cleanup_effect(
        &self,
        effect: awaken_session_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<awaken_session_contract::SessionCleanupDisposalReceipt, HostError> {
        let session_id = &effect.command.session_id;
        let root_realization = self
            .session_slots
            .read(session_id, |slot| slot.realization.clone())
            .ok_or_else(|| {
                HostError::unavailable_classified(
                    "session_terminal_cleanup_projection_missing",
                    "terminal cleanup has no installed root projection",
                )
            })?;
        let _root_realization = root_realization.lock().await;
        self.terminal_cleanup_disposal_authorization(&effect)?;
        let lifecycle = self
            .session_slots
            .update(session_id, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        let authorization = self.terminal_cleanup_disposal_authorization(&effect)?;
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
                    session_id,
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
        let owner = self
            .session_slots
            .read(session_id, |slot| slot.environment_owner.clone())
            .ok_or_else(|| {
                HostError::unavailable_classified(
                    "session_terminal_cleanup_projection_missing",
                    "terminal disposal lost its Environment owner projection",
                )
            })?;
        match (owner, effect.command.restore_target.as_ref()) {
            (
                SessionEnvironmentOwner::Restoring(
                    crate::session_slot::SessionEnvironmentRestoration::Awaiting { request },
                ),
                Some(expected),
            ) if &request == expected => {
                let provider = self.projected_session_environment_provider(session_id, None)?;
                let previous_resources = self
                    .session_slots
                    .read(session_id, |slot| {
                        slot.resource_transition
                            .as_ref()
                            .map(|transition| transition.previous().resources.clone())
                    })
                    .flatten();
                let spec = self.session_environment_adoption_spec(
                    session_id,
                    provider,
                    previous_resources.as_ref(),
                );
                provider
                    .dispose_restored(&spec, expected)
                    .await
                    .map_err(|error| HostError::unavailable(error.to_string()))?;
                // Keep the exact Restoring request as the response-loss fence.
                // The provider port treats proved absence as success, so an
                // identical Disposal can replay until aggregate acknowledgement
                // retires the whole terminal projection.
            }
            (SessionEnvironmentOwner::Restoring(_), _) | (_, Some(_)) => {
                return Err(HostError::unavailable_classified(
                    "session_terminal_restore_target_changed",
                    "terminal disposal does not match its exact Restoring owner",
                ));
            }
            (SessionEnvironmentOwner::Retiring(retirement), None) => {
                if !matches!(
                    retirement.cause,
                    SessionEnvironmentRetirementCause::Terminal { .. }
                ) {
                    return Err(HostError::unavailable_classified(
                        "session_terminal_environment_preparation_changed",
                        "terminal disposal found another retirement operation",
                    ));
                }
                let RetiringEnvironmentOwner::Bound(_) = &retirement.owned else {
                    return Err(HostError::unavailable_classified(
                        "session_terminal_unpublished_candidate",
                        "terminal disposal cannot consume an unpublished Environment",
                    ));
                };
                if !provider_proved_absence {
                    let environment = retirement.owned.environment();
                    let exact_authorization =
                        self.terminal_cleanup_disposal_authorization(&effect)?;
                    if exact_authorization != authorization {
                        return Err(HostError::unavailable_classified(
                            "session_terminal_cleanup_disposal_changed",
                            "terminal disposal authorization changed under its lifecycle lock",
                        ));
                    }
                    environment
                        .dispose_for_effect(&authorization)
                        .await
                        .map_err(|error| HostError::unavailable(error.to_string()))?;
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
                return Err(HostError::unavailable_classified(
                    "session_terminal_environment_not_prepared",
                    "physical disposal requires the exact retained Preparation owner",
                ));
            }
        }
        self.session_slots.update(session_id, |slot| {
            slot.runtime = None;
            slot.environment_resource_reconciliation =
                crate::session_slot::EnvironmentResourceReconciliation::None;
        });
        self.terminal_cleanup_disposal_authorization(&effect)?;
        Ok(awaken_session_contract::SessionCleanupDisposalReceipt::new(
            &effect.command,
        ))
    }

    /// Retire only a prepared child after Control durably accepts its receipt.
    /// The root owns the shared Environment and must remain until disposal.
    pub(crate) async fn acknowledge_terminal_cleanup_preparation(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupEffect,
    ) {
        if effect.command.thread_id == effect.command.session_id {
            return;
        }
        self.retire_terminal_projection_for_generation(
            &effect.command.session_id,
            &effect.lease,
            Some(&effect.command.thread_id),
        )
        .await;
    }

    /// Forget the exact root teardown projection only after Control durably
    /// accepts the physical-disposal receipt. This edge performs no I/O.
    pub(crate) async fn acknowledge_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupDisposalEffect,
    ) {
        self.retire_terminal_projection_for_generation(
            &effect.command.session_id,
            &effect.lease,
            None,
        )
        .await;
    }

    /// Close aggregate-completion response loss through the existing projection
    /// retirement owner; this is an acknowledgement, never another cleanup path.
    pub(crate) async fn acknowledge_completed_terminal_cleanup(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) {
        self.retire_completed_terminal_cleanup_projection(session_id, lease)
            .await;
    }

    /// Retire a retained terminal projection after Control reports that the
    /// durable cleanup operation is already complete. This is the response-loss
    /// readback of the same process-local acknowledgement above, not a second
    /// cleanup command or receipt source.
    pub(crate) async fn retire_completed_terminal_cleanup_projection(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) -> bool {
        let projected_terminal = self
            .session_slots
            .read(session_id, |slot| slot.terminal_environment_state.is_some())
            .unwrap_or(false);
        if !projected_terminal {
            return false;
        }
        self.retire_terminal_projection_for_generation(session_id, lease, None)
            .await
    }

    async fn retire_terminal_projection_for_generation(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
        target: Option<&str>,
    ) -> bool {
        let Some(root_realization) = self
            .session_slots
            .read(session_id, |slot| slot.realization.clone())
        else {
            return false;
        };
        let _root_realization = root_realization.lock().await;
        let authorized = self
            .session_slots
            .read(session_id, |slot| {
                slot.realization_lease.as_ref().is_some_and(|current| {
                    awaken_session_contract::realization_lease_generation_authorizes(current, lease)
                })
            })
            .unwrap_or(false);
        if !authorized {
            return false;
        }

        let mut targets = if target.is_none() || target == Some(session_id) {
            self.session_slots
                .session_ids()
                .into_iter()
                .filter(|thread_id| {
                    thread_id == session_id
                        || self
                            .session_slots
                            .read(thread_id, |slot| {
                                slot.terminal_cleanup_root.as_deref() == Some(session_id)
                            })
                            .unwrap_or(false)
                })
                .collect::<Vec<_>>()
        } else {
            target.map(str::to_owned).into_iter().collect()
        };
        // Remove the root last so its realization generation remains available
        // as the exact fence while every child projection is retired.
        targets.sort_by_key(|thread_id| thread_id == session_id);
        let mut removed = false;
        for thread_id in targets {
            let target_lifecycle = self
                .session_slots
                .read(&thread_id, |slot| slot.lifecycle.clone());
            if let Some(lifecycle) = target_lifecycle {
                let _target_lifecycle = lifecycle.lock().await;
                let authorized = self
                    .session_slots
                    .read(session_id, |slot| {
                        slot.realization_lease.as_ref().is_some_and(|current| {
                            awaken_session_contract::realization_lease_generation_authorizes(
                                current, lease,
                            )
                        })
                    })
                    .unwrap_or(false);
                if !authorized {
                    return removed;
                }
                let belongs_to_operation = thread_id == session_id
                    || self
                        .session_slots
                        .read(&thread_id, |slot| {
                            slot.terminal_cleanup_root.as_deref() == Some(session_id)
                        })
                        .unwrap_or(false);
                if !belongs_to_operation {
                    continue;
                }
                if let Some(relay) = self.mcp_relay.get() {
                    relay.remove_routes(&thread_id);
                }
                removed |= self.session_slots.remove(&thread_id).is_some();
                continue;
            }
            let authorized = self
                .session_slots
                .read(session_id, |slot| {
                    slot.realization_lease.as_ref().is_some_and(|current| {
                        awaken_session_contract::realization_lease_generation_authorizes(
                            current, lease,
                        )
                    })
                })
                .unwrap_or(false);
            if !authorized {
                return removed;
            }
            let belongs_to_operation = thread_id == session_id
                || self
                    .session_slots
                    .read(&thread_id, |slot| {
                        slot.terminal_cleanup_root.as_deref() == Some(session_id)
                    })
                    .unwrap_or(false);
            if !belongs_to_operation {
                continue;
            }
            if let Some(relay) = self.mcp_relay.get() {
                relay.remove_routes(&thread_id);
            }
            removed |= self.session_slots.remove(&thread_id).is_some();
        }
        removed
    }
}
