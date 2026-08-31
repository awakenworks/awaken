//! Session Environment publication, adoption, fencing, and retirement.
//!
//! This module is the sole writer of [`SessionEnvironmentOwner`]. Durable
//! lifecycle truth remains in the Session aggregate; the slot machine retains
//! each physical wrapper across every fallible or cancellable provider call.

use super::*;
use crate::session_slot::{
    BoundSessionEnvironment, BoundSessionEnvironmentIdentity, LegacyDirectEnvironmentProvenance,
    RetiringEnvironmentOwner, RetiringSessionEnvironment, SessionEnvironmentOwner,
    SessionEnvironmentPreparation, SessionEnvironmentRetirementCause, UnboundSessionEnvironment,
};

mod publication;
mod state;

use state::{RetirementSelection, checkpoint_source_identity, projected_environment_owner};

/// One atomic process-local retirement transition. The Runtime context is
/// removed from the slot together with its Environment owner and exists only in
/// the caller's stack while provider status is awaited; it is never durable or
/// independently discoverable.
struct LocalRetirementTransition {
    owner: RetiringSessionEnvironment,
    runtime: Option<Arc<SessionCtx>>,
}

impl SharedHost {
    pub(crate) async fn session_environment(
        &self,
        thread: &str,
    ) -> Option<Arc<crate::session_environment::SessionEnvironment>> {
        self.session_slots
            .read(thread, |slot| slot.environment_owner.resident())
            .flatten()
    }

    pub(crate) fn session_environment_owner_is_vacant(&self, thread: &str) -> bool {
        self.session_slots
            .read(thread, |slot| {
                matches!(slot.environment_owner, SessionEnvironmentOwner::Vacant)
            })
            .unwrap_or(true)
    }

    pub(crate) fn durable_session_environment_binding(&self, thread: &str) -> Option<String> {
        self.session_slots
            .read(thread, |slot| {
                slot.environment_owner.durable_binding().map(str::to_owned)
            })
            .flatten()
    }

