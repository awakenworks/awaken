// @formal entrypoint session_environment.closed_revoked_recovery_consumes_only_exact_unprotected_owner
// @formal evidence session_environment.owner_topology_is_exact_and_frozen
// @formal evidence session_environment.source_disposal_requires_a_verified_checkpoint
//! Session Environment publication, adoption, fencing, and retirement.
//!
//! This module is the sole writer of [`SessionEnvironmentOwner`]. Durable
//! lifecycle truth remains in the Session aggregate; the slot machine retains
//! each physical wrapper across every fallible or cancellable provider call.

use crate::bound_environment::TerminalEnvironmentInstallation;
use crate::environment_owner::{
    BoundSessionEnvironment, BoundSessionEnvironmentIdentity, LegacyDirectEnvironmentProvenance,
    RetiringEnvironmentOwner, RetiringSessionEnvironment, SessionEnvironmentOwner,
    SessionEnvironmentPreparation, SessionEnvironmentRetirementCause, UnboundSessionEnvironment,
};
use crate::realization_error::{AuthorityLoss, PendingState, ProjectionConflict, RealizationError};
use crate::session_environment::{
    AuthorizedSessionEnvironmentEffect, BoundEnvironmentPreparationMode,
    SessionEnvironmentAdoptionDisposition, SessionEnvironmentUnavailablePolicy,
};
use crate::session_environment::{PreparedBoundEnvironment, TerminalEffectMode};
use awaken_session_configuration_contract::resource as session_resource;
use awaken_tool_contract::tool_batch::ResourceOriginRef;
use std::sync::Arc;

mod publication;
pub mod state;
mod terminal_cleanup;
mod terminal_preparation;

use state::{
    RetirementSelection, checkpoint_source_identity, pending_revoked_resource_reservation,
};

/// One atomic process-local retirement transition. A successful transition
/// detaches the Runtime together with its Environment owner; an unmatched
/// exact-selection leaves both untouched.
pub struct LocalRetirementTransition {
    pub owner: RetiringSessionEnvironment,
}

/// One process-local teardown projection derived from the frozen aggregate
/// Environment state. It carries no authority of its own; keeping the state and
/// optional physical owner together prevents terminal checkpoint and Sandbox
/// cleanup from re-reading or independently interpreting the slot.
struct TerminalEnvironmentPreparation {
    state: awaken_session_state_contract::SessionEnvironmentState,
    environment: PreparedBoundEnvironment,
    pending_checkpoint: Option<awaken_session_state_contract::SandboxCheckpointRequest>,
}

/// Exact decoded binding, provider-effective spec, and any process-local
/// owner observations produced by the single adoption validator.
type ValidatedSessionEnvironmentAdoption = (
    awaken_provisioning_contract::SandboxHandle,
    awaken_provisioning_contract::SandboxSpec,
    Option<Arc<crate::session_environment::SessionEnvironment>>,
    Option<UnboundSessionEnvironment>,
);

/// Pure terminal Memory projection produced before Artifact, checkpoint, or
/// provider effects. The contract owns the optional-handle join and intent
/// construction; this transient plan only carries its exact result forward.
struct TerminalMemoryPlan {
    intents: Vec<awaken_session_state_contract::SessionTerminalMemoryIntent>,
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
    revoked_durable: Option<&'a RetiringSessionEnvironment>,
    policy: SessionEnvironmentUnavailablePolicy,
    provider: &'a crate::session_environment::SessionEnvironmentProvider,
    handle: &'a awaken_provisioning_contract::SandboxHandle,
    observation: &'a awaken_provisioning_contract::SandboxObservation,
}

/// Exact process-local owner selected after closed provider evidence has been
/// validated. The value itself is the fence consumed by the sole lifecycle
/// writer; no binding-only or cause-only proxy can authorize recovery.
enum ClosedEnvironmentRecoveryOwner {
    Retiring(Box<RetiringSessionEnvironment>),
    Resident(Arc<crate::session_environment::SessionEnvironment>),
    Pending(Box<(BoundSessionEnvironmentIdentity, String)>),
    Vacant,
}

/// Capability minted only after the provider validates this exact closed
/// observation against the exact handle.
struct ValidatedClosedEnvironmentObservation<'a>(
    &'a awaken_provisioning_contract::SandboxObservation,
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClosedEnvironmentRecoveryOwnerClass {
    Revoked,
    RecoveryDiscard,
    Resident,
    Pending,
    Vacant,
    ProtectedOrStale,
}

/// The only closed-observation recovery plan. `Consume` retains the exact
/// owner fence through the subsequent slot mutation; `Retain` is fail-closed.
enum ClosedEnvironmentRecoveryPlan<'a> {
    Consume {
        observation: ValidatedClosedEnvironmentObservation<'a>,
        owner: ClosedEnvironmentRecoveryOwner,
    },
    Retain,
}

const fn closed_environment_recovery_admission(
    policy: SessionEnvironmentUnavailablePolicy,
    owner: ClosedEnvironmentRecoveryOwnerClass,
) -> bool {
    matches!(policy, SessionEnvironmentUnavailablePolicy::Rebuild)
        && !matches!(owner, ClosedEnvironmentRecoveryOwnerClass::ProtectedOrStale)
}

#[cfg(kani)]
/// @obligation session_environment.closed_revoked_recovery_consumes_only_exact_unprotected_owner
#[kani::proof]
fn closed_environment_recovery_consumes_only_rebuildable_owner_classes() {
    // Complete cause/effect table: C1 Reject/Rebuild policy; C2 every owner
    // class. E1 Consume iff Rebuild and the class is exact and unprotected;
    // E2 Reject, protected, and stale inputs retain. The production plan then
    // carries the corresponding exact Rust owner value into the sole writer.
    let rebuild: bool = kani::any();
    let tag: u8 = kani::any();
    let policy = if rebuild {
        SessionEnvironmentUnavailablePolicy::Rebuild
    } else {
        SessionEnvironmentUnavailablePolicy::Reject
    };
    let owner = match tag % 6 {
        0 => ClosedEnvironmentRecoveryOwnerClass::Revoked,
        1 => ClosedEnvironmentRecoveryOwnerClass::RecoveryDiscard,
        2 => ClosedEnvironmentRecoveryOwnerClass::Resident,
        3 => ClosedEnvironmentRecoveryOwnerClass::Pending,
        4 => ClosedEnvironmentRecoveryOwnerClass::Vacant,
        _ => ClosedEnvironmentRecoveryOwnerClass::ProtectedOrStale,
    };
    assert_eq!(
        closed_environment_recovery_admission(policy, owner),
        rebuild && owner != ClosedEnvironmentRecoveryOwnerClass::ProtectedOrStale
    );
}

