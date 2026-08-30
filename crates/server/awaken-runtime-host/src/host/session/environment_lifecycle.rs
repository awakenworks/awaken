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
mod terminal_cleanup;
mod terminal_preparation;

pub(super) use state::committed_identity;
use state::{RetirementSelection, checkpoint_source_identity, projected_environment_owner};

/// One atomic process-local retirement transition. A successful transition
/// detaches the Runtime together with its Environment owner; an unmatched
/// exact-selection leaves both untouched.
struct LocalRetirementTransition {
    owner: RetiringSessionEnvironment,
}

/// One process-local teardown projection derived from the frozen aggregate
/// Environment state. It carries no authority of its own; keeping the state and
/// optional physical owner together prevents terminal checkpoint and Sandbox
/// cleanup from re-reading or independently interpreting the slot.
struct TerminalEnvironmentPreparation {
    state: awaken_session_contract::SessionEnvironmentState,
    environment: PreparedBoundEnvironment,
    pending_checkpoint: Option<awaken_session_contract::SandboxCheckpointRequest>,
}

/// Exact decoded binding, provider-effective spec, and any process-local
/// owner observations produced by the single adoption validator.
type ValidatedSessionEnvironmentAdoption = (
    awaken_provisioning_contract::SandboxHandle,
    awaken_provisioning_contract::SandboxSpec,
    Option<Arc<crate::session_environment::SessionEnvironment>>,
    Option<UnboundSessionEnvironment>,
);

/// Process-local projection of which source effects remain legal after the
/// provider observation. The provider-owned [`SandboxObservation`] remains the
/// only physical fact; this mode merely keeps live I/O, source preparation,
/// and already-prepared recovery from drifting into three independent tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalEffectMode {
    LiveUnprepared,
    RecoverOnlyUnprepared,
    RecoverOnlyAlreadyPrepared,
}

/// One exact terminal owner plus its non-durable I/O mode. This stays inside
/// runtime-host and is never serialized, acknowledged, or used as mutation
/// authority.
pub(crate) struct PreparedBoundEnvironment {
    pub(crate) environment: Option<(Arc<crate::session_environment::SessionEnvironment>, bool)>,
    durable_handle: Option<awaken_provisioning_contract::SandboxHandle>,
    effect_mode: TerminalEffectMode,
}

impl PreparedBoundEnvironment {
    fn new(
        environment: Option<(Arc<crate::session_environment::SessionEnvironment>, bool)>,
        durable_handle: Option<awaken_provisioning_contract::SandboxHandle>,
        effect_mode: TerminalEffectMode,
    ) -> Self {
        Self {
            environment,
            durable_handle,
            effect_mode,
        }
    }

    /// Join the provider's physical observation with the aggregate's closed
    /// continuation predecessor. The latter dominates: after A is durable,
    /// total physical absence is a response-loss recovery case rather than an
    /// excuse to repeat live source effects under terminal T.
    fn with_preparation_authorization(
        mut self,
        authorization: &awaken_session_contract::SessionTerminalCleanupPreparationAuthorization,
    ) -> Self {
        if authorization.inherited_provider_disposal().is_some() {
            self.effect_mode = TerminalEffectMode::RecoverOnlyAlreadyPrepared;
        }
        self
    }

    pub(crate) fn permits_live_io(&self) -> bool {
        self.effect_mode == TerminalEffectMode::LiveUnprepared
    }

    fn requires_provider_preparation(&self) -> bool {
        self.effect_mode != TerminalEffectMode::RecoverOnlyAlreadyPrepared
    }

    fn source_effects_are_already_prepared(&self) -> bool {
        self.effect_mode == TerminalEffectMode::RecoverOnlyAlreadyPrepared
    }

    fn durable_handle(&self) -> Option<&awaken_provisioning_contract::SandboxHandle> {
        self.durable_handle.as_ref()
    }
}