    pub(crate) fn frozen_session_environment_provisioning(
        &self,
        thread: &str,
    ) -> Result<awaken_runtime_contract::resolved::ModelProvisioning, HostError> {
        self.session_slots
            .read(thread, |slot| {
                slot.published_snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.resolved_spec.model_binding.provisioning().clone())
            })
            .flatten()
            .ok_or_else(|| {
                HostError::internal(format!(
                    "Session {thread} has no exact frozen Environment provider"
                ))
            })
    }

    fn validate_frozen_session_environment_provisioning(
        &self,
        thread: &str,
        asserted: &awaken_runtime_contract::resolved::ModelProvisioning,
    ) -> Result<awaken_runtime_contract::resolved::ModelProvisioning, HostError> {
        let frozen = self.frozen_session_environment_provisioning(thread)?;
        if &frozen != asserted {
            return Err(HostError::internal(
                "Session Environment provider differs from its frozen publication",
            ));
        }
        Ok(frozen)
    }

    fn validate_suspend_operation(
        &self,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        _generation: &awaken_session_contract::SandboxGeneration,
    ) -> Result<(), HostError> {
        // The Session aggregate is the sole operation writer. Existing durable
        // operations, including legacy fingerprints, replay opaquely; Runtime
        // validates the current realization fence and exact source owner rather
        // than deriving a competing effect identity.
        let current = self
            .session_slots
            .read(thread, |slot| slot.realization_lease.clone())
            .flatten();
        let now_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        let authorized = match (current.as_ref(), operation.realization.as_ref()) {
            (Some(current), Some(asserted)) => {
                awaken_session_contract::realization_lease_authorizes(
                    current,
                    asserted,
                    now_unix_ms,
                )
            }
            (None, None) => true,
            _ => false,
        };
        if !authorized {
            return Err(HostError::internal(
                "checkpoint continuation operation lost its exact realization lease",
            ));
        }
        Ok(())
    }

    pub(crate) fn resident_checkpoint_source_environment(
        &self,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        source_effect_id: &str,
        source_binding: &str,
        generation: &awaken_session_contract::SandboxGeneration,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, HostError> {
        self.validate_suspend_operation(thread, operation, generation)?;
        let expected_identity =
            checkpoint_source_identity(source_effect_id, source_binding, generation);
        self.session_slots
            .read(thread, |slot| match &slot.environment_owner {
                SessionEnvironmentOwner::Resident(owned)
                    if owned.identity == expected_identity
                        && owned.binding == source_binding
                        && serde_json::to_string(&owned.environment.handle())
                            .is_ok_and(|binding| binding == source_binding) =>
                {
                    Some(owned.environment.clone())
                }
                _ => None,
            })
            .flatten()
            .ok_or_else(|| {
                HostError::internal(
                    "checkpoint continuation does not own its exact resident source",
                )
            })
    }

    #[cfg(test)]
    pub(crate) async fn session_environment_handle(
        &self,
        thread: &str,
    ) -> Option<awaken_provisioning_contract::SandboxHandle> {
        self.session_environment(thread)
            .await
            .map(|env| env.handle())
    }

    /// Install only the process-local projection of durable Environment truth.
    /// It may supply an adoption expectation or the exact request-bound restore
    /// fence; it never creates a physical Environment or owns a restored handle.
    pub(crate) fn install_session_environment_owner_projection(
        &self,
        thread: &str,
        workspace_id: &str,
        state: &awaken_session_contract::SessionEnvironmentState,
    ) -> Result<(), HostError> {
        let projected = projected_environment_owner(state, workspace_id, thread)?;
        self.session_slots.update(thread, |slot| {
            slot.environment_owner.install_projection(projected)?;
            slot.reopen_mcp_realization_admission_from_projection(state);
            Ok(())
        })
    }

    /// Resolve the shared-background/quiescence key from the sole exact
    /// process-local Environment owner. The durable generation remains owned by
    /// the Session aggregate; this method only projects it into Runtime.
    pub(crate) fn resident_environment_activity_generation_id(
        &self,
        thread: &str,
        environment: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<String, HostError> {
        self.session_slots
            .read(thread, |slot| {
                slot.environment_owner
                    .resident_activity_generation_id(environment)
            })
            .flatten()
            .ok_or_else(|| {
                HostError::internal(
                    "Runtime Environment does not match its exact Resident activity owner",
                )
            })
    }

    #[cfg(test)]
    pub(crate) fn install_test_resident_session_environment(
        &self,
        thread: &str,
        environment: Arc<crate::session_environment::SessionEnvironment>,
    ) {
        let binding = serde_json::to_string(&environment.handle())
            .expect("test Environment handle serializes");
        let receipt = awaken_session_contract::SessionEnvironmentReceipt::new(
            thread,
            awaken_session_contract::SessionEnvironmentEffectKind::Create,
            binding.clone(),
            None,
        );
        self.session_slots.update(thread, |slot| {
            slot.environment_owner = SessionEnvironmentOwner::Resident(BoundSessionEnvironment {
                identity: BoundSessionEnvironmentIdentity::LegacyDirect(
                    LegacyDirectEnvironmentProvenance::Direct(receipt),
                ),
                binding,
                environment,
            });
        });
    }

    pub(crate) fn begin_session_environment_restore(
        &self,
        thread: &str,
        request: &awaken_session_contract::SandboxRestoreRequest,
    ) -> Result<(), HostError> {
        self.session_slots
            .update(thread, |slot| slot.environment_owner.begin_restore(request))
    }

    pub(crate) fn complete_session_environment_restore_target_disposal(
        &self,
        thread: &str,
        request: &awaken_session_contract::SandboxRestoreRequest,
    ) -> Result<(), HostError> {
        self.session_slots.update(thread, |slot| {
            slot.environment_owner
                .complete_restore_target_disposal(request)
        })
    }

    /// Resolve an opaque durable binding through the one Session environment
    /// provider. Retiring owners are never tool-visible; an authorized adoption
    /// may reactivate the exact Ready owner, while rebuild clears only a
    /// provider-confirmed Terminated owner.
    pub(crate) async fn adopt_bound_session_environment(
        &self,
        thread: &str,
        encoded: Option<&str>,
        provisioning: &awaken_runtime_contract::resolved::ModelProvisioning,
        rebuild_unavailable: bool,
    ) -> Result<(Option<crate::session_environment::SessionEnvironment>, bool), HostError> {
        let Some(encoded) = encoded else {
            return Ok((None, false));
        };
        let handle: awaken_provisioning_contract::SandboxHandle = serde_json::from_str(encoded)
            .map_err(|error| {
                HostError::internal(format!("invalid Session sandbox binding: {error}"))
            })?;
        if handle.sandbox_id != thread {
            return Err(HostError::internal(format!(
                "sandbox {} does not belong to Session {thread}",
                handle.sandbox_id
            )));
        }
        let lifecycle = self
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        if let Some(environment) = self.session_environment(thread).await {
            if environment.handle() != handle {
                return Err(HostError::internal(format!(
                    "Session {thread} is already bound to a different sandbox"
                )));
            }
            if rebuild_unavailable {
                let retired = self
                    .retire_current_environment(
                        thread,
                        SessionEnvironmentRetirementCause::RecoveryDiscard,
                        RetirementSelection::Exact(&environment),
                    )?
                    .ok_or_else(|| {
                        HostError::internal(format!(
                            "lost the sandbox recovery fence for Session {thread}"
                        ))
                    })?;
                return match environment.status().await {
                    Ok(awaken_provisioning_contract::SandboxStatus::Ready) => {
                        self.reactivate_retired_environment(thread, &retired)?;
                        Ok((None, false))
                    }
                    Ok(awaken_provisioning_contract::SandboxStatus::Terminated) => {
                        self.validate_frozen_session_environment_provisioning(
                            thread,
                            provisioning,
                        )?;
                        if !self.confirm_terminated_retirement(thread, &retired.owner) {
                            return Err(HostError::internal(
                                "terminated recovery owner lost its exact Retiring fence",
                            ));
                        }
                        Ok((None, true))
                    }
                    Ok(status) => Err(HostError::internal(format!(
                        "Session sandbox {} is not ready ({status:?})",
                        handle.sandbox_id
                    ))),
                    Err(error) => Err(HostError::internal(format!(
                        "could not inspect Session sandbox {}: {error}",
                        handle.sandbox_id
                    ))),
                };
            }
            return match environment.status().await {
                Ok(awaken_provisioning_contract::SandboxStatus::Ready) => Ok((None, false)),
                Ok(status) => Err(HostError::internal(format!(
                    "Session sandbox {} is not ready ({status:?})",
                    handle.sandbox_id
                ))),
                Err(error) => Err(HostError::internal(format!(
                    "could not inspect Session sandbox {}: {error}",
                    handle.sandbox_id
                ))),
            };
        }
        if let Some(candidate) = self.prepared_session_environment(thread) {
            if candidate.binding != encoded {
                return Err(HostError::internal(format!(
                    "Session {thread} is already preparing a different sandbox"
                )));
            }
            if candidate.requires_initial_provisioning() {
                self.complete_prepared_session_environment(thread, &candidate, false)
                    .await?;
                return Ok((None, false));
            }
            self.validate_frozen_session_environment_provisioning(thread, provisioning)?;
            return self
                .complete_adopted_session_environment_candidate(
                    thread,
                    &candidate,
                    &handle,
                    rebuild_unavailable,
                )
                .await;
        }
        let retiring = self
            .session_slots
            .read(thread, |slot| match &slot.environment_owner {
                SessionEnvironmentOwner::Retiring(retiring)
                    if retiring.owned.binding() == encoded =>
                {
                    Some(retiring.clone())
                }
                _ => None,
            })
            .flatten();
        if let Some(retiring) = retiring {
            let environment = retiring.owned.environment();
            return match environment.status().await {
                Ok(awaken_provisioning_contract::SandboxStatus::Ready) => {
                    self.session_slots.update(thread, |slot| {
                        slot.environment_owner.reactivate_retiring(&retiring)
                    })?;
                    Ok((None, false))
                }
                Ok(awaken_provisioning_contract::SandboxStatus::Terminated)
                    if rebuild_unavailable =>
                {
                    self.validate_frozen_session_environment_provisioning(thread, provisioning)?;
                    if !self.confirm_terminated_retirement(thread, &retiring) {
                        return Err(HostError::internal(
                            "terminated recovery owner lost its exact Retiring fence",
                        ));
                    }
                    Ok((None, true))
                }
                Ok(status) => Err(HostError::internal(format!(
                    "Session sandbox {} is not ready ({status:?})",
                    handle.sandbox_id
                ))),
                Err(error) => Err(HostError::internal(format!(
                    "could not inspect Session sandbox {}: {error}",
                    handle.sandbox_id
                ))),
            };
        }
        if let Some((_, binding)) = self.pending_environment_adoption(thread) {
            if binding != encoded {
                return Err(HostError::internal(format!(
                    "Session {thread} is awaiting a different durable sandbox"
                )));
            }
        } else if !self.session_environment_owner_is_vacant(thread) {
            return Err(HostError::internal(format!(
                "Session {thread} has a non-adoptable Environment owner phase"
            )));
        }
        let frozen_provisioning =
            self.validate_frozen_session_environment_provisioning(thread, provisioning)?;
        let provider = self.session_environment_provider(&frozen_provisioning)?;
        let spec = self.sandbox_spec(thread);
        let sandbox = provider
            .adopt(&spec, &handle)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        // No await is permitted between provider return and this ownership
        // transition. Cancellation from the next poll retains the exact Arc.
        let candidate = self.begin_session_environment_adoption(thread, Arc::new(sandbox))?;
        self.complete_adopted_session_environment_candidate(
            thread,
            &candidate,
            &handle,
            rebuild_unavailable,
        )
        .await
    }

    /// Retire the process-local Environment retained when a prior realization
    /// was revoked before the legacy/direct Session had published a durable
    /// binding. Quiescence permanently closes its Hand dispatch lifecycle, so a
    /// new claimed attempt must rebuild through the ordinary creation path;
    /// ordinary direct callers cannot use this transition as disposal authority.
    pub(crate) async fn rebuild_claimed_legacy_environment_after_revocation(
        &self,
        thread: &str,
    ) -> Result<(), HostError> {
        let retiring = self
            .session_slots
            .read(thread, |slot| match &slot.environment_owner {
                SessionEnvironmentOwner::Retiring(retiring)
                    if matches!(
                        (&retiring.cause, &retiring.owned),
                        (
                            SessionEnvironmentRetirementCause::RealizationRevocation,
                            RetiringEnvironmentOwner::Bound(BoundSessionEnvironment {
                                identity: BoundSessionEnvironmentIdentity::LegacyDirect(
                                    LegacyDirectEnvironmentProvenance::Direct(_),
                                ),
                                ..
                            }),
                        )
                    ) =>
                {
                    Some(retiring.clone())
                }
                _ => None,
            })
            .flatten();
        let Some(retiring) = retiring else {
            return Ok(());
        };
        let environment = retiring.owned.environment();
        match environment.status().await {
            Ok(awaken_provisioning_contract::SandboxStatus::Ready) => {
                self.dispose_and_confirm_retirement(thread, &retiring)
                    .await?;
                Ok(())
            }
            Ok(awaken_provisioning_contract::SandboxStatus::Terminated) => {
                if !self.confirm_terminated_retirement(thread, &retiring) {
                    return Err(HostError::internal(
                        "terminated legacy recovery owner lost its exact Retiring fence",
                    ));
                }
                Ok(())
            }
            Ok(status) => Err(HostError::internal(format!(
                "Session sandbox {} is not ready ({status:?})",
                environment.handle().sandbox_id,
            ))),
            Err(error) => Err(HostError::internal(format!(
                "could not inspect Session sandbox {}: {error}",
                environment.handle().sandbox_id,
            ))),
        }
    }

    async fn complete_adopted_session_environment_candidate(
        &self,
        thread: &str,
        candidate: &UnboundSessionEnvironment,
        handle: &awaken_provisioning_contract::SandboxHandle,
        rebuild_unavailable: bool,
    ) -> Result<(Option<crate::session_environment::SessionEnvironment>, bool), HostError> {
        if candidate.effect_kind() != awaken_session_contract::SessionEnvironmentEffectKind::Adopt {
            return Err(HostError::internal(
                "provider adoption cannot consume a create candidate",
            ));
        }
        let environment = candidate.environment.clone();
        match environment.status().await {
            Ok(awaken_provisioning_contract::SandboxStatus::Ready) => {
                environment
                    .reconcile_adopted_mounts(&self.thread_session_mounts(thread))
                    .await
                    .map_err(|error| HostError::internal(error.to_string()))?;
                self.complete_prepared_session_environment(thread, candidate, false)
                    .await?;
                Ok((None, false))
            }
            Ok(awaken_provisioning_contract::SandboxStatus::Terminated) if rebuild_unavailable => {
                let retired = self
                    .retire_current_environment(
                        thread,
                        SessionEnvironmentRetirementCause::RecoveryDiscard,
                        RetirementSelection::Exact(&environment),
                    )?
                    .ok_or_else(|| {
                        HostError::internal(
                            "terminated adopted sandbox lost its exact Candidate fence",
                        )
                    })?;
                if !self.confirm_terminated_retirement(thread, &retired.owner) {
                    return Err(HostError::internal(
                        "terminated adopted sandbox lost its exact Retiring fence",
                    ));
                }
                Ok((None, true))
            }
            Ok(status) => Err(HostError::internal(format!(
                "Session sandbox {} is not ready ({status:?})",
                handle.sandbox_id
            ))),
            Err(error) => Err(HostError::internal(format!(
                "could not inspect Session sandbox {}: {error}",
                handle.sandbox_id
            ))),
        }
    }

    fn retire_current_environment(
        &self,
        thread: &str,
        cause: SessionEnvironmentRetirementCause,
        selection: RetirementSelection<'_>,
    ) -> Result<Option<LocalRetirementTransition>, HostError> {
        self.session_slots
            .modify(thread, |slot| {
                let retirement = slot.environment_owner.begin_retirement(cause, selection)?;
                Ok(retirement.map(|owner| LocalRetirementTransition {
                    owner,
                    runtime: slot.runtime.take(),
                }))
            })
            .transpose()
            .map(Option::flatten)
    }

    fn reactivate_retired_environment(
        &self,
        thread: &str,
        transition: &LocalRetirementTransition,
    ) -> Result<(), HostError> {
        self.session_slots
            .modify(thread, |slot| {
                if slot.runtime.is_some() {
                    return Err(HostError::internal(
                        "Session Environment reactivation found a replacement Runtime",
                    ));
                }
                slot.environment_owner
                    .reactivate_retiring(&transition.owner)?;
                slot.runtime = transition.runtime.clone();
                Ok(())
            })
            .unwrap_or_else(|| {
                Err(HostError::internal(
                    "Session Environment reactivation lost its Runtime slot",
                ))
            })
    }

    fn confirm_terminated_retirement(
        &self,
        thread: &str,
        retirement: &RetiringSessionEnvironment,
    ) -> bool {
        self.session_slots
            .modify(thread, |slot| {
                slot.environment_owner.confirm_terminated(retirement)
            })
            .unwrap_or(false)
    }

    fn observe_retirement_status<E>(
        &self,
        thread: &str,
        retirement: &RetiringSessionEnvironment,
        status: Result<awaken_provisioning_contract::SandboxStatus, E>,
    ) -> Result<bool, E> {
        match self.session_slots.modify(thread, |slot| {
            slot.environment_owner
                .observe_retirement_status(retirement, status)
        }) {
            Some(observed) => observed,
            None => Ok(false),
        }
    }

    async fn stop_and_confirm_retirement(
        &self,
        thread: &str,
        retirement: &RetiringSessionEnvironment,
    ) -> Result<bool, HostError> {
        let environment = retirement.owned.environment();
        environment
            .stop_bound_processes()
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        self.observe_retirement_status(thread, retirement, environment.status().await)
            .map_err(|error| HostError::internal(error.to_string()))
    }

    async fn dispose_and_confirm_retirement(
        &self,
        thread: &str,
        retirement: &RetiringSessionEnvironment,
    ) -> Result<(), HostError> {
        let environment = retirement.owned.environment();
        environment
            .dispose()
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        let cleared = self
            .observe_retirement_status(thread, retirement, environment.status().await)
            .map_err(|error| HostError::internal(error.to_string()))?;
        if !cleared {
            return Err(HostError::internal(
                "retired Session Environment still reports a live Sandbox",
            ));
        }
        Ok(())
    }

    /// Dispose one unpublished candidate without ever dropping its slot owner
    /// across provider I/O. Cancellation, provider error, a live status, or an
    /// ABA fence loss leaves `Retiring` available to the same lifecycle retry.
    pub(crate) async fn dispose_unpublished_session_environment(
        &self,
        thread: &str,
        expected: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<(), HostError> {
        let retirement = self
            .retire_current_environment(
                thread,
                SessionEnvironmentRetirementCause::UnpublishedCandidate,
                RetirementSelection::Exact(expected),
            )?
            .ok_or_else(|| {
                HostError::internal(
                    "unpublished Session Environment cleanup lost its exact owner fence",
                )
            })?;
        self.dispose_and_confirm_retirement(thread, &retirement.owner)
            .await
    }

    /// Execute the irreversible checkpoint-source disposal for the exact
    /// durable suspend operation. A missing local Arc with a pending durable
    /// binding is adopted into Retiring before any provider call; Vacant is an
    /// unavailable owner, never a successful no-op.
    pub(crate) async fn dispose_checkpoint_source_environment(
        &self,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        source_effect_id: &str,
        generation: &awaken_session_contract::SandboxGeneration,
        source_binding: &str,
    ) -> Result<(), HostError> {
        self.validate_suspend_operation(thread, operation, generation)?;
        let expected_identity =
            checkpoint_source_identity(source_effect_id, source_binding, generation);
        let cause = SessionEnvironmentRetirementCause::CheckpointSource {
            operation: operation.clone(),
            generation: generation.clone(),
        };
        let owner = self
            .session_slots
            .read(thread, |slot| slot.environment_owner.clone())
            .unwrap_or_default();
        let retirement = match owner {
            SessionEnvironmentOwner::Resident(owned) => {
                if owned.identity != expected_identity
                    || owned.binding != source_binding
                    || !serde_json::to_string(&owned.environment.handle())
                        .is_ok_and(|binding| binding == source_binding)
                {
                    return Err(HostError::internal(
                        "checkpoint source disposal does not match its exact resident owner",
                    ));
                }
                self.retire_current_environment(
                    thread,
                    cause.clone(),
                    RetirementSelection::Exact(&owned.environment),
                )?
                .ok_or_else(|| {
                    HostError::internal(
                        "checkpoint source disposal lost its exact resident owner fence",
                    )
                })?
                .owner
            }
            SessionEnvironmentOwner::Retiring(retirement) => {
                let RetiringEnvironmentOwner::Bound(owned) = &retirement.owned else {
                    return Err(HostError::internal(
                        "checkpoint source disposal cannot consume an unbound retirement",
                    ));
                };
                if retirement.cause != cause
                    || owned.identity != expected_identity
                    || owned.binding != source_binding
                {
                    return Err(HostError::internal(
                        "checkpoint source disposal does not match its exact retained retirement",
                    ));
                }
                retirement
            }
            SessionEnvironmentOwner::Preparing(
                SessionEnvironmentPreparation::AwaitingAdoption { identity, binding },
            ) => {
                if identity != expected_identity || binding != source_binding {
                    return Err(HostError::internal(
                        "checkpoint source disposal does not match its durable pending owner",
                    ));
                }
                self.adopt_pending_environment_for_retirement(
                    thread,
                    &identity,
                    &binding,
                    cause.clone(),
                )
                .await?
            }
            SessionEnvironmentOwner::Vacant => {
                // Prove the exact frozen provider before installing a cold
                // pending owner. The seed then survives cancellation of adopt.
                self.frozen_session_environment_provisioning(thread)?;
                self.session_slots.update(thread, |slot| {
                    slot.environment_owner.seed_pending_adoption(
                        expected_identity.clone(),
                        source_binding.to_string(),
                    )
                })?;
                self.adopt_pending_environment_for_retirement(
                    thread,
                    &expected_identity,
                    source_binding,
                    cause.clone(),
                )
                .await?
            }
            SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::Candidate(_))
            | SessionEnvironmentOwner::Restoring(_) => {
                return Err(HostError::internal(
                    "checkpoint source disposal conflicts with an in-flight Environment owner",
                ));
            }
        };
        self.dispose_and_confirm_retirement(thread, &retirement)
            .await
    }

    /// Adopt one exact durable pending binding directly into Retiring. This is
    /// the only cold cleanup adoption edge shared by checkpoint disposal,
    /// realization revocation, and terminal cleanup. Provider return is
    /// immediately captured by the slot owner before another await may cancel
    /// the caller.
    async fn adopt_pending_environment_for_retirement(
        &self,
        thread: &str,
        identity: &BoundSessionEnvironmentIdentity,
        binding: &str,
        cause: SessionEnvironmentRetirementCause,
    ) -> Result<RetiringSessionEnvironment, HostError> {
        let provisioning = self.frozen_session_environment_provisioning(thread)?;
        let handle: awaken_provisioning_contract::SandboxHandle = serde_json::from_str(binding)
            .map_err(|error| {
                HostError::internal(format!("invalid pending Session sandbox binding: {error}"))
            })?;
        if handle.sandbox_id != thread {
            return Err(HostError::internal(format!(
                "pending sandbox {} does not belong to Session {thread}",
                handle.sandbox_id
            )));
        }
        let adopted = Arc::new(
            self.session_environment_provider(&provisioning)?
                .adopt(&self.sandbox_spec(thread), &handle)
                .await
                .map_err(|error| HostError::internal(error.to_string()))?,
        );
        self.session_slots.update(thread, |slot| {
            slot.environment_owner
                .begin_retirement_with_adopted(identity, binding, adopted, cause)
        })
    }

    /// Forget a dead environment only after its exact Arc/binding/identity and
    /// Retiring phase are still current and the provider reports Terminated.
    #[cfg(test)]
    pub(crate) async fn discard_session_environment(
        &self,
        thread: &str,
        expected: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> bool {
        let lifecycle = self
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        let Ok(Some(retirement)) = self.retire_current_environment(
            thread,
            SessionEnvironmentRetirementCause::RecoveryDiscard,
            RetirementSelection::Exact(expected),
        ) else {
            return false;
        };
        self.stop_and_confirm_retirement(thread, &retirement.owner)
            .await
            .unwrap_or(false)
    }

    fn pending_environment_adoption(
        &self,
        thread: &str,
    ) -> Option<(BoundSessionEnvironmentIdentity, String)> {
        self.session_slots
            .read(thread, |slot| match &slot.environment_owner {
                SessionEnvironmentOwner::Preparing(
                    SessionEnvironmentPreparation::AwaitingAdoption { identity, binding },
                ) => Some((identity.clone(), binding.clone())),
                _ => None,
            })
            .flatten()
    }

    /// Revoke process-local authority without dropping the physical owner. The
    /// wrapper enters Retiring before stop/status and stays there while the
    /// Sandbox is live or its outcome is unknown; a later authorized adoption
    /// may reactivate the same exact durable owner.
    pub(crate) async fn retire_session_environment_for_revocation(
        &self,
        thread: &str,
    ) -> Result<bool, HostError> {
        let cause = SessionEnvironmentRetirementCause::RealizationRevocation;
        let retirement = match self.retire_current_environment(
            thread,
            cause.clone(),
            RetirementSelection::Current,
        ) {
            Ok(Some(retirement)) => Some(retirement.owner),
            Ok(None) => match self.pending_environment_adoption(thread) {
                Some((identity, binding)) => Some(
                    self.adopt_pending_environment_for_retirement(
                        thread, &identity, &binding, cause,
                    )
                    .await?,
                ),
                None => None,
            },
            Err(error) => return Err(error),
        };

        self.drain_mcp_projections(thread, &[]).await?;
        let retirement_result = match retirement {
            Some(retirement) => match &retirement.owned {
                RetiringEnvironmentOwner::Unbound(_) => {
                    self.dispose_and_confirm_retirement(thread, &retirement)
                        .await
                }
                RetiringEnvironmentOwner::Bound(_) => self
                    .stop_and_confirm_retirement(thread, &retirement)
                    .await
                    .map(|_| ()),
            },
            None => Ok(()),
        };
        if self.session_environment_owner_is_vacant(thread) {
            self.session_slots.remove(thread);
        } else {
            self.session_slots.modify(thread, |slot| {
                let realization = slot.realization.clone();
                let lifecycle = slot.lifecycle.clone();
                let resource_projection = slot.resource_projection.clone();
                let realization_changed = slot.realization_changed.clone();
                let environment_owner = std::mem::take(&mut slot.environment_owner);
                // A live/unknown Retiring owner is retryable physical state.
                // Preserve its exact provider selection and SandboxSpec inputs;
                // terminal takeover must not reconstruct either after a failed
                // or cancelled revocation attempt.
                let published_snapshot = slot.published_snapshot.take();
                let environment_projection = slot.environment_projection.take();
                let environment_snapshot = slot.environment_snapshot.take();
                let content_delivery = slot.content_delivery;
                let baseline = slot.baseline.take();
                let resources = std::mem::take(&mut slot.resources);
                // Terminal retry still has to publish the retained owner's
                // mutable outputs into its exact Workspace. This is a derived
                // harvest scope, not continuing realization authority.
                let workspace = slot.workspace.take();
                // Revocation clears process-local realization material but is
                // not an Environment restoration/source-disposal authority.
                // Preserve an already-closed quiescence fence so only the
                // canonical durable projection proof may reopen MCP effects.
                let mcp_quiescence_fence = slot.mcp_quiescence_fence.take();
                *slot = crate::session_slot::SessionRuntimeSlot::default();
                slot.realization = realization;
                slot.lifecycle = lifecycle;
                slot.resource_projection = resource_projection;
                slot.realization_changed = realization_changed;
                slot.environment_owner = environment_owner;
                slot.published_snapshot = published_snapshot;
                slot.environment_projection = environment_projection;
                slot.environment_snapshot = environment_snapshot;
                slot.content_delivery = content_delivery;
                slot.baseline = baseline;
                slot.resources = resources;
                slot.workspace = workspace;
                slot.mcp_quiescence_fence = mcp_quiescence_fence;
            });
        }
        retirement_result?;
        Ok(true)
    }

    /// End a Session under the caller-held lifecycle guard. The exact terminal
    /// effect becomes part of `Retiring` before any stop/dispose/status await.
    pub(crate) async fn end_session(
        &self,
        thread: &str,
        terminal_effect_id: &str,
    ) -> Result<(), HostError> {
        let cause = SessionEnvironmentRetirementCause::Terminal {
            effect_id: terminal_effect_id.to_string(),
        };
        let (retirement, adopted_for_cleanup) = if let Some(retirement) =
            self.retire_current_environment(thread, cause.clone(), RetirementSelection::Current)?
        {
            (Some(retirement.owner), false)
        } else if let Some((identity, binding)) = self.pending_environment_adoption(thread) {
            (
                Some(
                    self.adopt_pending_environment_for_retirement(
                        thread, &identity, &binding, cause,
                    )
                    .await?,
                ),
                true,
            )
        } else {
            match self.session_slots.read(thread, |slot| {
                matches!(slot.environment_owner, SessionEnvironmentOwner::Vacant)
            }) {
                Some(true) | None => (None, false),
                Some(false) => {
                    return Err(HostError::internal(
                        "terminal cleanup has an Environment owner without an adoptable binding",
                    ));
                }
            }
        };

        self.drain_mcp_projections(thread, &[]).await?;
        if let Some(retirement) = retirement {
            let env = retirement.owned.environment();
            if adopted_for_cleanup || env.needs_recovered_memory_reconciliation() {
                let mounter = self.memory_mounter().ok_or_else(|| {
                    HostError::internal("recovered Memory copy has no MemoryMounter")
                })?;
                for mount in self.thread_session_mounts(thread) {
                    if let awaken_provisioning_contract::MountSource::MemoryStore {
                        store_id,
                        materialization_reference,
                        ..
                    } = &mount.source
                    {
                        let files = env
                            .list_frozen_mount_files(&mount.mount_path)
                            .await
                            .map_err(|error| HostError::internal(error.to_string()))?;
                        mounter
                            .reconcile_recovered_copy(
                                materialization_reference.as_deref().unwrap_or(store_id),
                                &files,
                                mount.access,
                            )
                            .await
                            .map_err(|error| HostError::internal(error.to_string()))?;
                    }
                }
            }
            self.dispose_and_confirm_retirement(thread, &retirement)
                .await?;
        }

        self.session_slots.remove(thread);
        if let Some(relay) = self.mcp_relay.get() {
            relay.remove_routes(thread);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