impl TerminalEnvironmentPreparation {
    /// Select the one artifact edge without conflating aggregate state with
    /// provider I/O safety. Once the provider reports `Disposing`, durable
    /// receipt recovery is mandatory even when the aggregate state would not
    /// authorize a live output read.
    fn artifact_capture_mode(&self) -> Option<crate::artifact_harvest::ArtifactCaptureMode> {
        if !self.environment.permits_live_io() {
            Some(crate::artifact_harvest::ArtifactCaptureMode::ReceiptOnly)
        } else if self.state.binding().is_some() {
            Some(crate::artifact_harvest::ArtifactCaptureMode::Live)
        } else {
            None
        }
    }
}

impl crate::environment_holding::EnvironmentHolding {
    /// Retire the exact current Environment owner; a changed owner fence fails
    /// with the caller's classified reason.
    pub fn retire_exact_environment(
        &self,
        thread: &str,
        cause: SessionEnvironmentRetirementCause,
        environment: &Arc<crate::session_environment::SessionEnvironment>,
        lost: crate::realization_error::AuthorityLoss,
        message: &'static str,
    ) -> Result<(), RealizationError> {
        self.retire_current_environment(thread, cause, RetirementSelection::Exact(environment))?
            .map(|_| ())
            .ok_or_else(|| RealizationError::authority_lost(lost, message))
    }