/// Pure terminal Memory projection produced before Artifact, checkpoint, or
/// provider effects. The contract owns the optional-handle join and intent
/// construction; this transient plan only carries its exact result forward.
struct TerminalMemoryPlan {
    intents: Vec<awaken_session_contract::SessionTerminalMemoryIntent>,
    acknowledged_materializations:
        Option<Vec<awaken_provisioning_contract::MemoryMaterializationEvidence>>,
    workspace_id: Option<String>,
}

/// Borrowed inputs for the single unavailable-observation phase under the
/// Session lifecycle lock. Every authority remains in its existing typed value;
/// this transient view neither mirrors the slot nor survives the call.
struct UnavailableEnvironmentRecovery<'a> {
    thread: &'a str,
    binding: &'a str,
    source_generation_id: Option<&'a str>,
    pending_adoption: Option<(BoundSessionEnvironmentIdentity, String)>,
    policy: SessionEnvironmentUnavailablePolicy,
    provider: &'a crate::session_environment::SessionEnvironmentProvider,
    handle: &'a awaken_provisioning_contract::SandboxHandle,
    observation: &'a awaken_provisioning_contract::SandboxObservation,
}

/// Borrowed inputs for one terminal Environment installation phase. The
/// aggregate effect fence, provider spec, handle, and optional restore fence
/// remain their canonical value objects; this context owns no lifecycle fact.
struct TerminalEnvironmentInstallation<'a> {
    thread: &'a str,
    binding: Option<&'a str>,
    provider: &'a crate::session_environment::SessionEnvironmentProvider,
    spec: &'a awaken_provisioning_contract::SandboxSpec,
    handle: Option<&'a awaken_provisioning_contract::SandboxHandle>,
    expected_effect_fence: Option<&'a awaken_provisioning_contract::SandboxEffectFence>,
    effect_fence: &'a awaken_provisioning_contract::SandboxEffectFence,
}

impl TerminalEnvironmentPreparation {
    /// Resident and suspending states still name the Agent-authored source.
    /// Hibernated has no live root, while Restoring may contain only a partial
    /// replay of checkpoint bytes and must never publish those bytes as fresh
    /// terminal output.
    fn harvests_agent_outputs(&self) -> bool {
        terminal_state_harvests_agent_outputs(&self.state)
    }

    /// Select the one artifact edge without conflating aggregate state with
    /// provider I/O safety. Once the provider reports `Disposing`, durable
    /// receipt recovery is mandatory even when the aggregate state would not
    /// authorize a live output read.
    fn artifact_capture_mode(&self) -> Option<crate::provisioning::ArtifactCaptureMode> {
        if !self.environment.permits_live_io() {
            Some(crate::provisioning::ArtifactCaptureMode::ReceiptOnly)
        } else if self.harvests_agent_outputs() {
            Some(crate::provisioning::ArtifactCaptureMode::Live)
        } else {
            None
        }
    }
}

fn terminal_state_harvests_agent_outputs(
    state: &awaken_session_contract::SessionEnvironmentState,
) -> bool {
    matches!(
        state,
        awaken_session_contract::SessionEnvironmentState::Resident { .. }
            | awaken_session_contract::SessionEnvironmentState::Suspending { .. }
    )
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

    /// Compile the one provider-effective spec used by active adoption,
    /// continuation disposal, and terminal takeover. A terminal projection
    /// supplies the previous Resource endpoint because that is what the exact
    /// physical substrate contains; no continuation path may reconstruct a
    /// second layout from ambient slot state.
    fn session_environment_adoption_spec(
        &self,
        thread: &str,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        resolved_resources: Option<&awaken_session_contract::ResolvedSessionResources>,
    ) -> awaken_provisioning_contract::SandboxSpec {
        match resolved_resources {
            Some(resources) => {
                self.sandbox_spec_for_resolved_resources_and_provider(thread, resources, provider)
            }
            None => self
                .session_slots
                .read(thread, |slot| slot.resource_transition.clone())
                .flatten()
                .map_or_else(
                    || self.sandbox_spec_for_provider(thread, provider),
                    |transition| {
                        self.sandbox_spec_for_resolved_resources_and_provider(
                            thread,
                            &transition.desired().resources,
                            provider,
                        )
                    },
                ),
        }
    }

    /// Decode and validate one durable binding against the exact provider and
    /// frozen Resource layout before any backend observation or process effect.
    /// Ordinary recovery supplies the desired transition endpoint; terminal
    /// cleanup supplies the previous endpoint that the bound substrate actually
    /// contains. Keeping this projection shared prevents two handle/layout
    /// validators from drifting.
    fn validated_session_environment_adoption(
        &self,
        thread: &str,
        encoded: &str,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        resolved_resources: Option<&awaken_session_contract::ResolvedSessionResources>,
    ) -> Result<ValidatedSessionEnvironmentAdoption, HostError> {
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
        let spec = self.session_environment_adoption_spec(thread, provider, resolved_resources);
        let spec = self.validate_session_environment_adoption(provider, &spec, &handle)?;
        let (retained, candidate) = self
            .session_slots
            .read(thread, |slot| match &slot.environment_owner {
                SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::Candidate(
                    candidate,
                )) => (None, Some(candidate.clone())),
                owner => (owner.terminal_bound_environment(), None),
            })
            .unwrap_or_default();
        let resident = retained
            .map(|owned| {
                if owned.binding != encoded {
                    return Err(HostError::internal(
                        "Session Environment retry conflicts with its retained binding",
                    ));
                }
                Ok(owned.environment)
            })
            .transpose()?;
        if let Some(candidate) = &candidate
            && (candidate.binding != encoded || candidate.environment.handle() != handle)
        {
            return Err(HostError::internal(
                "Session Environment retry conflicts with its hidden Candidate binding",
            ));
        }
        if let Some(environment) = resident.as_ref() {
            let resident_handle = environment.handle();
            if resident_handle != handle {
                return Err(HostError::internal(format!(
                    "Session {thread} is already bound to sandbox {}, not {}",
                    resident_handle.sandbox_id, handle.sandbox_id
                )));
            }
        }
        Ok((handle, spec, resident, candidate))
    }

    /// Resolve an opaque durable binding through the one Session lifecycle
    /// owner. Pure decode/layout validation precedes aggregate authorization;
    /// effect-free provider observation, exact adoption, root receipt, delivery
    /// cache, slot publication, and Resource convergence then execute under one
    /// process-local lifecycle lock. `rebuild_unavailable` only admits typed,
    /// exact-incarnation unavailability; provider errors remain retryable.
    pub(crate) async fn adopt_bound_session_environment(
        &self,
        thread: &str,
        encoded: Option<&str>,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        source_generation_id: Option<&str>,
        rebuild_unavailable: bool,
    ) -> Result<SessionEnvironmentAdoptionDisposition, HostError> {
        let Some(encoded) = encoded else {
            return Ok(SessionEnvironmentAdoptionDisposition::NoBinding);
        };
        let lifecycle = self
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        self.adopt_bound_session_environment_under_lifecycle(
            thread,
            encoded,
            provider,
            source_generation_id,
            if rebuild_unavailable {
                SessionEnvironmentUnavailablePolicy::Rebuild
            } else {
                SessionEnvironmentUnavailablePolicy::Reject
            },
            None,
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
        provider: &crate::session_environment::SessionEnvironmentProvider,
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
        let handle = environment.handle();
        let neutral_spec = self.session_environment_adoption_spec(thread, provider, None);
        let spec = provider
            .effective_adoption_layout(&neutral_spec, &handle)
            .map_err(|error| {
                HostError::classified(
                    "session_environment_observation_incompatible",
                    error.to_string(),
                )
            })?
            .spec;
        let observation = provider
            .observe_effective(&spec, &handle)
            .await
            .map_err(|error| {
                HostError::unavailable_classified(
                    "session_environment_observation_indeterminate",
                    format!(
                        "could not inspect Session sandbox {}: {error}",
                        handle.sandbox_id
                    ),
                )
            })?;
        match observation {
            awaken_provisioning_contract::SandboxObservation::Ready => {
                self.dispose_and_confirm_retirement(thread, &retiring)
                    .await?;
                Ok(())
            }
            observation
            @ awaken_provisioning_contract::SandboxObservation::DefinitivelyUnavailable {
                ..
            } => {
                provider
                    .validate_closed_observation(&handle, &observation)
                    .map_err(|error| {
                        HostError::classified(
                            "session_environment_observation_incompatible",
                            error.to_string(),
                        )
                    })?;
                if !self.confirm_terminated_retirement(thread, &retiring) {
                    return Err(HostError::internal(
                        "terminated legacy recovery owner lost its exact Retiring fence",
                    ));
                }
                Ok(())
            }
            observation
            @ (awaken_provisioning_contract::SandboxObservation::Terminal { .. }
            | awaken_provisioning_contract::SandboxObservation::Disposing { .. }) => {
                provider
                    .validate_closed_observation(&handle, &observation)
                    .map_err(|error| {
                        HostError::classified(
                            "session_environment_observation_incompatible",
                            error.to_string(),
                        )
                    })?;
                self.dispose_and_confirm_retirement(thread, &retiring)
                    .await?;
                Ok(())
            }
            awaken_provisioning_contract::SandboxObservation::Provisioning => {
                Err(HostError::unavailable_classified(
                    "session_environment_provisioning",
                    format!(
                        "Session sandbox {} is still provisioning",
                        handle.sandbox_id
                    ),
                ))
            }
            awaken_provisioning_contract::SandboxObservation::Incompatible { reason } => Err(
                HostError::classified("session_environment_observation_incompatible", reason),
            ),
        }
    }

    pub(super) async fn adopt_bound_session_environment_under_lifecycle(
        &self,
        thread: &str,
        encoded: &str,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        source_generation_id: Option<&str>,
        unavailable_policy: SessionEnvironmentUnavailablePolicy,
        preauthorized: Option<&AuthorizedSessionEnvironmentEffect>,
    ) -> Result<SessionEnvironmentAdoptionDisposition, HostError> {
        let (handle, spec, mut resident, candidate) =
            self.validated_session_environment_adoption(thread, encoded, provider, None)?;
        // Realization revocation permanently closed the old process-level Hand,
        // but a durable owner still names the physical Sandbox. Retain its exact
        // fence through typed observation; only Ready may move it back to
        // AwaitingAdoption so canonical provider adoption constructs a fresh
        // wrapper over the same handle and root. LegacyDirect is deliberately
        // excluded: its claimed recovery owns physical disposal and same-id
        // creation instead.
        let revoked_durable = self
            .session_slots
            .read(thread, |slot| match &slot.environment_owner {
                SessionEnvironmentOwner::Retiring(retiring)
                    if matches!(
                        (&retiring.cause, &retiring.owned),
                        (
                            SessionEnvironmentRetirementCause::RealizationRevocation,
                            RetiringEnvironmentOwner::Bound(BoundSessionEnvironment {
                                identity: BoundSessionEnvironmentIdentity::Durable { .. },
                                binding,
                                ..
                            }),
                        ) if binding == encoded
                    ) =>
                {
                    Some(retiring.clone())
                }
                _ => None,
            })
            .flatten();
        // Capture the exact pending identity before provider I/O. Closed
        // observation may clear only this value; a same-binding aggregate ABA
        // installed while observation is in flight must survive.
        let pending_adoption = self.pending_environment_adoption(thread);
        let authorized;
        let effect = match preauthorized {
            Some(effect) => effect,
            None => {
                authorized = self
                    .authorize_environment_effect_before_io(
                        thread,
                        awaken_session_contract::SessionEnvironmentEffectKind::Adopt,
                        Some(encoded),
                    )
                    .await?;
                &authorized
            }
        };
        if let awaken_session_contract::SessionEnvironmentEffectAuthorization::AlreadyApplied {
            binding,
        } = &effect.authorization
            && binding != encoded
        {
            return Err(HostError::internal(
                "Session Environment adoption effect already committed another binding",
            ));
        }
        let observation = match effect.provider_fence.as_ref() {
            Some(effect_fence) => {
                provider
                    .observe_effective_for_effect(&spec, &handle, effect_fence)
                    .await
            }
            None => provider.observe_effective(&spec, &handle).await,
        }
        .map_err(|error| {
            HostError::unavailable_classified(
                "session_environment_observation_indeterminate",
                format!(
                    "could not inspect Session sandbox {}: {error}",
                    handle.sandbox_id
                ),
            )
        })?;
        // Ready-adoption cause/effect table: C1 the validated exact handle is
        // already Resident or ordinarily Retiring; C2 an exact hidden Candidate
        // was retained across cancellation; C3 no process-local owner exists;
        // C4 a durable owner was Retiring for RealizationRevocation. E1 C1 reuses
        // the exact Arc/identity and performs only Resource reconciliation, with
        // zero Candidate, binding persistence, or publication; E2 C2 reuses its
        // Arc and retries only the existing persistence/publication edge; E3 C3
        // adopts once and enters that same edge; E4 C4 adopts the same physical
        // handle through a fresh wrapper/Hand. A foreign binding or handle has
        // already failed validation above and reaches no rule.
        match observation {
            awaken_provisioning_contract::SandboxObservation::Ready => {
                if let Some(retiring) = revoked_durable {
                    self.session_slots.update(thread, |slot| {
                        slot.environment_owner
                            .prepare_retiring_realization_adoption(&retiring)
                    })?;
                    // Validation captured the old Arc before the exact phase
                    // transition. Its Hand is closed, so Ready must flow through
                    // canonical provider adoption rather than reuse that Arc.
                    resident = None;
                }
                let environment = match (resident, candidate) {
                    (Some(environment), None) => environment,
                    (None, Some(candidate)) => {
                        self.publish_session_environment_under_lifecycle(
                            thread,
                            candidate.environment,
                            effect,
                            crate::session_slot::EnvironmentResourceReconciliation::Adopted,
                        )
                        .await?
                    }
                    (None, None) => {
                        let environment = Arc::new(
                            provider
                            .adopt_effective_for_effect(
                                &spec,
                                &handle,
                                effect.provider_fence.as_ref(),
                            )
                            .await
                            .map_err(|error| {
                                HostError::unavailable_classified(
                                    "session_environment_adoption_indeterminate",
                                    error.to_string(),
                                )
                            })?,
                        );
                        self.publish_session_environment_under_lifecycle(
                            thread,
                            environment,
                            effect,
                            crate::session_slot::EnvironmentResourceReconciliation::Adopted,
                        )
                        .await?
                    }
                    (Some(_), Some(_)) => {
                        return Err(HostError::internal(
                            "Session Environment cannot be both retained and unpublished",
                        ));
                    }
                };
                self.ensure_published_environment_reconciled_under_lifecycle(thread, environment)
                    .await?;
                self.session_slots.update(thread, |slot| {
                    slot.environment_rebuild_source = None;
                });
                Ok(SessionEnvironmentAdoptionDisposition::Ready)
            }
            awaken_provisioning_contract::SandboxObservation::Provisioning => {
                Err(HostError::unavailable_classified(
                    "session_environment_provisioning",
                    format!(
                        "Session sandbox {} is still provisioning",
                        handle.sandbox_id
                    ),
                ))
            }
            observation
            @ (awaken_provisioning_contract::SandboxObservation::DefinitivelyUnavailable {
                ..
            }
            | awaken_provisioning_contract::SandboxObservation::Terminal { .. }
            | awaken_provisioning_contract::SandboxObservation::Disposing { .. }) => {
                if candidate.is_some() {
                    return Err(HostError::unavailable_classified(
                        "session_environment_candidate_observation_closed",
                        "retained Session Environment Candidate is no longer Ready",
                    ));
                }
                self.record_unavailable_session_environment_under_lifecycle(
                    UnavailableEnvironmentRecovery {
                        thread,
                        binding: encoded,
                        source_generation_id,
                        pending_adoption,
                        policy: unavailable_policy,
                        provider,
                        handle: &handle,
                        observation: &observation,
                    },
                    resident.as_ref(),
                )
                .await
            }
            awaken_provisioning_contract::SandboxObservation::Incompatible { reason } => Err(
                HostError::classified("session_environment_observation_incompatible", reason),
            ),
        }
    }

    /// Convert only exact provider evidence into the existing aggregate Rebuild
    /// transition. Both a proved-absent realization and an exact terminal
    /// realization use this one policy owner; adapters never decide whether a
    /// Session may replace its durable binding.
    async fn record_unavailable_session_environment_under_lifecycle(
        &self,
        recovery: UnavailableEnvironmentRecovery<'_>,
        resident: Option<&Arc<crate::session_environment::SessionEnvironment>>,
    ) -> Result<SessionEnvironmentAdoptionDisposition, HostError> {
        let UnavailableEnvironmentRecovery {
            thread,
            binding,
            source_generation_id,
            pending_adoption,
            policy,
            provider,
            handle,
            observation,
        } = recovery;
        provider
            .validate_closed_observation(handle, observation)
            .map_err(|error| HostError::internal(error.to_string()))?;
        match policy {
            SessionEnvironmentUnavailablePolicy::Reject => Err(HostError::internal(format!(
                "Session sandbox {} is unavailable or terminal",
                handle.sandbox_id
            ))),
            SessionEnvironmentUnavailablePolicy::Rebuild => {
                if let Some(environment) = resident
                    && !self.discard_observed_session_environment(thread, environment)
                {
                    return Err(HostError::internal(format!(
                        "lost the sandbox recovery fence for Session {thread}"
                    )));
                }
                if let Some((identity, pending_binding)) = pending_adoption
                    && !self
                        .session_slots
                        .modify(thread, |slot| {
                            slot.environment_owner
                                .discard_closed_pending_adoption(&identity, &pending_binding)
                        })
                        .unwrap_or(false)
                {
                    return Err(HostError::internal(format!(
                        "lost the pending sandbox recovery fence for Session {thread}"
                    )));
                }
                self.session_slots.update(thread, |slot| {
                    slot.environment_rebuild_source = Some((
                        binding.to_string(),
                        source_generation_id.map(str::to_string),
                    ));
                });
                Ok(SessionEnvironmentAdoptionDisposition::RebuildRequired)
            }
        }
    }

    /// Consume only the closed provider evidence validated immediately above.
    /// No second status read may weaken that exact physical observation or
    /// create a competing recovery fact.
    fn discard_observed_session_environment(
        &self,
        thread: &str,
        expected: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> bool {
        let Ok(Some(retirement)) = self.retire_current_environment(
            thread,
            SessionEnvironmentRetirementCause::RecoveryDiscard,
            RetirementSelection::Exact(expected),
        ) else {
            return false;
        };
        self.confirm_terminated_retirement(thread, &retirement.owner)
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
                Ok(retirement.map(|owner| {
                    slot.runtime = None;
                    LocalRetirementTransition { owner }
                }))
            })
            .transpose()
            .map(Option::flatten)
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

    #[cfg(test)]
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

    #[cfg(test)]
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
        if !self.confirm_terminated_retirement(thread, retirement) {
            return Err(HostError::internal(
                "retired Session Environment owner changed after physical disposal",
            ));
        }
        Ok(())
    }

    /// Freeze the exact checkpoint source in Retiring only after every
    /// quiescence and source-durability participant has completed. This is the
    /// Preparation-to-Disposal ownership edge; it performs no provider I/O.
    pub(crate) fn retain_checkpoint_source_environment_for_disposal(
        &self,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        generation: &awaken_session_contract::SandboxGeneration,
        source_binding: &str,
        environment: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<(), HostError> {
        self.validate_suspend_operation(thread, operation, generation)?;
        let cause = SessionEnvironmentRetirementCause::CheckpointSource {
            operation: operation.clone(),
            generation: generation.clone(),
        };
        let retirement = self
            .retire_current_environment(
                thread,
                cause.clone(),
                RetirementSelection::Exact(environment),
            )?
            .ok_or_else(|| {
                HostError::unavailable_classified(
                    "session_checkpoint_source_owner_changed",
                    "checkpoint source lost its exact Resident owner before Preparation completed",
                )
            })?;
        let RetiringEnvironmentOwner::Bound(owned) = &retirement.owner.owned else {
            return Err(HostError::internal(
                "checkpoint source Preparation retained an unpublished owner",
            ));
        };
        if retirement.owner.cause != cause
            || !matches!(
                &owned.identity,
                BoundSessionEnvironmentIdentity::Durable {
                    generation: owned_generation,
                    ..
                } if owned_generation == generation
            )
            || owned.binding != source_binding
        {
            return Err(HostError::unavailable_classified(
                "session_checkpoint_source_owner_changed",
                "checkpoint source Preparation retained different durable authority",
            ));
        }
        Ok(())
    }

    /// Consume only the exact Retiring owner and durable provider preparation.
    /// Artifact, Memory, Hand, MCP, observation, and provider preparation are
    /// forbidden here; this is the physical-only Disposal phase.
    pub(crate) async fn dispose_prepared_checkpoint_source_environment(
        &self,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        generation: &awaken_session_contract::SandboxGeneration,
        source_binding: &str,
        authorization: &awaken_provisioning_contract::SandboxDisposalAuthorization,
    ) -> Result<(), HostError> {
        self.validate_suspend_operation(thread, operation, generation)?;
        let expected_cause = SessionEnvironmentRetirementCause::CheckpointSource {
            operation: operation.clone(),
            generation: generation.clone(),
        };
        let retirement = self
            .session_slots
            .read(thread, |slot| match &slot.environment_owner {
                SessionEnvironmentOwner::Retiring(retirement) => Some(retirement.clone()),
                _ => None,
            })
            .flatten()
            .ok_or_else(|| {
                HostError::unavailable_classified(
                    "session_checkpoint_source_not_prepared",
                    "checkpoint source Disposal has no retained Preparation owner",
                )
            })?;
        let RetiringEnvironmentOwner::Bound(owned) = &retirement.owned else {
            return Err(HostError::internal(
                "checkpoint source Disposal cannot consume an unpublished owner",
            ));
        };
        if retirement.cause != expected_cause
            || !matches!(
                &owned.identity,
                BoundSessionEnvironmentIdentity::Durable {
                    generation: owned_generation,
                    ..
                } if owned_generation == generation
            )
            || owned.binding != source_binding
        {
            return Err(HostError::unavailable_classified(
                "session_checkpoint_source_owner_changed",
                "checkpoint source Disposal does not match its retained Preparation owner",
            ));
        }
        owned
            .environment
            .dispose_for_effect(authorization)
            .await
            .map_err(|error| HostError::unavailable(error.to_string()))?;
        if !self.confirm_terminated_retirement(thread, &retirement) {
            return Err(HostError::unavailable_classified(
                "session_checkpoint_source_owner_changed",
                "checkpoint source owner changed after exact physical Disposal",
            ));
        }
        self.session_slots
            .update(thread, |slot| slot.runtime = None);
        Ok(())
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
            cause,
            RetirementSelection::Current,
        )? {
            Some(retirement) => Some(retirement.owner),
            None if self.pending_environment_adoption(thread).is_some() => {
                // Revocation decision table: C1 durable binding is projected,
                // C2 no exact local Arc has been adopted, and C3 no terminal
                // effect fence authorizes cold reconstruction. E1 fail before
                // draining MCP or clearing any frozen provider/layout input;
                // E2 retain AwaitingAdoption for the canonical terminal or
                // ordinary adoption retry. Reconstructing here would duplicate
                // terminal preparation and manufacture physical authority.
                return Err(HostError::unavailable_classified(
                    "session_environment_revocation_pending_adoption",
                    "Session Environment revocation cannot retire a durable binding without its exact local owner",
                ));
            }
            None => None,
        };

        self.drain_mcp_projections(thread, &[]).await?;
        let retirement_result = match retirement {
            Some(retirement) => match &retirement.owned {
                RetiringEnvironmentOwner::Unbound(_) => {
                    self.dispose_and_confirm_retirement(thread, &retirement)
                        .await
                }
                RetiringEnvironmentOwner::Bound(_) => retirement
                    .owned
                    .environment()
                    .stop_bound_processes()
                    .await
                    .map(|_| ())
                    .map_err(|error| HostError::internal(error.to_string())),
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
}

#[cfg(test)]
mod terminal_output_tests {
    use super::*;

    #[test]
    fn terminal_effect_mode_keeps_live_recovery_and_provider_preparation_coupled() {
        /* Process-local mode table HT0. Causes: C1 provider observation is
         * Ready/Terminal, definitive primary absence, or Disposing; C2 the
         * exact cleanup owner may be present/absent. Effects: E1 only live mode
         * permits source reads; E2 live and unprepared-recovery require the
         * provider preparation edge; E3 only Disposing proves that source
         * effects were already prepared. The owner presence does not change
         * these rules. This single projector prevents Artifact, Memory, and
         * provider callers from inventing separate observation policies.
         *
         * | Rule | observation projection | live I/O | provider prep | prior prep |
         * | T0 | LiveUnprepared | yes | yes | no |
         * | T1 | RecoverOnlyUnprepared | no | yes | no |
         * | T2 | RecoverOnlyAlreadyPrepared | no | no | yes | */
        let cases = [
            (
                PreparedBoundEnvironment::new(None, None, TerminalEffectMode::LiveUnprepared),
                true,
                true,
                false,
            ),
            (
                PreparedBoundEnvironment::new(
                    None,
                    None,
                    TerminalEffectMode::RecoverOnlyUnprepared,
                ),
                false,
                true,
                false,
            ),
            (
                PreparedBoundEnvironment::new(
                    None,
                    None,
                    TerminalEffectMode::RecoverOnlyAlreadyPrepared,
                ),
                false,
                false,
                true,
            ),
        ];
        for (prepared, live_io, provider_preparation, already_prepared) in cases {
            assert_eq!(prepared.permits_live_io(), live_io);
            assert_eq!(
                prepared.requires_provider_preparation(),
                provider_preparation,
            );
            assert_eq!(
                prepared.source_effects_are_already_prepared(),
                already_prepared,
            );
        }
    }

    #[test]
    fn only_agent_authored_terminal_states_read_live_outputs() {
        /* Host terminal-output table HT1. Causes: C1 aggregate state is
         * Resident/Suspending/otherwise; C2 provider I/O mode is Live or
         * RecoverOnly. Effects: E1 Agent-authored+Live uses the one live
         * ArtifactHarvester edge; E2 non-Agent-authored+Live skips live
         * capture; E3 every RecoverOnly state invokes receipt recovery and
         * performs no live read. Rules: HT1a Resident|Suspending+Live=>E1;
         * HT1b otherwise+Live=>E2; HT1c any+RecoverOnly=>E3. C2 dominates C1:
         * a durable Disposing gate must never be mistaken for "no artifacts"
         * merely because the aggregate state is non-Agent-authored. */
        let resident = awaken_session_contract::SessionEnvironmentState::Resident {
            binding: "binding-a".into(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        };
        assert!(terminal_state_harvests_agent_outputs(&resident), "HT1a");
        assert!(
            !terminal_state_harvests_agent_outputs(
                &awaken_session_contract::SessionEnvironmentState::Unmaterialized
            ),
            "HT1b"
        );

        let disposing_non_agent_state = TerminalEnvironmentPreparation {
            state: awaken_session_contract::SessionEnvironmentState::Unmaterialized,
            environment: PreparedBoundEnvironment::new(
                None,
                None,
                TerminalEffectMode::RecoverOnlyAlreadyPrepared,
            ),
            pending_checkpoint: None,
        };
        assert_eq!(
            disposing_non_agent_state.artifact_capture_mode(),
            Some(crate::provisioning::ArtifactCaptureMode::ReceiptOnly),
            "HT1c"
        );
    }
}

#[cfg(test)]
mod tests;