    /// An unpublished terminal or cleanup retirement holds the Environment;
    /// ordinary adoption may not resume over it.
    pub fn holds_unpublished_retirement(&self, thread: &str) -> Result<bool, RealizationError> {
        Ok(self
            .sessions
            .read_realization(thread, |r| {
                matches!(
                    &r.environment.owner,
                    SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                        cause, owned: RetiringEnvironmentOwner::Unbound(_),
                    }) if *cause != SessionEnvironmentRetirementCause::RealizationRevocation
                )
            })?
            .unwrap_or(false))
    }

    pub fn begin_session_environment_restore(
        &self,
        thread: &str,
        request: &awaken_session_state_contract::SandboxRestoreRequest,
    ) -> Result<(), RealizationError> {
        self.sessions
            .update_realization(thread, |r| r.environment.owner.begin_restore(request))?
    }

    fn closed_environment_recovery_plan<'a>(
        &self,
        thread: &str,
        policy: SessionEnvironmentUnavailablePolicy,
        observation: ValidatedClosedEnvironmentObservation<'a>,
        revoked: Option<&'a RetiringSessionEnvironment>,
        resident: Option<&'a Arc<crate::session_environment::SessionEnvironment>>,
        pending: Option<&'a (BoundSessionEnvironmentIdentity, String)>,
    ) -> Result<ClosedEnvironmentRecoveryPlan<'a>, RealizationError> {
        let (owner_class, exact_owner) = self
            .sessions
            .read_realization(thread, |r| {
                if let Some(expected) = revoked
                    && expected.cause == SessionEnvironmentRetirementCause::RealizationRevocation
                    && matches!(
                        &r.environment.owner,
                        SessionEnvironmentOwner::Retiring(current)
                            if current.exact_matches(expected)
                    )
                {
                    (
                        ClosedEnvironmentRecoveryOwnerClass::Revoked,
                        Some(ClosedEnvironmentRecoveryOwner::Retiring(Box::new(
                            expected.clone(),
                        ))),
                    )
                } else if let Some(expected) = resident
                    && let SessionEnvironmentOwner::Retiring(current) = &r.environment.owner
                    && current.cause == SessionEnvironmentRetirementCause::RecoveryDiscard
                    && Arc::ptr_eq(&current.owned.environment(), expected)
                    && current.owned.environment().lifecycle().handle() == expected.lifecycle().handle()
                {
                    (
                        ClosedEnvironmentRecoveryOwnerClass::RecoveryDiscard,
                        Some(ClosedEnvironmentRecoveryOwner::Retiring(Box::new(
                            current.clone(),
                        ))),
                    )
                } else if let Some(expected) = resident
                    && matches!(
                        &r.environment.owner,
                        SessionEnvironmentOwner::Resident(current)
                            if Arc::ptr_eq(&current.environment, expected)
                                && current.environment.lifecycle().handle() == expected.lifecycle().handle()
                    )
                {
                    (
                        ClosedEnvironmentRecoveryOwnerClass::Resident,
                        Some(ClosedEnvironmentRecoveryOwner::Resident(expected.clone())),
                    )
                } else if let Some((identity, binding)) = pending
                    && matches!(
                        &r.environment.owner,
                        SessionEnvironmentOwner::Preparing(
                            SessionEnvironmentPreparation::AwaitingAdoption {
                                identity: current_identity,
                                binding: current_binding,
                            }
                        ) if current_identity == identity && current_binding == binding
                    )
                {
                    (
                        ClosedEnvironmentRecoveryOwnerClass::Pending,
                        Some(ClosedEnvironmentRecoveryOwner::Pending(Box::new((
                            identity.clone(),
                            binding.clone(),
                        )))),
                    )
                } else if revoked.is_none()
                    && resident.is_none()
                    && pending.is_none()
                    && matches!(r.environment.owner, SessionEnvironmentOwner::Vacant)
                {
                    (
                        ClosedEnvironmentRecoveryOwnerClass::Vacant,
                        Some(ClosedEnvironmentRecoveryOwner::Vacant),
                    )
                } else {
                    (ClosedEnvironmentRecoveryOwnerClass::ProtectedOrStale, None)
                }
            })?
            .unwrap_or((ClosedEnvironmentRecoveryOwnerClass::ProtectedOrStale, None));
        if !closed_environment_recovery_admission(policy, owner_class) {
            return Ok(ClosedEnvironmentRecoveryPlan::Retain);
        }
        Ok(match owner_class {
            ClosedEnvironmentRecoveryOwnerClass::Revoked
            | ClosedEnvironmentRecoveryOwnerClass::RecoveryDiscard
            | ClosedEnvironmentRecoveryOwnerClass::Resident
            | ClosedEnvironmentRecoveryOwnerClass::Pending
            | ClosedEnvironmentRecoveryOwnerClass::Vacant => {
                ClosedEnvironmentRecoveryPlan::Consume {
                    observation,
                    owner: exact_owner.expect("admitted recovery class retains its exact owner"),
                }
            }
            ClosedEnvironmentRecoveryOwnerClass::ProtectedOrStale => {
                ClosedEnvironmentRecoveryPlan::Retain
            }
        })
    }

    fn confirm_terminated_retirement(
        &self,
        thread: &str,
        retirement: &RetiringSessionEnvironment,
    ) -> Result<bool, RealizationError> {
        Ok(self
            .sessions
            .modify_realization(thread, |r| {
                r.environment.owner.confirm_terminated(retirement)
            })?
            .unwrap_or(false))
    }

    /// Consume only the closed provider evidence validated immediately above.
    /// No second status read may weaken that exact physical observation or
    /// create a competing recovery fact.
    fn discard_observed_session_environment(
        &self,
        thread: &str,
        expected: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<bool, RealizationError> {
        let Ok(Some(retirement)) = self.retire_current_environment(
            thread,
            SessionEnvironmentRetirementCause::RecoveryDiscard,
            RetirementSelection::Exact(expected),
        ) else {
            return Ok(false);
        };
        self.confirm_terminated_retirement(thread, &retirement.owner)
    }

    async fn dispose_and_confirm_retirement(
        &self,
        thread: &str,
        retirement: &RetiringSessionEnvironment,
    ) -> Result<(), RealizationError> {
        let environment = retirement.owned.environment();
        environment
            .lifecycle()
            .dispose()
            .await
            .map_err(RealizationError::from)?;
        if !self.confirm_terminated_retirement(thread, retirement)? {
            return Err(RealizationError::invariant(
                "retired Session Environment owner changed after physical disposal",
            ));
        }
        Ok(())
    }

    pub fn durable_environment_binding(
        &self,
        thread: &str,
    ) -> Result<Option<String>, RealizationError> {
        Ok(self
            .sessions
            .read_realization(thread, |r| {
                r.environment.owner.durable_binding().map(str::to_owned)
            })?
            .flatten())
    }

    pub fn pending_environment_adoption(
        &self,
        thread: &str,
    ) -> Result<Option<(BoundSessionEnvironmentIdentity, String)>, RealizationError> {
        Ok(self
            .sessions
            .read_realization(thread, |r| match &r.environment.owner {
                SessionEnvironmentOwner::Preparing(
                    SessionEnvironmentPreparation::AwaitingAdoption { identity, binding },
                ) => Some((identity.clone(), binding.clone())),
                _ => None,
            })?
            .flatten())
    }

    /// Retire the process-local Environment retained when a prior realization
    /// was revoked before the legacy/direct Session had published a durable
    /// binding. Quiescence permanently closes its Hand dispatch lifecycle, so a
    /// new claimed attempt must rebuild through the ordinary creation path;
    /// ordinary direct callers cannot use this transition as disposal authority.
    pub async fn rebuild_claimed_legacy_environment_after_revocation(
        &self,
        thread: &str,
        provider: &crate::session_environment::SessionEnvironmentProvider,
    ) -> Result<(), RealizationError> {
        let retiring = self
            .sessions
            .read_realization(thread, |r| match &r.environment.owner {
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
            })?
            .flatten();
        let Some(retiring) = retiring else {
            return Ok(());
        };
        let environment = retiring.owned.environment();
        let handle = environment.lifecycle().handle();
        let neutral_spec = self.session_environment_adoption_spec(thread, provider, None)?;
        let spec = provider
            .adoption()
            .effective_adoption_layout(&neutral_spec, &handle)
            .map_err(|error| {
                RealizationError::conflict(
                    ProjectionConflict::ObservationIncompatible,
                    error.to_string(),
                )
            })?
            .spec;
        let observation = provider
            .adoption()
            .observe_effective(
                &spec,
                &handle,
                Some(environment.as_ref()),
                awaken_provisioning_contract::SandboxObservationBasis::Unattached,
                awaken_session_environment_contract::ObservationPurpose::Recovery,
            )
            .await
            .map_err(|error| {
                RealizationError::pending(
                    PendingState::ObservationIndeterminate,
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
                    .adoption().validate_closed_observation(&handle, &observation)
                    .map_err(|error| {
                        RealizationError::conflict(
                            ProjectionConflict::ObservationIncompatible,
                            error.to_string(),
                        )
                    })?;
                if !self.confirm_terminated_retirement(thread, &retiring)? {
                    return Err(RealizationError::invariant(
                        "terminated legacy recovery owner lost its exact Retiring fence",
                    ));
                }
                Ok(())
            }
            observation
            @ (awaken_provisioning_contract::SandboxObservation::Terminal { .. }
            | awaken_provisioning_contract::SandboxObservation::Disposing { .. }) => {
                provider
                    .adoption().validate_closed_observation(&handle, &observation)
                    .map_err(|error| {
                        RealizationError::conflict(
                            ProjectionConflict::ObservationIncompatible,
                            error.to_string(),
                        )
                    })?;
                self.dispose_and_confirm_retirement(thread, &retiring)
                    .await?;
                Ok(())
            }
            awaken_provisioning_contract::SandboxObservation::Provisioning => {
                Err(RealizationError::pending(
                    PendingState::Provisioning,
                    format!(
                        "Session sandbox {} is still provisioning",
                        handle.sandbox_id
                    ),
                ))
            }
            awaken_provisioning_contract::SandboxObservation::Incompatible { reason } => Err(
                RealizationError::conflict(ProjectionConflict::ObservationIncompatible, reason),
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
    ) -> Result<SessionEnvironmentAdoptionDisposition, RealizationError> {
        let UnavailableEnvironmentRecovery {
            thread,
            binding,
            source_generation_id,
            pending_adoption,
            revoked_durable,
            policy,
            provider,
            handle,
            observation,
        } = recovery;
        provider
            .adoption()
            .validate_closed_observation(handle, observation)
            .map_err(RealizationError::from)?;
        // Cause/effect decision table: C1 exact provider evidence is closed;
        // C2 policy is Reject/Rebuild; C3 the current owner is the captured
        // Revocation, Resident, pending adoption, Vacant, or protected/stale.
        // R1 Reject => retain/error. R2 Rebuild + exact unprotected owner =>
        // consume that typed fence and record Rebuild. R3 protected/stale =>
        // retain/error. Provider validation above owns C1; this plan owns C2/C3.
        let plan = self.closed_environment_recovery_plan(
            thread,
            policy,
            ValidatedClosedEnvironmentObservation(observation),
            revoked_durable,
            resident,
            pending_adoption.as_ref(),
        )?;
        match plan {
            ClosedEnvironmentRecoveryPlan::Retain => Err(RealizationError::invariant(format!(
                "Session sandbox {} is unavailable or terminal, or its exact recovery owner is protected",
                handle.sandbox_id
            ))),
            ClosedEnvironmentRecoveryPlan::Consume { observation, owner } => {
                let _validated_observation = observation.0;
                let consumed = match owner {
                    ClosedEnvironmentRecoveryOwner::Retiring(retiring) => {
                        self.confirm_terminated_retirement(thread, &retiring)?
                    }
                    ClosedEnvironmentRecoveryOwner::Resident(environment) => {
                        self.discard_observed_session_environment(thread, &environment)?
                    }
                    ClosedEnvironmentRecoveryOwner::Pending(pending) => self
                        .sessions
                        .modify_realization(thread, |r| {
                            r.environment
                                .owner
                                .discard_closed_pending_adoption(&pending.0, &pending.1)
                        })?
                        .unwrap_or(false),
                    ClosedEnvironmentRecoveryOwner::Vacant => true,
                };
                if !consumed {
                    return Err(RealizationError::invariant(format!(
                        "lost the exact sandbox recovery fence for Session {thread}"
                    )));
                }
                self.sessions.update_realization(thread, |r| {
                    r.environment.rebuild_source = Some((
                        binding.to_string(),
                        source_generation_id.map(str::to_string),
                    ));
                })?;
                Ok(SessionEnvironmentAdoptionDisposition::RebuildRequired)
            }
        }
    }

    pub fn resident_checkpoint_source_environment(
        &self,
        thread: &str,
        operation: &awaken_session_state_contract::SessionEnvironmentOperation,
        source_effect_id: &str,
        source_binding: &str,
        generation: &awaken_session_state_contract::SandboxGeneration,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, RealizationError> {
        self.validate_suspend_operation(thread, operation, generation)?;
        let expected_identity =
            checkpoint_source_identity(source_effect_id, source_binding, generation);
        self.sessions
            .read_realization(thread, |r| match &r.environment.owner {
                SessionEnvironmentOwner::Resident(owned)
                    if owned.identity == expected_identity
                        && owned.binding == source_binding
                        && serde_json::to_string(&owned.environment.lifecycle().handle())
                            .is_ok_and(|binding| binding == source_binding) =>
                {
                    Some(owned.environment.clone())
                }
                _ => None,
            })?
            .flatten()
            .ok_or_else(|| {
                RealizationError::invariant(
                    "checkpoint continuation does not own its exact resident source",
                )
            })
    }

    /// Resolve the shared-background/quiescence key from the sole exact
    /// process-local Environment owner. The durable generation remains owned by
    /// the Session aggregate; this method only projects it into Runtime.
    pub fn resident_environment_activity_identity(
        &self,
        thread: &str,
        environment: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<(String, Option<ResourceOriginRef>), RealizationError> {
        self.sessions
            .read_realization(thread, |r| {
                r.environment.owner.resident_activity_identity(environment)
            })?
            .flatten()
            .ok_or_else(|| {
                RealizationError::invariant(
                    "Runtime Environment does not match its exact Resident activity owner",
                )
            })
    }

    /// Freeze the exact checkpoint source in Retiring only after every
    /// quiescence and source-durability participant has completed. This is the
    /// Preparation-to-Disposal ownership edge; it performs no provider I/O.
    pub fn retain_checkpoint_source_environment_for_disposal(
        &self,
        thread: &str,
        operation: &awaken_session_state_contract::SessionEnvironmentOperation,
        generation: &awaken_session_state_contract::SandboxGeneration,
        source_binding: &str,
        environment: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<(), RealizationError> {
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
                RealizationError::authority_lost(
                    AuthorityLoss::CheckpointSourceOwnerChanged,
                    "checkpoint source lost its exact Resident owner before Preparation completed",
                )
            })?;
        let RetiringEnvironmentOwner::Bound(owned) = &retirement.owner.owned else {
            return Err(RealizationError::invariant(
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
            return Err(RealizationError::authority_lost(
                AuthorityLoss::CheckpointSourceOwnerChanged,
                "checkpoint source Preparation retained different durable authority",
            ));
        }
        Ok(())
    }

    pub fn retire_current_environment(
        &self,
        thread: &str,
        cause: SessionEnvironmentRetirementCause,
        selection: RetirementSelection<'_>,
    ) -> Result<Option<LocalRetirementTransition>, RealizationError> {
        self.sessions
            .modify_session(thread, |slot| {
                let retirement = slot
                    .realization
                    .environment
                    .owner
                    .begin_retirement(cause, selection)?;
                Ok(retirement.map(|owner| {
                    slot.run.drop_context();
                    LocalRetirementTransition { owner }
                }))
            })?
            .transpose()
            .map(Option::flatten)
    }

    /// Decode and validate one durable binding against the exact provider and
    /// frozen Resource layout before any backend observation or process effect.
    /// Ordinary recovery supplies the desired transition endpoint; terminal
    /// cleanup supplies the previous endpoint that the bound substrate actually
    /// contains. Keeping this projection shared prevents two handle/layout
    /// validators from drifting.
    fn revoked_owner_for_binding(
        &self,
        thread: &str,
        binding: &str,
        source_generation_id: Option<&str>,
    ) -> Result<Option<RetiringSessionEnvironment>, RealizationError> {
        Ok(self
            .sessions
            .read_realization(thread, |r| match &r.environment.owner {
                SessionEnvironmentOwner::Retiring(retiring)
                    if retiring.cause
                        == SessionEnvironmentRetirementCause::RealizationRevocation =>
                {
                    let matches = match &retiring.owned {
                        RetiringEnvironmentOwner::Bound(owned)
                            if matches!(
                                &owned.identity,
                                BoundSessionEnvironmentIdentity::Durable { .. }
                            ) =>
                        {
                            owned.binding == binding
                                || (matches!(&owned.identity,
                                    BoundSessionEnvironmentIdentity::Durable { generation, .. }
                                        if Some(generation.id()) == source_generation_id)
                                    && pending_revoked_resource_reservation(
                                        owned,
                                        &owned.identity,
                                        binding,
                                    ))
                        }
                        RetiringEnvironmentOwner::Unbound(candidate) => {
                            candidate.binding == binding
                        }
                        _ => false,
                    };
                    matches.then(|| retiring.clone())
                }
                _ => None,
            })?
            .flatten())
    }

    pub async fn session_environment(
        &self,
        thread: &str,
    ) -> Result<Option<Arc<crate::session_environment::SessionEnvironment>>, RealizationError> {
        Ok(self
            .sessions
            .read_realization(thread, |r| r.environment.owner.resident())?
            .flatten())
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
        resolved_resources: Option<&session_resource::ResolvedSessionResources>,
    ) -> Result<awaken_provisioning_contract::SandboxSpec, RealizationError> {
        Ok(match resolved_resources {
            Some(resources) => self
                .staging
                .sandbox_spec_for_resolved_resources_and_provider(thread, resources, provider)?,
            None => self
                .sessions
                .read_realization(thread, |r| r.staging.transition.clone())?
                .flatten()
                .map_or_else(
                    || self.staging.sandbox_spec_for_provider(thread, provider),
                    |transition| {
                        self.staging
                            .sandbox_spec_for_resolved_resources_and_provider(
                                thread,
                                &transition.desired().resources,
                                provider,
                            )
                    },
                )?,
        })
    }

    pub fn session_environment_owner_is_vacant(
        &self,
        thread: &str,
    ) -> Result<bool, RealizationError> {
        Ok(self
            .sessions
            .read_realization(thread, |r| {
                matches!(r.environment.owner, SessionEnvironmentOwner::Vacant)
            })?
            .unwrap_or(true))
    }

    pub fn validate_suspend_operation(
        &self,
        thread: &str,
        operation: &awaken_session_state_contract::SessionEnvironmentOperation,
        _generation: &awaken_session_state_contract::SandboxGeneration,
    ) -> Result<(), RealizationError> {
        // The Session aggregate is the sole operation writer. Existing durable
        // operations, including legacy fingerprints, replay opaquely; Runtime
        // validates the current realization fence and exact source owner rather
        // than deriving a competing effect identity.
        let authorized = self
            .sessions
            .read_realization(thread, |r| match operation.realization.as_ref() {
                Some(asserted) => r.execution.authority.remaining_for(asserted).is_some(),
                None => matches!(
                    r.execution.authority,
                    crate::realization_state::SessionRealizationAuthority::Uninstalled
                ),
            })?
            .unwrap_or(operation.realization.is_none());
        if !authorized {
            return Err(RealizationError::invariant(
                "checkpoint continuation operation lost its exact realization lease",
            ));
        }
        Ok(())
    }

    pub fn validated_session_environment_adoption(
        &self,
        thread: &str,
        encoded: &str,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        resolved_resources: Option<&session_resource::ResolvedSessionResources>,
        source_generation_id: Option<&str>,
    ) -> Result<ValidatedSessionEnvironmentAdoption, RealizationError> {
        let handle: awaken_provisioning_contract::SandboxHandle = serde_json::from_str(encoded)
            .map_err(crate::realization_error::EvidenceFault::InvalidBinding)?;
        if handle.sandbox_id != thread {
            return Err(RealizationError::invariant(format!(
                "sandbox {} does not belong to Session {thread}",
                handle.sandbox_id
            )));
        }
        let spec = self.session_environment_adoption_spec(thread, provider, resolved_resources)?;
        let spec = self.validate_session_environment_adoption(provider, &spec, &handle)?;
        let (retained, candidate) = self
            .sessions
            .read_realization(thread, |r| match &r.environment.owner {
                SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::Candidate(
                    candidate,
                )) => (None, Some(candidate.clone())),
                SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                    owned: RetiringEnvironmentOwner::Unbound(candidate),
                    ..
                }) => (
                    Some((candidate.binding.clone(), candidate.environment.clone())),
                    None,
                ),
                owner => (
                    owner
                        .terminal_bound_environment()
                        .map(|owned| (owned.binding, owned.environment)),
                    None,
                ),
            })?
            .unwrap_or_default();
        let resident = retained
            .map(|(binding, environment)| {
                if binding != encoded {
                    let successor = self
                        .revoked_owner_for_binding(thread, encoded, source_generation_id)?
                        .is_some_and(|retiring| {
                            matches!(retiring.owned, RetiringEnvironmentOwner::Bound(_))
                        });
                    if successor {
                        return Ok(None);
                    }
                    return Err(RealizationError::invariant(
                        "Session Environment retry conflicts with its retained binding",
                    ));
                }
                Ok(Some(environment))
            })
            .transpose()?
            .flatten();
        if let Some(candidate) = &candidate
            && (candidate.binding != encoded
                || candidate.environment.lifecycle().handle() != handle)
        {
            return Err(RealizationError::invariant(
                "Session Environment retry conflicts with its hidden Candidate binding",
            ));
        }
        if let Some(environment) = resident.as_ref() {
            let resident_handle = environment.lifecycle().handle();
            if resident_handle != handle {
                return Err(RealizationError::invariant(format!(
                    "Session {thread} is already bound to sandbox {}, not {}",
                    resident_handle.sandbox_id, handle.sandbox_id
                )));
            }
        }
        Ok((handle, spec, resident, candidate))
    }
}

impl crate::environment_holding::EnvironmentHolding {
    /// Consume only the exact Retiring owner and durable provider preparation.
    /// Artifact, Memory, Hand, MCP, observation, and provider preparation are
    /// forbidden here; this is the physical-only Disposal phase.
    pub async fn dispose_prepared_checkpoint_source_environment(
        &self,
        thread: &str,
        operation: &awaken_session_state_contract::SessionEnvironmentOperation,
        generation: &awaken_session_state_contract::SandboxGeneration,
        source_binding: &str,
        authorization: &awaken_provisioning_contract::SandboxDisposalAuthorization,
    ) -> Result<(), RealizationError> {
        // Preparation A remains the exact retained source identity. Disposal
        // may be driven by an aggregate-authorized higher-epoch successor B;
        // requiring A to remain current here would make failover unrecoverable.
        // The canonical authorization validates A -> B, and the physical effect
        // authority captured below checks B against the current Session slot.
        authorization
            .validate()
            .map_err(RealizationError::terminal_effect_pending)?;
        let expected_cause = SessionEnvironmentRetirementCause::CheckpointSource {
            operation: operation.clone(),
            generation: generation.clone(),
        };
        let retirement = self
            .sessions
            .read_realization(thread, |r| match &r.environment.owner {
                SessionEnvironmentOwner::Retiring(retirement) => Some(retirement.clone()),
                _ => None,
            })?
            .flatten()
            .ok_or_else(|| {
                RealizationError::pending(
                    PendingState::CheckpointSourceNotPrepared,
                    "checkpoint source Disposal has no retained Preparation owner",
                )
            })?;
        let RetiringEnvironmentOwner::Bound(owned) = &retirement.owned else {
            return Err(RealizationError::invariant(
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
            return Err(RealizationError::authority_lost(
                AuthorityLoss::CheckpointSourceOwnerChanged,
                "checkpoint source Disposal does not match its retained Preparation owner",
            ));
        }
        let authority = self
            .execution
            .provider_effect_authority(thread, authorization.effect_fence())?;
        owned
            .environment
            .lifecycle()
            .dispose_for_effect(authorization, &authority)
            .await
            .map_err(RealizationError::terminal_effect_pending)?;
        if !self.confirm_terminated_retirement(thread, &retirement)? {
            return Err(RealizationError::authority_lost(
                AuthorityLoss::CheckpointSourceOwnerChanged,
                "checkpoint source owner changed after exact physical Disposal",
            ));
        }
        self.sessions.drop_run_context(thread)?;
        Ok(())
    }
}

impl crate::environment_holding::EnvironmentHolding {
    /// Resolve an opaque durable binding through the one Session lifecycle
    /// owner. Pure decode/layout validation precedes aggregate authorization;
    /// effect-free provider observation, exact adoption, root receipt, delivery
    /// cache, slot publication, and Resource convergence then execute under one
    /// process-local lifecycle lock. `rebuild_unavailable` only admits typed,
    /// exact-incarnation unavailability; provider errors remain retryable.
    pub async fn adopt_bound_session_environment(
        &self,
        thread: &str,
        encoded: Option<&str>,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        source_generation_id: Option<&str>,
        rebuild_unavailable: bool,
    ) -> Result<SessionEnvironmentAdoptionDisposition, RealizationError> {
        let realizations = self.sessions.clone();
        let Some(encoded) = encoded else {
            return Ok(SessionEnvironmentAdoptionDisposition::NoBinding);
        };
        let _lifecycle = realizations.lock_lifecycle(thread).await?;
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

    pub async fn adopt_bound_session_environment_under_lifecycle(
        &self,
        thread: &str,
        encoded: &str,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        source_generation_id: Option<&str>,
        unavailable_policy: SessionEnvironmentUnavailablePolicy,
        preauthorized: Option<&AuthorizedSessionEnvironmentEffect>,
    ) -> Result<SessionEnvironmentAdoptionDisposition, RealizationError> {
        let realizer = self;
        let exec = realizer;
        if exec.holds_unpublished_retirement(thread)? {
            return Err(RealizationError::invariant(
                "unpublished terminal or cleanup ownership cannot resume ordinary adoption",
            ));
        }
        let (handle, spec, mut resident, candidate) = exec.validated_session_environment_adoption(
            thread,
            encoded,
            provider,
            None,
            source_generation_id,
        )?;
        // Realization revocation permanently closed the old process-level Hand,
        // but the retained owner still names the physical Sandbox. Retain its
        // exact fence through typed observation; only Ready may enroll a fresh
        // Candidate over the same physical owner, handle and root. A missing
        // commit response requires aggregate adoption authority. LegacyDirect is
        // excluded: its claimed recovery owns physical disposal and same-id
        // creation instead.
        let revoked_owner =
            exec.revoked_owner_for_binding(thread, encoded, source_generation_id)?;
        // Capture the exact pending identity before provider I/O. Closed
        // observation may clear only this value; a same-binding aggregate ABA
        // installed while observation is in flight must survive.
        let pending_adoption = exec.pending_environment_adoption(thread)?;
        let authorized;
        let effect = match preauthorized {
            Some(effect) => effect,
            None => {
                authorized = realizer
                    .authorize_environment_effect_before_io(
                        thread,
                        &realizer.execution.new_environment_effect_intent(
                            thread,
                            awaken_session_state_contract::SessionEnvironmentEffectKind::Adopt,
                            Some(encoded),
                        )?,
                    )
                    .await?;
                &authorized
            }
        };
        if revoked_owner
            .as_ref()
            .is_some_and(|retiring| matches!(&retiring.owned, RetiringEnvironmentOwner::Unbound(_)))
            && effect.provider_fence.is_none()
        {
            return Err(RealizationError::invariant(
                "unacknowledged Candidate recovery requires current aggregate adoption authority",
            ));
        }
        if let awaken_session_state_contract::SessionEnvironmentEffectAuthorization::AlreadyApplied {
            binding,
            ..
        } = &effect.authorization
            && binding != encoded
        {
            return Err(RealizationError::invariant(
                "Session Environment adoption effect already committed another binding",
            ));
        }
        // A revoked Hand is unusable, but its retained physical Sandbox still
        // owns the original create attempt. Borrow that proof for observation;
        // do not turn a warm rebind into an unprovable cold adoption.
        let revoked_environment = revoked_owner
            .as_ref()
            .map(|retiring| retiring.owned.environment());
        let retained = resident
            .as_deref()
            .or_else(|| {
                candidate
                    .as_ref()
                    .map(|candidate| candidate.environment.as_ref())
            })
            .or(revoked_environment.as_deref());
        let authority = effect
            .provider_fence
            .as_ref()
            .map(|fence| realizer.execution.provider_effect_authority(thread, fence))
            .transpose()?;
        let provider_effect = crate::session_environment::ProviderEffect::from_parts(
            effect.provider_fence.as_ref(),
            authority.as_ref().map(|authority| {
                authority as &dyn awaken_provisioning_contract::CurrentExecutionAuthority
            }),
        )
        .map_err(|error| {
            RealizationError::authority_lost(
                AuthorityLoss::AdoptionAuthorityLost,
                error.to_string(),
            )
        })?;
        let admit_original = || {
            provider_effect.admit().map_err(|error| {
                RealizationError::authority_lost(
                    AuthorityLoss::AdoptionAuthorityLost,
                    error.to_string(),
                )
            })
        };
        let observation = provider
            .adoption()
            .observe_effective(
                &spec,
                &handle,
                retained,
                provider_effect.observation_basis(&spec),
                awaken_session_environment_contract::ObservationPurpose::Recovery,
            )
            .await
            .map_err(|error| {
                RealizationError::pending(
                    PendingState::ObservationIndeterminate,
                    format!(
                        "could not inspect Session sandbox {}: {error}",
                        handle.sandbox_id
                    ),
                )
            })?;
        admit_original()?;
        // Ready-adoption cause/effect table: C1 the validated exact handle is
        // already Resident or ordinarily Retiring; C2 an exact hidden Candidate
        // was retained across cancellation; C3 no process-local owner exists;
        // C4 a durable owner was Retiring for RealizationRevocation. E1 C1 reuses
        // the exact Arc/identity and performs only Resource reconciliation, with
        // zero Candidate, binding persistence, or publication; E2 C2 reuses its
        // Arc and retries only the existing persistence/publication edge; E3 C3
        // adopts once and enters that same edge; E4 C4 retains the exact physical
        // owner and publishes a fresh wrapper/Hand through the same edge. A
        // foreign binding or handle already failed validation above and reaches
        // no rule.
        match observation {
            awaken_provisioning_contract::SandboxObservation::Ready => {
                if revoked_owner.is_some() {
                    // Keep the Retiring owner in its slot until a fresh wrapper
                    // exists. Failure/cancellation must preserve the only local
                    // create-attempt and cleanup participants for a later retry.
                    resident = None;
                }
                let environment = match (resident, candidate) {
                    (Some(environment), None) => environment,
                    (None, Some(candidate)) => {
                        self.publish_session_environment_under_lifecycle(
                            thread,
                            candidate.environment,
                            effect,
                            crate::staged_resources::EnvironmentResourceReconciliation::Adopted,
                        )
                        .await?
                    }
                    (None, None) => {
                        let rebound = match revoked_owner.as_ref() {
                            Some(retiring) => provider
                                .adoption().rebind_retained(&spec, &handle, &retiring.owned.environment(),
                                    provider_effect)
                                .await,
                            None => provider
                                .adoption().adopt_effective_for_effect(&spec, &handle, provider_effect)
                                .await,
                        };
                        admit_original()?;
                        let environment = Arc::new(rebound.map_err(|error| {
                            RealizationError::pending(PendingState::AdoptionIndeterminate, error.to_string(),
                            )
                        })?);
                        if let Some(retiring) = revoked_owner.as_ref() {
                            // Atomically retain the fresh Candidate before its
                            // canonical binding/publication edge can suspend.
                            exec.sessions.update_realization(thread, |r| {
                                r.environment.owner
                                    .prepare_retiring_realization_adoption(retiring, environment.clone(), Some(effect.authorization.intent()))
                            })??;
                        }
                        self.publish_session_environment_under_lifecycle(
                            thread,
                            environment,
                            effect,
                            crate::staged_resources::EnvironmentResourceReconciliation::Adopted,
                        )
                        .await?
                    }
                    (Some(_), Some(_)) => {
                        return Err(RealizationError::invariant(
                            "Session Environment cannot be both retained and unpublished",
                        ));
                    }
                };
                self.ensure_published_environment_reconciled_under_lifecycle(thread, environment)
                    .await?;
                exec.sessions.update_realization(thread, |r| {
                    r.environment.rebuild_source = None;
                })?;
                Ok(SessionEnvironmentAdoptionDisposition::Ready)
            }
            awaken_provisioning_contract::SandboxObservation::Provisioning => {
                Err(RealizationError::pending(
                    PendingState::Provisioning,
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
                    return Err(RealizationError::pending(
                        PendingState::CandidateObservationClosed,
                        "retained Session Environment Candidate is no longer Ready",
                    ));
                }
                // Ask the same Root before consuming the captured old owner.
                // A pending Managed Rebuild must retain that exact owner;
                // unowned direct Threads keep the original recovery policy.
                if unavailable_policy == SessionEnvironmentUnavailablePolicy::Rebuild {
                    realizer.authorize_environment_effect_before_io(
                        thread,
                        &realizer.execution.new_environment_effect_intent(thread, awaken_session_state_contract::SessionEnvironmentEffectKind::Rebuild {
                            source_generation_id: source_generation_id.map(str::to_owned),
                        },
                        Some(encoded))?,
                    ).await?;
                }
                exec.record_unavailable_session_environment_under_lifecycle(
                    UnavailableEnvironmentRecovery {
                        thread,
                        binding: encoded,
                        source_generation_id,
                        pending_adoption,
                        revoked_durable: revoked_owner.as_ref(),
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
                RealizationError::conflict(ProjectionConflict::ObservationIncompatible, reason),
            ),
        }
    }

    /// Called by the complete projection installer under lifecycle -> Resource
    /// locks, before installing a different Session generation. The durable
    /// Sandbox identity does not change, but a scoped execution object's captured
    /// authority cannot follow that new generation. Reuse the sole retirement
    /// transition; failure/cancellation leaves its exact owner available to retry.
    #[cfg(target_os = "linux")]
    pub async fn retire_replaced_scoped_execution<S: awaken_session_mcp::McpSessionSlots>(
        &self,
        thread: &str,
        incoming: &awaken_session_realization_contract::ObservedSessionRealizationLease,
        mcp: (&awaken_session_mcp::McpSessions, &S),
    ) -> Result<(), RealizationError> {
        let realizations = self.sessions.clone();
        let retirement = self.execution.authorize_worker_effect(|| {
            let now = awaken_authority_clock::authority_proof_started().map_err(|error| {
                RealizationError::authority_lost(
                    AuthorityLoss::ProofClockUnavailable,
                    format!("authority clock: {error}"),
                )
            })?;
            if incoming.remaining_at(now).is_none() {
                return Err(RealizationError::authority_lost(
                    crate::realization_error::AuthorityLoss::Closed,
                    "replacement Session authority time proof is exhausted",
                ));
            }
            let selected = realizations.read_realization(thread, |r| {
                let environment = match &r.environment.owner {
                    SessionEnvironmentOwner::Resident(owned) => &owned.environment,
                    SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::Candidate(candidate))
                        => &candidate.environment,
                    SessionEnvironmentOwner::Retiring(retiring)
                        if retiring.cause == SessionEnvironmentRetirementCause::RealizationRevocation =>
                        return Some(retiring.owned.environment()),
                    _ => return None,
                };
                let same_generation = r.execution.authority.lease().is_some_and(|old| {
                    awaken_session_realization_contract::realization_lease_generation_authorizes(old, &incoming.lease)
                    || awaken_session_realization_contract::realization_lease_generation_authorizes(&incoming.lease, old)
                });
                (!same_generation).then(|| environment.clone())
            })?.flatten();
            let Some(environment) = selected else { return Ok(None) };
            if environment.scoped_execution().is_none() {
                return Ok(None);
            }
            self.retire_current_environment(thread,
                SessionEnvironmentRetirementCause::RealizationRevocation,
                RetirementSelection::Exact(&environment))
        })?;
        if let Some(retirement) = retirement {
            let environment = retirement.owner.owned.environment();
            // Independent participants must both stop even if one drain fails.
            // Cancellation leaves the same Retiring owner for the next caller.
            let (mcp, execution) = tokio::join!(
                mcp.0.drain_projections(mcp.1, thread, &[]),
                environment.lifecycle().stop_bound_processes(),
            );
            mcp?;
            execution.map_err(RealizationError::from)?;
        }
        Ok(())
    }

    /// Join the exact selected local root, verify it after its owner locks,
    /// signal its captured local controls, and retire it through the original
    /// physical owner. The callback grants no durable cancellation authority.
    pub async fn retire_selected_session_environment<S: awaken_session_mcp::McpSessionSlots>(
        &self,
        thread: &str,
        mcp: (&awaken_session_mcp::McpSessions, &S),
        selected: &crate::session_realizations::SessionRealizationRevocationSelection,
        before_retirement: impl FnOnce() + Send,
    ) -> Result<bool, RealizationError> {
        let _realization = selected.realization.lock().await;
        let _lifecycle = selected.lifecycle.lock().await;
        if !self
            .sessions
            .read_realization(thread, |r| selected.matches(r))?
            .unwrap_or(false)
        {
            return Ok(false);
        }
        before_retirement();
        self.retire_session_environment_for_revocation(thread, mcp, Some(selected))
            .await
    }

    /// Revoke process-local authority without dropping the physical owner. The
    /// wrapper enters Retiring before stop/status and stays there while the
    /// Sandbox is live or its outcome is unknown; a later authorized adoption
    /// may reactivate the same exact durable owner.
    pub async fn retire_session_environment_for_revocation<
        S: awaken_session_mcp::McpSessionSlots,
    >(
        &self,
        thread: &str,
        mcp: (&awaken_session_mcp::McpSessions, &S),
        expected_selection: Option<
            &crate::session_realizations::SessionRealizationRevocationSelection,
        >,
    ) -> Result<bool, RealizationError> {
        let (mcp, mcp_slots) = mcp;
        let realizations = &self.sessions;
        let projected = realizations.modify_session(thread, |slot| {
            let r = slot.realization;
            if expected_selection.is_some_and(|selected| !selected.matches(r)) {
                return Ok::<_, RealizationError>(None);
            }
            // Preserve the original Managed-vs-legacy retirement policy, while
            // binding physical transition and MCP selection to this same slot.
            let retained = matches!(
                &r.execution.authority,
                crate::realization_state::SessionRealizationAuthority::Live(_)
                    | crate::realization_state::SessionRealizationAuthority::Closed(Some(_))
            );
            if r.execution.authority.lease().is_some() {
                r.close_authority(None);
            }
            let selected = crate::session_realizations::SessionRealizationRevocationSelection::from_realization(r);
            let original_owner = r.environment.owner.clone();
            let retirement = r.environment.owner.begin_retirement(
                SessionEnvironmentRetirementCause::RealizationRevocation,
                RetirementSelection::Current,
            )?;
            if retirement.is_some() {
                slot.run.drop_context();
            } else if matches!(&original_owner,
                SessionEnvironmentOwner::Preparing(crate::environment_owner::SessionEnvironmentPreparation::AwaitingAdoption { .. })
            ) {
                return Err(RealizationError::pending(
                    PendingState::RevocationPendingAdoption,
                    "Session Environment revocation cannot retire a durable binding without its exact local owner",
                ));
            }
            let selected_projections = r.staging.mcp.select_projections_for_drain();
            let residual_routes = mcp.drain_existing_relay_routes(thread);
            Ok(Some((retained, selected, original_owner, retirement, selected_projections, residual_routes)))
        })?.transpose()?.flatten();
        let Some((
            retained_realization_owner,
            selected,
            original_owner,
            retirement,
            selected_projections,
            residual_routes,
        )) = projected
        else {
            // Absence is not a reusable identity. Do not reselect a new slot,
            // physical owner, projection, or a later Vacant owner by Thread.
            return Ok(false);
        };

        // Some owner phases have no local physical wrapper. In particular an
        // exact pending restore is retained, not treated as an empty slot.
        let expected_owner = retirement.as_ref().map_or(original_owner, |retirement| {
            SessionEnvironmentOwner::Retiring(retirement.clone())
        });
        let (projections, residual) = tokio::join!(
            mcp.drain_selected_projections(mcp_slots, &selected_projections),
            residual_routes,
        );
        projections?;
        residual?;
        let disposes_unbound = retirement.as_ref().is_some_and(|retirement| {
            matches!(&retirement.owned, RetiringEnvironmentOwner::Unbound(_))
                && !retained_realization_owner
        });
        let retirement_result = match retirement.as_ref() {
            Some(retirement) => match &retirement.owned {
                RetiringEnvironmentOwner::Unbound(_) if !retained_realization_owner => {
                    self.dispose_and_confirm_retirement(thread, retirement)
                        .await
                }
                // An absent response is not proof that the Session root did
                // not commit. Retain Managed Candidates for current-authority
                // adoption or canonical terminal cleanup; never dispose them
                // merely because revocation raced receipt delivery.
                _ => retirement
                    .owned
                    .environment()
                    .lifecycle()
                    .stop_bound_processes()
                    .await
                    .map(|_| ())
                    .map_err(RealizationError::from),
            },
            None => Ok(()),
        };
        let retired = realizations.finish_realization_revocation(
            thread,
            &selected,
            &expected_owner,
            disposes_unbound && retirement_result.is_ok(),
        );
        retirement_result?;
        Ok(retired)
    }
}

#[cfg(test)]
mod terminal_output_tests {
    use super::*;

    #[test]
    fn closed_environment_recovery_admission_covers_every_owner_class() {
        // Causes: C1 Reject/Rebuild policy; C2 exact Revocation,
        // RecoveryDiscard, Resident, pending, Vacant, or protected/stale owner.
        // Effects: E1 only Rebuild plus an exact unprotected class consumes; E2
        // every Reject or protected/stale row retains. This is the complete 2x6
        // decision table used before the production plan carries the exact fence.
        // Layout rule: large retirement/pending payloads remain exact owned values
        // behind indirection; small variants and the fail-closed plan must not reserve
        // hundreds of inline bytes across async recovery. A relative pointer-width
        // budget covers the representation independently of target architecture.
        assert!(
            std::mem::size_of::<ClosedEnvironmentRecoveryOwner>()
                <= 4 * std::mem::size_of::<usize>()
        );
        assert!(
            std::mem::size_of::<ClosedEnvironmentRecoveryPlan<'_>>()
                <= 6 * std::mem::size_of::<usize>()
        );
        let owners = [
            ClosedEnvironmentRecoveryOwnerClass::Revoked,
            ClosedEnvironmentRecoveryOwnerClass::RecoveryDiscard,
            ClosedEnvironmentRecoveryOwnerClass::Resident,
            ClosedEnvironmentRecoveryOwnerClass::Pending,
            ClosedEnvironmentRecoveryOwnerClass::Vacant,
            ClosedEnvironmentRecoveryOwnerClass::ProtectedOrStale,
        ];
        for policy in [
            SessionEnvironmentUnavailablePolicy::Reject,
            SessionEnvironmentUnavailablePolicy::Rebuild,
        ] {
            for owner in owners {
                assert_eq!(
                    closed_environment_recovery_admission(policy, owner),
                    policy == SessionEnvironmentUnavailablePolicy::Rebuild
                        && owner != ClosedEnvironmentRecoveryOwnerClass::ProtectedOrStale
                );
            }
        }
    }

    #[test]
    fn terminal_effect_mode_keeps_live_recovery_and_provider_preparation_coupled() {
        /* Process-local mode table HT0. Causes: C1 provider observation is
         * Ready/filesystem-Terminal, Container-Terminal, primary absence, or Disposing; C2 the
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
        assert_eq!(
            TerminalEffectMode::for_terminal_source(true),
            TerminalEffectMode::LiveUnprepared,
            "HT0 filesystem data remains readable"
        );
        assert_eq!(
            TerminalEffectMode::for_terminal_source(false),
            TerminalEffectMode::RecoverOnlyUnprepared,
            "HT0 Container terminal source requires receipt-only reads and still needs preparation"
        );
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
        let resident = awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: "binding-a".into(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        };
        // The aggregate's binding projection already owns exactly the
        // Resident/Suspending source partition. Exercise the consuming artifact
        // edge rather than maintaining a second variant classifier in Host.
        let live_capture = |state| {
            TerminalEnvironmentPreparation {
                state,
                environment: PreparedBoundEnvironment::new(
                    None,
                    None,
                    TerminalEffectMode::LiveUnprepared,
                ),
                pending_checkpoint: None,
            }
            .artifact_capture_mode()
        };
        assert_eq!(
            live_capture(resident),
            Some(crate::artifact_harvest::ArtifactCaptureMode::Live),
            "HT1a"
        );
        assert_eq!(
            live_capture(awaken_session_state_contract::SessionEnvironmentState::Unmaterialized),
            None,
            "HT1b"
        );

        let disposing_non_agent_state = TerminalEnvironmentPreparation {
            state: awaken_session_state_contract::SessionEnvironmentState::Unmaterialized,
            environment: PreparedBoundEnvironment::new(
                None,
                None,
                TerminalEffectMode::RecoverOnlyAlreadyPrepared,
            ),
            pending_checkpoint: None,
        };
        assert_eq!(
            disposing_non_agent_state.artifact_capture_mode(),
            Some(crate::artifact_harvest::ArtifactCaptureMode::ReceiptOnly),
            "HT1c"
        );
    }
}
