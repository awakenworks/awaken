// @formal entrypoint session_environment.checkpoint_receipt_requires_every_immutable_generation_axis
// @formal entrypoint session_environment.quiescence_receipt_requires_exact_operation_epoch_mcp_set_and_zero_live_effects
// @formal entrypoint session_environment.restore_receipt_requires_exact_checkpoint_and_nonempty_binding
// @formal entrypoint session_environment.source_disposal_requires_a_verified_checkpoint
//! Durable identity and continuation state of the execution environment bound
//! to a Session.
//!
//! This module is the sole durable lifecycle authority. Providers own bytes and
//! live handles; they may only advance this state with an exact, verified
//! receipt. Rebuildable processes remain Runtime Host concerns.

mod continuation;
mod effect_intent;
mod generation;
mod legacy_binding;
mod materialization;
mod rebuild_source;
mod receipt_admission;
mod transition;

use crate::state::SessionId;
pub use continuation::{
    SourceReleaseDisposal, SourceReleasePreparationEffect, SourceReleasePreparedReceipt,
};
pub use effect_intent::{
    SessionEnvironmentEffectAuthorization, SessionEnvironmentEffectIntent,
    SessionEnvironmentEffectKind,
};
pub use generation::{
    SandboxGeneration, SuspendPhase, checkpoint_source_disposal_authorized,
    checkpoint_source_preparation_authorized,
};
use generation::{
    checkpoint_receipt_admitted, quiescence_receipt_admitted, restore_receipt_admitted,
    source_disposal_receipt_admitted, source_release_preparation_receipt_admitted,
};
pub use materialization::SessionEnvironmentMaterialization;
pub use rebuild_source::{RebuildSourceAccess, RebuildSourceEvidence};
use receipt_admission::reservation_binding_is_monotonic;
pub use receipt_admission::{
    SessionEnvironmentReceiptError, SessionEnvironmentReceiptRecoveryAction,
    SessionEnvironmentTransitionError,
};
pub use transition::{
    MaterializationKind, RealizationLeaseStanding, SessionEnvironmentDecision,
    SessionEnvironmentEvent, SessionEnvironmentFacts, SessionEnvironmentInstruction,
    SessionEnvironmentRejection, SessionEnvironmentStep, SessionTerminality,
    transition_environment,
};

/// Stable effect identity. Recovery always reuses this value rather than
/// creating another checkpoint or restore authority.
///
/// Operation capability table: the existing constructors select one identity;
/// O1/O2 shared getters preserve its exact coordinates without a mutable view.
/// The four independent O3 field-write examples close separate source paths;
/// serialization remains the explicit legacy/untrusted import boundary, not an
/// execution grant. Original R1-R5 start/replay tests own runtime compatibility.
///
/// ```
/// use awaken_session_contract::{SandboxGeneration, SessionEnvironmentOperation};
/// use awaken_session_types::{SessionId};
/// fn build(session: &SessionId, generation: &SandboxGeneration) -> SessionEnvironmentOperation {
///     SessionEnvironmentOperation::new("workspace", session, "suspend", generation, 7, None, None)
/// }
/// ```
///
/// ```
/// use awaken_session_contract::SessionEnvironmentOperation;
/// use awaken_session_configuration_contract::resource::SessionRealizationLease;
/// fn read(operation: &SessionEnvironmentOperation) {
///     let _: &str = operation.effect_id();
///     let _: u64 = operation.activity_epoch();
///     let _: u64 = operation.started_at_unix_ms();
///     let _: Option<&SessionRealizationLease> = operation.realization();
/// }
/// ```
///
/// ```compile_fail,E0616
/// use awaken_session_contract::SessionEnvironmentOperation;
/// fn overwrite(operation: &mut SessionEnvironmentOperation) {
///     operation.effect_id = String::new();
/// }
/// ```
///
/// ```compile_fail,E0616
/// use awaken_session_contract::SessionEnvironmentOperation;
/// fn overwrite(operation: &mut SessionEnvironmentOperation) {
///     operation.activity_epoch = 0;
/// }
/// ```
///
/// ```compile_fail,E0616
/// use awaken_session_contract::SessionEnvironmentOperation;
/// fn overwrite(operation: &mut SessionEnvironmentOperation) {
///     operation.started_at_unix_ms = 0;
/// }
/// ```
///
/// ```compile_fail,E0616
/// use awaken_session_contract::SessionEnvironmentOperation;
/// fn overwrite(operation: &mut SessionEnvironmentOperation) {
///     operation.realization = None;
/// }
/// ```
///
/// ```compile_fail,E0308
/// use awaken_session_contract::SessionEnvironmentOperation;
/// fn mutable(operation: &mut SessionEnvironmentOperation) {
///     let _: &mut str = operation.effect_id();
/// }
/// ```
///
/// ```compile_fail,E0308
/// use awaken_session_contract::SessionEnvironmentOperation;
/// use awaken_session_configuration_contract::resource::SessionRealizationLease;
/// fn mutable(operation: &mut SessionEnvironmentOperation) {
///     let _: Option<&mut SessionRealizationLease> = operation.realization();
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionEnvironmentOperation {
    pub(crate) effect_id: String,
    pub(crate) activity_epoch: u64,
    /// Wall-clock edge captured when the aggregate first commits this
    /// operation. Every retry reuses it as checkpoint metadata; reading a new
    /// clock value after response loss would turn one effect into two distinct
    /// object writes. Legacy rows decode to zero and retain their already-stored
    /// effect identity.
    #[serde(default)]
    pub(crate) started_at_unix_ms: u64,
    pub(crate) realization: Option<crate::state::SessionRealizationLease>,
}

pub(crate) use awaken_provisioning_contract::{SandboxCheckpointRef, SandboxRestoreRequest};

/// Exact bounds and aggregate identity for one idempotent checkpoint effect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxCheckpointRequest {
    pub workspace_id: String,
    pub session_id: crate::state::SessionId,
    pub operation: SessionEnvironmentOperation,
    /// Exact create/adopt/restore effect and binding that own the physical
    /// checkpoint source. The suspend operation is only the release cause.
    pub source_effect_id: String,
    pub source_binding: String,
    pub generation: SandboxGeneration,
    pub format: String,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub max_bytes: u64,
}

/// Typed selector for a global count over the canonical Session environment
/// state: the phase of [`SessionEnvironmentState::step`] (see
/// [`SessionEnvironmentStep::phase`]). A read projection only;
/// [`SessionEnvironmentState`] remains the sole durable lifecycle authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEnvironmentPhase {
    Unmaterialized,
    Resident,
    Suspending,
    Hibernated,
    Restoring,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum SessionEnvironmentState {
    #[default]
    Unmaterialized,
    Resident {
        binding: String,
        /// Stable identity of the create/adopt/restore effect that last proved
        /// this binding. Legacy rows omit it and are upgraded on the next receipt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effect_id: Option<String>,
        /// Legacy rows had no generation. They remain resident and are assigned
        /// a generation by the next create/adopt operation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        generation: Option<SandboxGeneration>,
        /// Durable idle edge used by the one lifecycle supervisor. Activity
        /// clears it before Runtime I/O; legacy rows start without a timer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idle_since_unix_ms: Option<u64>,
    },
    Suspending {
        operation: SessionEnvironmentOperation,
        /// Exact physical owner selected before suspension began. Keeping this
        /// separate from the suspend effect prevents cleanup from replacing
        /// create/adopt/restore ownership.
        #[serde(default)]
        source_effect_id: Box<String>,
        source_binding: String,
        generation: SandboxGeneration,
        suspend_phase: SuspendPhase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkpoint: Option<SandboxCheckpointRef>,
        /// Exact aggregate-authorized effect that completed every live
        /// source-dependent durability participant. Legacy/non-disposing rows
        /// decode without it; physical disposal requires it to be present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_release_preparation: Option<Box<SourceReleasePreparedReceipt>>,
    },
    Hibernated {
        checkpoint: SandboxCheckpointRef,
        generation: SandboxGeneration,
    },
    Restoring {
        operation: SessionEnvironmentOperation,
        checkpoint: SandboxCheckpointRef,
        generation: SandboxGeneration,
    },
}

impl SessionEnvironmentState {
    /// Confirm a Store-read Resident against an already verified receipt. This
    /// does not authorize a phase transition or grant current execution rights.
    pub fn committed_receipt_generation(
        &self,
        receipt: &SessionEnvironmentReceipt,
    ) -> Result<&SandboxGeneration, SessionEnvironmentReceiptError> {
        let Self::Resident {
            binding,
            effect_id: Some(effect_id),
            generation: Some(generation),
            ..
        } = self
        else {
            return Err(SessionEnvironmentReceiptError::Rejected(
                SessionEnvironmentRejection::RequiresResident,
            ));
        };
        if binding != &receipt.binding || effect_id != &receipt.effect_id {
            return Err(SessionEnvironmentReceiptError::Mismatch);
        }
        Ok(generation)
    }

    /// The committed suspend operation with the exact physical source it
    /// releases (source effect, source binding, generation); `None` outside
    /// Suspending.
    #[must_use]
    pub fn suspend_source(
        &self,
    ) -> Option<(&SessionEnvironmentOperation, &str, &str, &SandboxGeneration)> {
        match self {
            Self::Suspending {
                operation,
                source_effect_id,
                source_binding,
                generation,
                ..
            } => Some((operation, source_effect_id, source_binding, generation)),
            _ => None,
        }
    }

    #[must_use]
    pub fn binding(&self) -> Option<&str> {
        match self {
            Self::Resident { binding, .. } => Some(binding),
            Self::Suspending { source_binding, .. } => Some(source_binding),
            Self::Unmaterialized | Self::Hibernated { .. } | Self::Restoring { .. } => None,
        }
    }

    /// Project the exact live source that may satisfy a terminal Repository
    /// publication. Terminal cleanup may adopt an existing Resident or
    /// pre-disposal suspension source, but ADR-0074 forbids restoring a
    /// Hibernated Environment merely to publish it and forbids source I/O once
    /// the durable disposal phase has begun.
    #[must_use]
    pub fn terminal_repository_publication_binding(&self) -> Option<&str> {
        match self {
            Self::Resident { binding, .. } => Some(binding),
            Self::Suspending {
                source_binding,
                suspend_phase:
                    SuspendPhase::Quiescing | SuspendPhase::Uploading | SuspendPhase::ReadyToDispose,
                ..
            } => Some(source_binding),
            Self::Unmaterialized
            | Self::Suspending {
                suspend_phase: SuspendPhase::Disposing,
                ..
            }
            | Self::Hibernated { .. }
            | Self::Restoring { .. } => None,
        }
    }

    #[must_use]
    pub fn effect_id(&self) -> Option<&str> {
        match self {
            Self::Resident { effect_id, .. } => effect_id.as_deref(),
            Self::Suspending { operation, .. } | Self::Restoring { operation, .. } => {
                Some(&operation.effect_id)
            }
            Self::Unmaterialized | Self::Hibernated { .. } => None,
        }
    }

    #[must_use]
    pub fn generation(&self) -> Option<&SandboxGeneration> {
        match self {
            Self::Resident { generation, .. } => generation.as_ref(),
            Self::Suspending { generation, .. }
            | Self::Hibernated { generation, .. }
            | Self::Restoring { generation, .. } => Some(generation),
            Self::Unmaterialized => None,
        }
    }

    #[must_use]
    pub fn checkpoint(&self) -> Option<&SandboxCheckpointRef> {
        match self {
            Self::Suspending { checkpoint, .. } => checkpoint.as_ref(),
            Self::Hibernated { checkpoint, .. } | Self::Restoring { checkpoint, .. } => {
                Some(checkpoint)
            }
            Self::Unmaterialized | Self::Resident { .. } => None,
        }
    }

    /// Project the sole durable Restoring tuple into the provider-neutral,
    /// exact physical target used by restore and terminal orphan disposal.
    #[must_use]
    pub fn restoring_request(
        &self,
        workspace_id: &str,
        session_id: &SessionId,
    ) -> Option<SandboxRestoreRequest> {
        let Self::Restoring {
            operation,
            checkpoint,
            generation,
        } = self
        else {
            return None;
        };
        Some(SandboxRestoreRequest {
            workspace_id: workspace_id.to_owned(),
            session_id: session_id.as_str().to_owned(),
            effect_id: operation.effect_id.clone(),
            generation_id: generation.id().to_owned(),
            checkpoint: checkpoint.clone(),
        })
    }

    /// Reconstruct the one exact checkpoint request authorized by a committed
    /// Uploading state. This is the shared projection for ordinary continuation
    /// and terminal recovery; adapters must not rebuild these fields from a
    /// fresh clock or local cache.
    pub fn checkpoint_request(
        &self,
        workspace_id: &str,
        session_id: &SessionId,
        policy: &crate::state::SandboxIdleRetentionPolicy,
    ) -> Result<Option<SandboxCheckpointRequest>, SessionEnvironmentTransitionError> {
        let (operation, generation) = match self {
            Self::Suspending {
                operation,
                generation,
                suspend_phase: SuspendPhase::Uploading,
                checkpoint: None,
                ..
            } => (operation, generation),
            Self::Suspending {
                suspend_phase: SuspendPhase::Uploading,
                checkpoint: Some(_),
                ..
            } => {
                return Err(SessionEnvironmentTransitionError::Rejected(
                    SessionEnvironmentRejection::InconsistentEvidence,
                ));
            }
            _ => return Ok(None),
        };
        if policy.mode != crate::state::SandboxIdleRetentionMode::CheckpointAndRelease {
            return Err(SessionEnvironmentTransitionError::InvalidCheckpointPolicy(
                "checkpoint operation requires checkpoint-and-release mode",
            ));
        }
        policy
            .validate()
            .map_err(SessionEnvironmentTransitionError::InvalidCheckpointPolicy)?;
        let Self::Suspending {
            source_effect_id,
            source_binding,
            ..
        } = self
        else {
            unreachable!("Uploading request was matched above")
        };
        if source_effect_id.is_empty() || source_binding.is_empty() {
            return Err(SessionEnvironmentTransitionError::ReceiptMismatch);
        }
        Ok(Some(SandboxCheckpointRequest {
            workspace_id: workspace_id.to_owned(),
            session_id: session_id.clone(),
            operation: operation.clone(),
            source_effect_id: source_effect_id.as_ref().clone(),
            source_binding: source_binding.clone(),
            generation: generation.clone(),
            format: policy.checkpoint_format.clone(),
            created_at_unix_ms: operation.started_at_unix_ms,
            expires_at_unix_ms: generation.expires_at_unix_ms(),
            max_bytes: policy.max_checkpoint_bytes,
        }))
    }

    pub fn set_resident(&mut self, binding: impl Into<String>) {
        *self = Self::legacy_resident(binding);
    }

    /// Authorize one Environment effect before a provider performs I/O. This is
    /// the same phase/source/Resource-transition kernel consumed by receipt
    /// application, so admission cannot drift across the physical effect.
    pub fn authorize_effect(
        &self,
        intent: &SessionEnvironmentEffectIntent,
        expected_environment_fingerprint: &str,
        pending_resource_transition_fingerprint: Option<&str>,
    ) -> Result<SessionEnvironmentEffectAuthorization, SessionEnvironmentReceiptError> {
        intent.verify()?;
        if intent.environment_fingerprint() != Some(expected_environment_fingerprint) {
            return Err(SessionEnvironmentReceiptError::EnvironmentMismatch);
        }
        if let (
            Self::Restoring {
                operation,
                checkpoint,
                ..
            },
            SessionEnvironmentEffectKind::Restore {
                operation: asserted,
                checkpoint_id,
            },
        ) = (self, intent.kind())
        {
            if operation == asserted.as_ref()
                && checkpoint.id == *checkpoint_id
                && intent.source_binding().is_none()
            {
                return Ok(SessionEnvironmentEffectAuthorization::Authorized {
                    intent: Box::new(intent.clone()),
                });
            }
            return Err(SessionEnvironmentReceiptError::Mismatch);
        }
        if matches!(
            self,
            Self::Suspending { .. } | Self::Hibernated { .. } | Self::Restoring { .. }
        ) {
            return Err(SessionEnvironmentReceiptError::Rejected(
                SessionEnvironmentRejection::ContinuationInProgress,
            ));
        }

        if let SessionEnvironmentEffectKind::ResourceProjectionReservation {
            transition_fingerprint,
        } = intent.kind()
            && pending_resource_transition_fingerprint != Some(transition_fingerprint.as_str())
        {
            return Err(SessionEnvironmentReceiptError::ResourceTransitionMismatch);
        }

        if let Self::Resident {
            binding, effect_id, ..
        } = self
            && effect_id.as_deref() == Some(intent.effect_id())
        {
            return Ok(SessionEnvironmentEffectAuthorization::AlreadyApplied {
                binding: binding.clone(),
                intent: Box::new(intent.clone()),
            });
        }

        if matches!(
            intent.kind(),
            SessionEnvironmentEffectKind::ResourceProjectionReservation { .. }
        ) && let Self::Resident { binding, .. } = self
            && intent.source_binding() != Some(binding.as_str())
        {
            return Err(SessionEnvironmentReceiptError::InvalidReservation);
        }

        match (self, intent.kind()) {
            (Self::Unmaterialized, SessionEnvironmentEffectKind::Create)
                if intent.source_binding().is_none() => {}
            (Self::Unmaterialized, SessionEnvironmentEffectKind::Adopt)
                if intent.source_binding().is_some() => {}
            (
                Self::Resident {
                    binding,
                    generation: Some(generation),
                    ..
                },
                SessionEnvironmentEffectKind::Rebuild {
                    source_generation_id,
                },
            ) if intent.source_binding() == Some(binding.as_str())
                && source_generation_id.as_deref() == Some(generation.id()) => {}
            (
                Self::Resident {
                    binding,
                    generation: None,
                    ..
                },
                SessionEnvironmentEffectKind::Rebuild {
                    source_generation_id: None,
                },
            ) if intent.source_binding() == Some(binding.as_str()) => {}
            (Self::Resident { binding, .. }, SessionEnvironmentEffectKind::Adopt)
                if intent.source_binding() == Some(binding.as_str()) => {}
            (
                Self::Resident { binding, .. },
                SessionEnvironmentEffectKind::ResourceProjectionReservation { .. },
            ) if intent.source_binding() == Some(binding.as_str()) => {}
            (Self::Unmaterialized, _) => {
                return Err(SessionEnvironmentReceiptError::Rejected(
                    SessionEnvironmentRejection::RequiresResident,
                ));
            }
            (Self::Resident { .. }, _) => {
                return Err(SessionEnvironmentReceiptError::Rejected(
                    SessionEnvironmentRejection::InvalidTransition,
                ));
            }
            (Self::Suspending { .. } | Self::Hibernated { .. } | Self::Restoring { .. }, _) => {
                unreachable!("continuation phases were rejected above")
            }
        }

        Ok(SessionEnvironmentEffectAuthorization::Authorized {
            intent: Box::new(intent.clone()),
        })
    }

    /// Apply one ordinary Environment receipt through the aggregate's closed
    /// state machine. Provider adapters may prove bytes and liveness, but only
    /// this owner can decide whether that evidence is causally valid for the
    /// current phase and pending Resource transition.
    pub fn try_apply_receipt(
        &mut self,
        receipt: &SessionEnvironmentReceipt,
        expected_environment_fingerprint: &str,
        pending_resource_transition_fingerprint: Option<&str>,
    ) -> Result<bool, SessionEnvironmentReceiptError> {
        receipt.verify()?;
        let intent = receipt.effect_intent()?;
        let authorization = self.authorize_effect(
            &intent,
            expected_environment_fingerprint,
            pending_resource_transition_fingerprint,
        )?;

        self.apply_authorized_receipt(receipt, authorization)
    }

    pub(crate) fn apply_authorized_receipt(
        &mut self,
        receipt: &SessionEnvironmentReceipt,
        authorization: SessionEnvironmentEffectAuthorization,
    ) -> Result<bool, SessionEnvironmentReceiptError> {
        if let SessionEnvironmentEffectAuthorization::AlreadyApplied { binding, .. } = authorization
        {
            if binding != receipt.binding {
                return Err(
                    if matches!(
                        receipt.kind,
                        SessionEnvironmentEffectKind::ResourceProjectionReservation { .. }
                    ) {
                        SessionEnvironmentReceiptError::InvalidReservation
                    } else {
                        SessionEnvironmentReceiptError::Rejected(
                            SessionEnvironmentRejection::InvalidTransition,
                        )
                    },
                );
            }
            if matches!(
                receipt.kind,
                SessionEnvironmentEffectKind::ResourceProjectionReservation { .. }
            ) {
                let source = receipt
                    .source_binding
                    .as_deref()
                    .ok_or(SessionEnvironmentReceiptError::InvalidReservation)?;
                if !reservation_binding_is_monotonic(source, &binding)? {
                    return Err(SessionEnvironmentReceiptError::InvalidReservation);
                }
            }
            return Ok(false);
        }
        if matches!(
            authorization,
            SessionEnvironmentEffectAuthorization::Unowned { .. }
        ) {
            return Err(SessionEnvironmentReceiptError::Rejected(
                SessionEnvironmentRejection::InvalidTransition,
            ));
        }

        let idle_since_unix_ms = self.idle_since_unix_ms();
        let generation = self.generation().cloned();
        match (&*self, &receipt.kind) {
            (Self::Unmaterialized, SessionEnvironmentEffectKind::Create) => {}
            (Self::Unmaterialized, SessionEnvironmentEffectKind::Adopt)
                if receipt.source_binding.as_deref() == Some(receipt.binding.as_str()) => {}
            (Self::Unmaterialized, SessionEnvironmentEffectKind::Adopt) => {
                return Err(SessionEnvironmentReceiptError::Rejected(
                    SessionEnvironmentRejection::InvalidTransition,
                ));
            }
            (Self::Resident { .. }, SessionEnvironmentEffectKind::Rebuild { .. }) => {}
            (Self::Resident { binding, .. }, SessionEnvironmentEffectKind::Adopt)
                if receipt.binding.as_str() == binding.as_str() => {}
            (
                Self::Resident { binding, .. },
                SessionEnvironmentEffectKind::ResourceProjectionReservation { .. },
            ) => {
                let source = receipt
                    .source_binding
                    .as_deref()
                    .ok_or(SessionEnvironmentReceiptError::InvalidReservation)?;
                if source != binding || !reservation_binding_is_monotonic(source, &receipt.binding)?
                {
                    return Err(SessionEnvironmentReceiptError::InvalidReservation);
                }
            }
            (Self::Unmaterialized, _) => {
                return Err(SessionEnvironmentReceiptError::Rejected(
                    SessionEnvironmentRejection::RequiresResident,
                ));
            }
            (Self::Resident { .. }, _) => {
                return Err(SessionEnvironmentReceiptError::Rejected(
                    SessionEnvironmentRejection::InvalidTransition,
                ));
            }
            (Self::Suspending { .. } | Self::Hibernated { .. } | Self::Restoring { .. }, _) => {
                unreachable!("continuation phases were rejected above")
            }
        }

        *self = Self::Resident {
            binding: receipt.binding.clone(),
            effect_id: Some(receipt.effect_id.clone()),
            generation,
            idle_since_unix_ms,
        };
        Ok(true)
    }

    pub fn mark_active(&mut self) {
        if let Self::Resident {
            idle_since_unix_ms, ..
        } = self
        {
            *idle_since_unix_ms = None;
        }
    }

    pub fn mark_idle(&mut self, now_unix_ms: u64) {
        if let Self::Resident {
            idle_since_unix_ms, ..
        } = self
        {
            *idle_since_unix_ms = Some(now_unix_ms);
        }
    }

    #[must_use]
    pub fn idle_since_unix_ms(&self) -> Option<u64> {
        match self {
            Self::Resident {
                idle_since_unix_ms, ..
            } => *idle_since_unix_ms,
            _ => None,
        }
    }

    /// Upgrade a legacy/create receipt to the immutable generation computed
    /// from the Session's frozen Environment. Adoption never rotates it.
    pub fn assign_generation(&mut self, next: SandboxGeneration) -> bool {
        let Self::Resident { generation, .. } = self else {
            return false;
        };
        if generation.is_some() {
            return false;
        }
        *generation = Some(next);
        true
    }
}

/// Secret-free evidence that one exact owner created or adopted the Session
/// environment. The durable binding is committed only after this receipt passes
/// the aggregate's realization fence.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionEnvironmentReceipt {
    pub session_id: crate::state::SessionId,
    pub effect_id: String,
    pub kind: SessionEnvironmentEffectKind,
    /// Exact prior aggregate binding. It is absent only for first creation;
    /// adoption, rebuild, and path reservation cannot blindly replace state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_binding: Option<String>,
    /// Immutable Session Environment compatibility fingerprint. The aggregate
    /// validates it against the frozen baseline before changing state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_fingerprint: Option<String>,
    pub binding: String,
    pub realization: Option<crate::state::SessionRealizationLease>,
    pub receipt_fingerprint: String,
}

impl SessionEnvironmentReceipt {
    #[must_use]
    pub fn new(
        session_id: impl Into<crate::state::SessionId>,
        kind: SessionEnvironmentEffectKind,
        binding: impl Into<String>,
        realization: Option<crate::state::SessionRealizationLease>,
    ) -> Self {
        let binding = binding.into();
        let intent = SessionEnvironmentEffectIntent::new(session_id, kind, realization);
        Self::from_verified_intent(&intent, binding)
    }

    /// Bind the receipt to the exact immutable Environment configuration.
    #[must_use]
    pub fn for_environment(mut self, environment_fingerprint: impl Into<String>) -> Self {
        self.environment_fingerprint = Some(environment_fingerprint.into());
        self.refresh_fingerprints();
        self
    }

    /// Bind an adopt, rebuild, or Resource reservation to the exact current
    /// aggregate binding it is allowed to advance.
    #[must_use]
    pub fn from_binding(mut self, source_binding: impl Into<String>) -> Self {
        self.source_binding = Some(source_binding.into());
        self.refresh_fingerprints();
        self
    }

    fn refresh_fingerprints(&mut self) {
        let intent = SessionEnvironmentEffectIntent::new(
            self.session_id.clone(),
            self.kind.clone(),
            self.realization.clone(),
        )
        .for_frozen_coordinates(
            self.source_binding.clone(),
            self.environment_fingerprint.clone(),
        );
        *self = Self::from_verified_intent(&intent, self.binding.clone());
    }

    fn from_verified_intent(intent: &SessionEnvironmentEffectIntent, binding: String) -> Self {
        let mut receipt = Self {
            session_id: crate::state::SessionId::from(intent.session_id().to_string()),
            effect_id: intent.effect_id().to_string(),
            kind: intent.kind().clone(),
            source_binding: intent.source_binding().map(str::to_string),
            environment_fingerprint: intent.environment_fingerprint().map(str::to_string),
            binding,
            realization: intent.realization().cloned(),
            receipt_fingerprint: String::new(),
        };
        receipt.receipt_fingerprint = crate::state::stable_fingerprint(&(
            &receipt.session_id,
            &receipt.effect_id,
            &receipt.kind,
            receipt.source_binding.as_deref(),
            receipt.environment_fingerprint.as_deref(),
            &receipt.binding,
            &receipt.realization,
        ));
        receipt
    }

    pub fn from_intent(
        intent: &SessionEnvironmentEffectIntent,
        binding: impl Into<String>,
    ) -> Result<Self, SessionEnvironmentReceiptError> {
        intent.verify()?;
        Ok(Self::from_verified_intent(intent, binding.into()))
    }

    pub fn effect_intent(
        &self,
    ) -> Result<SessionEnvironmentEffectIntent, SessionEnvironmentReceiptError> {
        SessionEnvironmentEffectIntent::from_persisted_receipt(
            self.session_id.to_string(),
            self.effect_id.clone(),
            self.kind.clone(),
            self.source_binding.clone(),
            self.environment_fingerprint.clone(),
            self.realization.clone(),
        )
    }

    pub fn verify(&self) -> Result<(), SessionEnvironmentReceiptError> {
        let intent = self.effect_intent()?;
        let expected = Self::from_verified_intent(&intent, self.binding.clone());
        if self == &expected {
            Ok(())
        } else {
            Err(SessionEnvironmentReceiptError::Mismatch)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QuiescenceReceipt {
    pub effect_id: String,
    pub generation_id: String,
    pub activity_epoch: u64,
    pub live_environment_effects: u32,
    /// Exact durable MCP owners requested by the Session aggregate. Runtime
    /// echoes this set after closing admission and draining every generation.
    #[serde(default)]
    pub mcp_generations: Vec<crate::state::McpGenerationRef>,
}

impl QuiescenceReceipt {
    pub fn verify(
        &self,
        operation: &SessionEnvironmentOperation,
        generation: &SandboxGeneration,
        expected_mcp_generations: &[crate::state::McpGenerationRef],
    ) -> Result<(), SessionEnvironmentTransitionError> {
        if quiescence_receipt_admitted(
            self.effect_id == operation.effect_id,
            self.generation_id == generation.id(),
            self.activity_epoch == operation.activity_epoch,
            self.mcp_generations == expected_mcp_generations,
            self.live_environment_effects,
        ) {
            Ok(())
        } else {
            Err(SessionEnvironmentTransitionError::ReceiptMismatch)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CheckpointReceipt {
    pub effect_id: String,
    pub generation_id: String,
    pub checkpoint: SandboxCheckpointRef,
}

impl CheckpointReceipt {
    pub fn verify(
        &self,
        operation: &SessionEnvironmentOperation,
        generation: &SandboxGeneration,
    ) -> Result<(), SessionEnvironmentTransitionError> {
        if checkpoint_receipt_admitted(
            self.effect_id == operation.effect_id,
            self.generation_id == generation.id(),
            self.checkpoint.suspend_effect_id == operation.effect_id,
            self.checkpoint.environment_fingerprint == generation.environment_fingerprint(),
            self.checkpoint.base_image_fingerprint == generation.base_image_fingerprint(),
        ) {
            Ok(())
        } else {
            Err(SessionEnvironmentTransitionError::ReceiptMismatch)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SourceDisposedReceipt {
    pub effect_id: String,
    pub generation_id: String,
    pub source_binding: String,
    pub terminated: bool,
}

impl SourceDisposedReceipt {
    pub fn verify(
        &self,
        operation: &SessionEnvironmentOperation,
        generation: &SandboxGeneration,
        source_binding: &str,
    ) -> Result<(), SessionEnvironmentTransitionError> {
        if source_disposal_receipt_admitted(
            self.effect_id == operation.effect_id,
            self.generation_id == generation.id(),
            self.source_binding == source_binding,
            self.terminated,
        ) {
            Ok(())
        } else {
            Err(SessionEnvironmentTransitionError::ReceiptMismatch)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RestoreReceipt {
    pub effect_id: String,
    pub generation_id: String,
    pub checkpoint_id: String,
    pub binding: String,
}

impl RestoreReceipt {
    pub fn verify(
        &self,
        operation: &SessionEnvironmentOperation,
        generation: &SandboxGeneration,
        checkpoint: &SandboxCheckpointRef,
    ) -> Result<(), SessionEnvironmentTransitionError> {
        if restore_receipt_admitted(
            self.effect_id == operation.effect_id,
            self.generation_id == generation.id(),
            self.checkpoint_id == checkpoint.id,
            !self.binding.is_empty(),
        ) {
            Ok(())
        } else {
            Err(SessionEnvironmentTransitionError::ReceiptMismatch)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn generation() -> SandboxGeneration {
        SandboxGeneration::new(
            &crate::state::SessionId::from("s1"),
            10,
            1_000,
            "env",
            "image",
        )
    }

    fn realization(expires_at_unix_ms: u64) -> crate::state::SessionRealizationLease {
        crate::state::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "runtime-a".into(),
            epoch: 4,
            expires_at_unix_ms,
        }
    }

    fn resident() -> SessionEnvironmentState {
        SessionEnvironmentState::Resident {
            binding: "source".into(),
            effect_id: Some("create".into()),
            generation: Some(generation()),
            idle_since_unix_ms: None,
        }
    }

    fn checkpoint(operation: &SessionEnvironmentOperation) -> SandboxCheckpointRef {
        SandboxCheckpointRef {
            id: "checkpoint".into(),
            format: "awaken-fs-v1".into(),
            digest: "digest".into(),
            size_bytes: 42,
            created_at_unix_ms: 20,
            expires_at_unix_ms: 1_000,
            environment_fingerprint: "env".into(),
            base_image_fingerprint: "image".into(),
            excluded_mounts: vec!["credential".into()],
            suspend_effect_id: operation.effect_id.clone(),
        }
    }

    fn source_release_prepared(
        operation: &SessionEnvironmentOperation,
        lease: &crate::state::SessionRealizationLease,
    ) -> SourceReleasePreparedReceipt {
        let preparation =
            SourceReleasePreparationEffect::new(operation.clone(), lease.clone()).unwrap();
        SourceReleasePreparedReceipt::try_new(
            preparation.clone(),
            preparation.sandbox_effect_fence().unwrap(),
            &generation(),
            "source",
        )
        .unwrap()
    }

    fn environment_receipt(
        kind: SessionEnvironmentEffectKind,
        binding: &str,
        source_binding: Option<&str>,
    ) -> SessionEnvironmentReceipt {
        let receipt =
            SessionEnvironmentReceipt::new("s1", kind, binding, None).for_environment("env");
        match source_binding {
            Some(source) => receipt.from_binding(source),
            None => receipt,
        }
    }

    #[test]
    fn committed_receipt_requires_resident_and_every_store_identity_axis() {
        // C1 Resident; C2 effect present; C3 generation present; C4 exact binding;
        // C5 exact effect. Rule S1=all five -> return Store generation;
        // S2=any false -> reject. All 32 rows are reachable readback inputs.
        // S3 continuation can expose matching binding/effect/generation getters
        // but is not a Resident readback and cannot confirm ordinary publication.
        let receipt = environment_receipt(SessionEnvironmentEffectKind::Create, "source", None);
        for row in 0..32 {
            let state = if row & 1 == 0 {
                SessionEnvironmentState::Unmaterialized
            } else {
                SessionEnvironmentState::Resident {
                    binding: if row & 8 != 0 {
                        receipt.binding.clone()
                    } else {
                        "other".into()
                    },
                    effect_id: (row & 2 != 0).then(|| {
                        if row & 16 != 0 {
                            receipt.effect_id.clone()
                        } else {
                            "other".into()
                        }
                    }),
                    generation: (row & 4 != 0).then(generation),
                    idle_since_unix_ms: None,
                }
            };
            let actual = state.committed_receipt_generation(&receipt);
            assert_eq!(actual.is_ok(), row == 31, "S1/S2 row={row}");
            if row == 31 {
                assert_eq!(actual.unwrap(), &generation());
            }
        }
        let suspended = SessionEnvironmentState::Suspending {
            operation: SessionEnvironmentOperation {
                effect_id: receipt.effect_id.clone(),
                activity_epoch: 1,
                started_at_unix_ms: 1,
                realization: None,
            },
            source_effect_id: Box::new(receipt.effect_id.clone()),
            source_binding: receipt.binding.clone(),
            generation: generation(),
            suspend_phase: SuspendPhase::Quiescing,
            checkpoint: None,
            source_release_preparation: None,
        };
        assert_eq!(suspended.binding(), Some(receipt.binding.as_str()));
        assert_eq!(suspended.effect_id(), Some(receipt.effect_id.as_str()));
        assert_eq!(suspended.generation(), Some(&generation()));
        assert!(
            suspended.committed_receipt_generation(&receipt).is_err(),
            "S3"
        );
    }

    #[test]
    fn authorization_readback_preserves_expiry_but_rejects_operation_drift() {
        // C1 original expiry < request expiry; C2 same Session/effect; C3 result
        // variant Unowned/Authorized/AlreadyApplied. A1=C2+either C1+any C3 ->
        // preserve original expiry. A2=!C2 -> reject for every variant. The
        // result is identity confirmation only, never a current execution grant.
        let original = SessionEnvironmentEffectIntent::new(
            "s1",
            SessionEnvironmentEffectKind::Create,
            Some(realization(100)),
        );
        for variant in 0..3 {
            let result = match variant {
                0 => SessionEnvironmentEffectAuthorization::Unowned {
                    intent: Box::new(original.clone()),
                },
                1 => SessionEnvironmentEffectAuthorization::Authorized {
                    intent: Box::new(original.clone()),
                },
                _ => SessionEnvironmentEffectAuthorization::AlreadyApplied {
                    binding: "source".into(),
                    intent: Box::new(original.clone()),
                },
            };
            for expiry in [100, 200] {
                let request = SessionEnvironmentEffectIntent::new(
                    "s1",
                    SessionEnvironmentEffectKind::Create,
                    Some(realization(expiry)),
                );
                result.verify_for(&request).unwrap();
                assert_eq!(result.intent(), &original, "A1");
            }
            for request in [
                SessionEnvironmentEffectIntent::new(
                    "other",
                    SessionEnvironmentEffectKind::Create,
                    Some(realization(100)),
                ),
                SessionEnvironmentEffectIntent::new(
                    "s1",
                    SessionEnvironmentEffectKind::Adopt,
                    Some(realization(100)),
                )
                .from_binding("source"),
                original.clone().for_environment("other-fingerprint"),
            ] {
                assert!(result.verify_for(&request).is_err(), "A2 variant={variant}");
            }
        }
    }

    fn local_binding_for(sandbox_id: &str, fingerprint: &str, paths: &[&str]) -> String {
        serde_json::to_string(
            &awaken_provisioning_contract::test_support::fixture_current_handle(
                sandbox_id,
                &awaken_provisioning_contract::test_support::FixtureHandleLocator {
                    outputs_path: "/outputs".into(),
                    physical_incarnation: "environment-test-incarnation".into(),
                },
                &serde_json::from_value(serde_json::json!(fingerprint))
                    .expect("test realization fingerprint"),
                &awaken_provisioning_contract::SandboxEffectFence::new(
                    "environment-test-effect",
                    "environment-test-owner",
                    "environment-test-runtime",
                    1,
                    u64::MAX,
                )
                .expect("test filesystem effect fence"),
                &paths
                    .iter()
                    .map(|path| (*path).to_string())
                    .collect::<Vec<_>>(),
            ),
        )
        .expect("typed local binding")
    }

    #[test]
    fn terminal_repository_publication_projects_only_an_existing_live_source() {
        // Cause/effect graph: C1 is the aggregate Environment phase; C2 is
        // whether that phase still owns a source binding on which publication
        // may perform Repository I/O. E1 projects that exact binding; E2
        // projects no authority, so archive/claim/Host recovery must reject
        // before restoring, publishing, or disposing anything.
        //
        // | Rule | Environment phase | Existing publication source | Effect |
        // | P1 | Resident | yes | E1 |
        // | P2 | Suspending Quiescing/Uploading/ReadyToDispose | yes | E1 |
        // | P3 | Unmaterialized/Hibernated/Restoring | no | E2 |
        // | P4 | Suspending Disposing | no source I/O permitted | E2 |
        let operation = SessionEnvironmentOperation {
            effect_id: "suspend-publication".into(),
            activity_epoch: 7,
            started_at_unix_ms: 20,
            realization: Some(realization(1_000)),
        };
        let checkpoint = checkpoint(&operation);
        let prepared = source_release_prepared(&operation, &realization(1_000));
        let suspending = |suspend_phase, checkpoint, source_release_preparation| {
            SessionEnvironmentState::Suspending {
                operation: operation.clone(),
                source_effect_id: Box::new("create".into()),
                source_binding: "source".into(),
                generation: generation(),
                suspend_phase,
                checkpoint,
                source_release_preparation,
            }
        };
        let cases = [
            (resident(), Some("source"), "P1"),
            (
                suspending(SuspendPhase::Quiescing, None, None),
                Some("source"),
                "P2/quiescing",
            ),
            (
                suspending(SuspendPhase::Uploading, None, None),
                Some("source"),
                "P2/uploading",
            ),
            (
                suspending(SuspendPhase::ReadyToDispose, Some(checkpoint.clone()), None),
                Some("source"),
                "P2/ready",
            ),
            (
                SessionEnvironmentState::Unmaterialized,
                None,
                "P3/unmaterialized",
            ),
            (
                SessionEnvironmentState::Hibernated {
                    checkpoint: checkpoint.clone(),
                    generation: generation(),
                },
                None,
                "P3/hibernated",
            ),
            (
                SessionEnvironmentState::Restoring {
                    operation: operation.clone(),
                    checkpoint: checkpoint.clone(),
                    generation: generation(),
                },
                None,
                "P3/restoring",
            ),
            (
                suspending(
                    SuspendPhase::Disposing,
                    Some(checkpoint),
                    Some(Box::new(prepared)),
                ),
                None,
                "P4",
            ),
        ];
        for (state, expected, rule) in cases {
            assert_eq!(
                state.terminal_repository_publication_binding(),
                expected,
                "{rule}"
            );
        }
    }

    fn local_binding(paths: &[&str]) -> String {
        local_binding_for("sandbox-1", "environment-test-spec", paths)
    }

    #[test]
    fn environment_operation_start_time_is_one_frozen_effect_fact() {
        // Cause/effect table: C1 a fresh suspend starts at T1; C2 a replay
        // supplies T2; C3 another aggregate starts with the same/different
        // admission facts; C4 a rolling-upgrade row omits the new field; C5 a
        // realization lease is present or absent.
        // Effects: R1 C1=>persist T1 and bind it into the effect id; R2 C1+C2
        // =>return the original operation unchanged; R3 equal facts=>equal id
        // and different T=>different id; R4 C4=>decode zero without rewriting
        // the persisted legacy identity; R5 C5=>project the one provider fence
        // or explicit None without reconstructing lease fields in an adapter.
        // O1/O4 read the original stored values through the same public immutable API.
        let mut first = resident();
        let operation = first
            .begin_suspend_at(
                "workspace",
                &crate::state::SessionId::from("s1"),
                7,
                None,
                100,
            )
            .unwrap()
            .clone();
        assert_eq!(operation.started_at_unix_ms(), 100, "R1");
        let current_wire = serde_json::to_value(&operation).unwrap();
        assert_eq!(current_wire["started_at_unix_ms"], 100, "R1 wire");
        assert_eq!(
            serde_json::from_value::<SessionEnvironmentOperation>(current_wire).unwrap(),
            operation,
            "R1 round trip"
        );
        assert_eq!(
            first
                .begin_suspend_at(
                    "workspace",
                    &crate::state::SessionId::from("s1"),
                    7,
                    None,
                    200
                )
                .unwrap(),
            &operation,
            "R2"
        );

        let mut equal_state = resident();
        let equal_operation = equal_state
            .begin_suspend_at(
                "workspace",
                &crate::state::SessionId::from("s1"),
                7,
                None,
                100,
            )
            .unwrap();
        let mut later_state = resident();
        let later_operation = later_state
            .begin_suspend_at(
                "workspace",
                &crate::state::SessionId::from("s1"),
                7,
                None,
                101,
            )
            .unwrap();
        assert_eq!(
            equal_operation.effect_id(),
            operation.effect_id(),
            "R3 equal facts"
        );
        assert_ne!(
            later_operation.effect_id(),
            operation.effect_id(),
            "R3 different time"
        );

        let legacy: SessionEnvironmentOperation = serde_json::from_value(serde_json::json!({
            "effect_id": "persisted-v1-effect",
            "activity_epoch": 7,
            "realization": null
        }))
        .unwrap();
        assert_eq!(legacy.started_at_unix_ms(), 0, "R4 default");
        assert_eq!(legacy.effect_id(), "persisted-v1-effect", "R4 identity");
        assert_eq!(operation.sandbox_effect_fence().unwrap(), None, "R5 legacy");

        let lease = crate::state::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "runtime-a".into(),
            epoch: 9,
            expires_at_unix_ms: 1_000,
        };
        let fenced = SessionEnvironmentOperation::new_started_at(
            "workspace",
            &crate::state::SessionId::from("s1"),
            "suspend",
            &generation(),
            7,
            100,
            Some(lease),
            None,
        );
        let fence = fenced.sandbox_effect_fence().unwrap().unwrap();
        assert_eq!(fence.operation_id(), fenced.effect_id(), "R5 operation");
        assert_eq!(fence.owner(), "worker-a", "R5 owner");
        assert_eq!(fence.runtime_incarnation(), "runtime-a", "R5 runtime");
        assert_eq!(fence.epoch(), 9, "R5 epoch");
        assert_eq!(fence.expires_at_unix_ms(), 1_000, "R5 expiry");
    }

    #[test]
    fn checkpoint_request_is_projected_only_from_uploading_truth() {
        // Cause/effect table: C1 state is Resident/Quiescing/Uploading/
        // ReadyToDispose; C2 frozen policy is valid/invalid; C3 Uploading has
        // no checkpoint/has an impossible pre-existing checkpoint. Effects: R1
        // !Uploading=>None; R2 Uploading+valid=>one request containing the exact
        // Workspace, Session, operation, generation, start time, expiry, format,
        // and byte bound; R3 Uploading+invalid policy=>error without an I/O
        // request; R4 Uploading+checkpoint=>fail closed instead of replaying an
        // upload while another path treats the object as known.
        let policy = crate::state::SandboxIdleRetentionPolicy {
            mode: crate::state::SandboxIdleRetentionMode::CheckpointAndRelease,
            checkpoint_after_secs: 1,
            retention_secs: 100,
            expiry_behavior: Default::default(),
            max_checkpoint_bytes: 4_096,
            max_checkpoint_duration_secs: 30,
            checkpoint_format: "awaken-fs-tar-v1".into(),
        };
        let mut state = resident();
        assert_eq!(
            state
                .checkpoint_request("workspace-a", &crate::state::SessionId::from("s1"), &policy)
                .unwrap(),
            None,
            "R1 Resident"
        );
        let operation = state
            .begin_suspend_at(
                "workspace-a",
                &crate::state::SessionId::from("s1"),
                7,
                None,
                123,
            )
            .unwrap()
            .clone();
        assert_eq!(
            state
                .checkpoint_request("workspace-a", &crate::state::SessionId::from("s1"), &policy)
                .unwrap(),
            None,
            "R1 Quiescing"
        );
        state
            .record_quiescence(
                &QuiescenceReceipt {
                    effect_id: operation.effect_id.clone(),
                    generation_id: generation().id().to_owned(),
                    activity_epoch: 7,
                    live_environment_effects: 0,
                    mcp_generations: Vec::new(),
                },
                &[],
            )
            .unwrap();

        let request = state
            .checkpoint_request("workspace-a", &crate::state::SessionId::from("s1"), &policy)
            .unwrap()
            .unwrap();
        assert_eq!(request.workspace_id, "workspace-a", "R2 workspace");
        assert_eq!(request.session_id, "s1", "R2 Session");
        assert_eq!(request.operation, operation, "R2 operation");
        assert_eq!(request.generation, generation(), "R2 generation");
        assert_eq!(request.created_at_unix_ms, 123, "R2 frozen start");
        assert_eq!(request.expires_at_unix_ms, 1_000, "R2 expiry");
        assert_eq!(request.format, "awaken-fs-tar-v1", "R2 format");
        assert_eq!(request.max_bytes, 4_096, "R2 byte bound");

        let invalid_policy = crate::state::SandboxIdleRetentionPolicy {
            max_checkpoint_bytes: 0,
            ..policy.clone()
        };
        assert_eq!(
            state.checkpoint_request(
                "workspace-a",
                &crate::state::SessionId::from("s1"),
                &invalid_policy
            ),
            Err(SessionEnvironmentTransitionError::InvalidCheckpointPolicy(
                "checkpoint bounds must be positive"
            )),
            "R3"
        );
        assert_eq!(
            state.checkpoint_request(
                "workspace-a",
                &crate::state::SessionId::from("s1"),
                &crate::state::SandboxIdleRetentionPolicy::default(),
            ),
            Err(SessionEnvironmentTransitionError::InvalidCheckpointPolicy(
                "checkpoint operation requires checkpoint-and-release mode"
            )),
            "R3 mode"
        );

        let mut inconsistent = state.clone();
        if let SessionEnvironmentState::Suspending {
            checkpoint: slot, ..
        } = &mut inconsistent
        {
            *slot = Some(checkpoint(&operation));
        }
        assert_eq!(
            inconsistent.checkpoint_request(
                "workspace-a",
                &crate::state::SessionId::from("s1"),
                &policy
            ),
            Err(SessionEnvironmentTransitionError::Rejected(
                SessionEnvironmentRejection::InconsistentEvidence
            )),
            "R4"
        );

        state
            .record_checkpoint(&CheckpointReceipt {
                effect_id: operation.effect_id.clone(),
                generation_id: generation().id().to_owned(),
                checkpoint: checkpoint(&operation),
            })
            .unwrap();
        assert_eq!(
            state
                .checkpoint_request("workspace-a", &crate::state::SessionId::from("s1"), &policy)
                .unwrap(),
            None,
            "R1 ReadyToDispose"
        );
    }

    #[test]
    fn ordinary_receipts_follow_one_closed_aggregate_decision_table() {
        // Cause/effect graph: C1 phase is Unmaterialized/Resident/continuation;
        // C2 kind is Create/Adopt/Rebuild/Reservation; C3 source binding and
        // generation match; C4 Environment fingerprint matches; C5 pending
        // Resource operation matches; C6 V2 paths extend one exact substrate.
        // Effects: E1 admit first create or claim-WAL adoption; E2 admit exact
        // adopt/rebuild; E3 advance only monotonic reservation evidence; E4 exact
        // response-loss replay is no-write; E5 every other combination rejects
        // atomically. Rules exercised below: R1 !Resident+Create=>E1; R2
        // !Resident+Adopt(source=self)=>E1; R3 Resident+Rebuild(exact generation)
        // =>E2; R4 legacy Resident+Rebuild(None)=>E2; R5 Resident+Reservation+
        // C4+C5+C6=>E3 then E4; R6 wrong Environment, blind Create, Adopt
        // source/result drift, generated/legacy generation drift, or
        // continuation phase=>E5; R7 reservation missing/wrong/cleared pending,
        // missing source, V1 evidence, different substrate/fingerprint, or
        // nonmonotonic paths=>E5 without changing aggregate bytes.
        let create = environment_receipt(SessionEnvironmentEffectKind::Create, "created", None);
        let mut created = SessionEnvironmentState::Unmaterialized;
        assert!(
            created.try_apply_receipt(&create, "env", None).unwrap(),
            "R1"
        );
        assert!(
            !created.try_apply_receipt(&create, "env", None).unwrap(),
            "R1 replay"
        );
        let blind = environment_receipt(SessionEnvironmentEffectKind::Create, "other", None);
        let before = created.clone();
        assert_eq!(
            created.try_apply_receipt(&blind, "env", None),
            Err(SessionEnvironmentReceiptError::Rejected(
                SessionEnvironmentRejection::InvalidTransition
            )),
            "R6 blind Create"
        );
        assert_eq!(created, before, "R6 atomic");

        let wrong_environment = environment_receipt(
            SessionEnvironmentEffectKind::Create,
            "wrong-environment",
            None,
        )
        .for_environment("other-environment");
        let mut unmaterialized = SessionEnvironmentState::Unmaterialized;
        assert_eq!(
            unmaterialized.try_apply_receipt(&wrong_environment, "env", None),
            Err(SessionEnvironmentReceiptError::EnvironmentMismatch),
            "R6 Environment drift"
        );
        assert_eq!(
            unmaterialized,
            SessionEnvironmentState::Unmaterialized,
            "R6 atomic"
        );

        let adopt =
            environment_receipt(SessionEnvironmentEffectKind::Adopt, "claim", Some("claim"));
        let mut promoted = SessionEnvironmentState::Unmaterialized;
        assert!(
            promoted.try_apply_receipt(&adopt, "env", None).unwrap(),
            "R2"
        );
        assert!(
            !promoted.try_apply_receipt(&adopt, "env", None).unwrap(),
            "R2 replay"
        );

        let invalid_adopt = environment_receipt(
            SessionEnvironmentEffectKind::Adopt,
            "claim-result",
            Some("claim-source"),
        );
        let mut unmaterialized = SessionEnvironmentState::Unmaterialized;
        assert_eq!(
            unmaterialized.try_apply_receipt(&invalid_adopt, "env", None),
            Err(SessionEnvironmentReceiptError::Rejected(
                SessionEnvironmentRejection::InvalidTransition
            )),
            "R6 Adopt result must equal its claim source"
        );
        assert_eq!(
            unmaterialized,
            SessionEnvironmentState::Unmaterialized,
            "R6 atomic"
        );

        let source_generation = generation();
        let mut generated = SessionEnvironmentState::Resident {
            binding: "source".into(),
            effect_id: Some("origin".into()),
            generation: Some(source_generation.clone()),
            idle_since_unix_ms: Some(7),
        };
        let rebuild = environment_receipt(
            SessionEnvironmentEffectKind::Rebuild {
                source_generation_id: Some(source_generation.id().to_owned()),
            },
            "replacement",
            Some("source"),
        );
        assert!(
            generated.try_apply_receipt(&rebuild, "env", None).unwrap(),
            "R3"
        );
        assert_eq!(
            generated.generation(),
            Some(&source_generation),
            "R3 generation"
        );

        let mut legacy = SessionEnvironmentState::Resident {
            binding: "legacy".into(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        };
        let legacy_rebuild = environment_receipt(
            SessionEnvironmentEffectKind::Rebuild {
                source_generation_id: None,
            },
            "legacy-replacement",
            Some("legacy"),
        );
        assert!(
            legacy
                .try_apply_receipt(&legacy_rebuild, "env", None)
                .unwrap(),
            "R4"
        );

        let old = local_binding(&["/workspace/a"]);
        let next = local_binding(&["/workspace/a", "/workspace/b"]);
        let reservation = environment_receipt(
            SessionEnvironmentEffectKind::ResourceProjectionReservation {
                transition_fingerprint: "A-to-B".into(),
            },
            &next,
            Some(&old),
        );
        let mut reserved = SessionEnvironmentState::Resident {
            binding: old,
            effect_id: Some("origin".into()),
            generation: Some(generation()),
            idle_since_unix_ms: None,
        };
        assert!(
            reserved
                .try_apply_receipt(&reservation, "env", Some("A-to-B"))
                .unwrap(),
            "R5 first"
        );
        assert!(
            !reserved
                .try_apply_receipt(&reservation, "env", Some("A-to-B"))
                .unwrap(),
            "R5 response-loss replay"
        );
        let completed = reserved.clone();
        assert_eq!(
            reserved.try_apply_receipt(&reservation, "env", None),
            Err(SessionEnvironmentReceiptError::ResourceTransitionMismatch),
            "R6 an old reservation cannot replay after pending clears"
        );
        assert_eq!(reserved, completed, "R6 atomic");

        let mut hibernated = SessionEnvironmentState::Hibernated {
            checkpoint: checkpoint(&SessionEnvironmentOperation::new(
                "workspace",
                &crate::state::SessionId::from("s1"),
                "suspend",
                &generation(),
                1,
                None,
                None,
            )),
            generation: generation(),
        };
        let hibernated_before = hibernated.clone();
        assert_eq!(
            hibernated.try_apply_receipt(&create, "env", None),
            Err(SessionEnvironmentReceiptError::Rejected(
                SessionEnvironmentRejection::ContinuationInProgress
            )),
            "R6 continuation phase"
        );
        assert_eq!(hibernated, hibernated_before, "R6 atomic");
    }

    #[test]
    fn receipt_recovery_classification_follows_concurrent_truth() {
        // Cause/effect table: a lease, lifecycle phase, or pending Resource
        // transition may move concurrently and is retried from fresh root
        // truth; malformed, cross-Environment, source-less, or nonmonotonic
        // evidence is permanently rejected. No adapter may maintain a second
        // variant list or infer this distinction from display text.
        use SessionEnvironmentReceiptError as Error;
        use SessionEnvironmentReceiptRecoveryAction as Action;

        use SessionEnvironmentRejection as Rejection;

        for error in [
            Error::RealizationStale,
            Error::Rejected(Rejection::Terminal),
            Error::Rejected(Rejection::ContinuationInProgress),
            Error::Rejected(Rejection::WrongStep),
            Error::ResourceTransitionMismatch,
            Error::MaterializationUnknown,
            Error::Rejected(Rejection::MaterializationPending),
        ] {
            assert_eq!(error.recovery_action(), Action::Retry, "{error}");
        }
        for error in [
            Error::Mismatch,
            Error::EnvironmentMismatch,
            Error::Rejected(Rejection::RequiresResident),
            Error::Rejected(Rejection::InvalidTransition),
            Error::Rejected(Rejection::RebuildSourceUnavailable),
            Error::InvalidReservation,
        ] {
            assert_eq!(error.recovery_action(), Action::Reject, "{error}");
        }
    }

    #[test]
    fn transition_error_recovery_classification_reads_the_step_refusal() {
        // Cause/effect table: TR1 a step refusal keeps the decision's own
        // classification (every rejection, both classes); TR2 evidence only
        // the data law judges (receipt mismatch, expired checkpoint, invalid
        // policy) is final for the same evidence.
        use SessionEnvironmentReceiptRecoveryAction as Action;
        use SessionEnvironmentRejection as Rejection;
        use SessionEnvironmentTransitionError as Error;

        for rejection in [
            Rejection::Terminal,
            Rejection::NotTerminal,
            Rejection::ContinuationInProgress,
            Rejection::ActivitiesOpen,
            Rejection::NotGenerated,
            Rejection::WrongStep,
            Rejection::RequiresResident,
            Rejection::InvalidTransition,
            Rejection::LeaseNotCurrent,
            Rejection::GenerationExpired,
            Rejection::CheckpointLive,
            Rejection::InconsistentEvidence,
            Rejection::MaterializationPending,
            Rejection::UnsupportedContinuationTopology,
            Rejection::MemoryEvidenceUnproven,
            Rejection::RebuildSourceUnavailable,
        ] {
            assert_eq!(
                Error::Rejected(rejection).recovery_action(),
                rejection.recovery_action(),
                "TR1 {rejection}"
            );
        }
        for error in [
            Error::ReceiptMismatch,
            Error::CheckpointExpired,
            Error::InvalidCheckpointPolicy("policy"),
        ] {
            assert_eq!(error.recovery_action(), Action::Reject, "TR2 {error}");
        }
    }

    // Cause/effect decision table: C1=Resident, C2=no live effect, C3=current
    // epoch, C4=checkpoint succeeds, C5=source-durability preparation succeeds,
    // C6=physical termination succeeds; every receipt is exact-operation,
    // generation, and source bound. R1 C1..C4 => ReadyToDispose while a disposal
    // receipt is rejected; R2 R1+C5 => durable Disposing while an exact prep
    // replay is a no-op; R3 R2+C6 => Hibernated. Thus no source deletion is an
    // admitted fact before the aggregate has durably accepted preparation.
    // The private-module extraction adds no causes or effects: this table still
    // exercises every moved transition, while the public-API gate locks their
    // unchanged inherent-method surface.
    #[test]
    fn suspend_advances_only_in_durable_effect_order() {
        let mut state = resident();
        let lease = realization(1_000);
        let operation = state
            .begin_suspend_at(
                "workspace",
                &crate::state::SessionId::from("s1"),
                7,
                Some(lease.clone()),
                100,
            )
            .unwrap()
            .clone();
        assert!(matches!(
            state,
            SessionEnvironmentState::Suspending {
                suspend_phase: SuspendPhase::Quiescing,
                ..
            }
        ));
        state
            .record_quiescence(
                &QuiescenceReceipt {
                    effect_id: operation.effect_id.clone(),
                    generation_id: generation().id().to_owned(),
                    activity_epoch: 7,
                    live_environment_effects: 0,
                    mcp_generations: Vec::new(),
                },
                &[],
            )
            .unwrap();
        let checkpoint = checkpoint(&operation);
        state
            .record_checkpoint(&CheckpointReceipt {
                effect_id: operation.effect_id.clone(),
                generation_id: generation().id().to_owned(),
                checkpoint,
            })
            .unwrap();
        let disposed = SourceDisposedReceipt {
            effect_id: operation.effect_id.clone(),
            generation_id: generation().id().to_owned(),
            source_binding: "source".into(),
            terminated: true,
        };
        assert_eq!(
            state.complete_suspend(&disposed),
            Err(SessionEnvironmentTransitionError::Rejected(
                SessionEnvironmentRejection::WrongStep
            )),
            "R1 physical disposal is not yet authorized",
        );
        let prepared = source_release_prepared(&operation, &lease);
        assert!(
            state.record_source_release_prepared(&prepared).unwrap(),
            "R2"
        );
        assert!(matches!(
            state,
            SessionEnvironmentState::Suspending {
                suspend_phase: SuspendPhase::Disposing,
                ..
            }
        ));
        assert!(
            !state.record_source_release_prepared(&prepared).unwrap(),
            "R2 replay"
        );
        state.complete_suspend(&disposed).unwrap();
        assert!(matches!(state, SessionEnvironmentState::Hibernated { .. }));
    }

    // Cause/effect design: C1=Uploading and C6=dispose requested before a
    // checkpoint reference exists. FMECA irreversible-loss control => E3 retain
    // source and reject the transition.
    #[test]
    fn source_cannot_be_disposed_before_checkpoint_reference() {
        let mut state = resident();
        let operation = state
            .begin_suspend_at(
                "workspace",
                &crate::state::SessionId::from("s1"),
                7,
                None,
                100,
            )
            .unwrap()
            .clone();
        state
            .record_quiescence(
                &QuiescenceReceipt {
                    effect_id: operation.effect_id.clone(),
                    generation_id: generation().id().to_owned(),
                    activity_epoch: 7,
                    live_environment_effects: 0,
                    mcp_generations: Vec::new(),
                },
                &[],
            )
            .unwrap();
        assert_eq!(
            state.complete_suspend(&SourceDisposedReceipt {
                effect_id: operation.effect_id,
                generation_id: generation().id().to_owned(),
                source_binding: "source".into(),
                terminated: true,
            }),
            Err(SessionEnvironmentTransitionError::Rejected(
                SessionEnvironmentRejection::WrongStep
            ))
        );
        assert_eq!(state.binding(), Some("source"));
    }

    #[test]
    fn source_release_preparation_requires_one_canonical_exact_receipt() {
        // Preparation receipt table: C1 phase is ReadyToDispose/Disposing;
        // C2 effect, generation, and source binding all match; C3 canonical
        // fingerprint matches; C4 persisted wire is an old ReadyToDispose row or
        // a new Disposing row. R1 Ready+C2+C3 => Disposing; R2 Disposing+
        // C2+C3 => idempotent no-op; R3 any !C2 or !C3 wire => fail-closed
        // decode and byte-identical aggregate state; R4 either C4 row decodes under the additive
        // current reader and the new row round-trips exactly. This receipt proves
        // only pre-delete durability; it can never complete suspension by itself.
        // R5 the same transparent Box-backed field keeps the wire exact while
        // bounding the public state enum; this prevents one optional receipt
        // from inflating every resident Session aggregate value.
        let mut state = resident();
        let lease = realization(1_000);
        let operation = state
            .begin_suspend_at(
                "workspace",
                &crate::state::SessionId::from("s1"),
                7,
                Some(lease.clone()),
                100,
            )
            .unwrap()
            .clone();
        state
            .record_quiescence(
                &QuiescenceReceipt {
                    effect_id: operation.effect_id.clone(),
                    generation_id: generation().id().to_owned(),
                    activity_epoch: 7,
                    live_environment_effects: 0,
                    mcp_generations: Vec::new(),
                },
                &[],
            )
            .unwrap();
        state
            .record_checkpoint(&CheckpointReceipt {
                effect_id: operation.effect_id.clone(),
                generation_id: generation().id().to_owned(),
                checkpoint: checkpoint(&operation),
            })
            .unwrap();
        let ready_wire = serde_json::to_value(&state).unwrap();
        assert_eq!(ready_wire["suspend_phase"], "ready_to_dispose", "R4");
        assert!(
            ready_wire.get("source_release_preparation").is_none(),
            "R4 old row has no invented preparation",
        );
        assert_eq!(
            serde_json::from_value::<SessionEnvironmentState>(ready_wire).unwrap(),
            state,
            "R4 old row",
        );

        let exact = source_release_prepared(&operation, &lease);
        let mut forged_wires = Vec::new();
        for path in [
            &["preparation", "operation", "effect_id"][..],
            &["preparation", "lease", "owner"][..],
            &["generation_id"][..],
            &["source_binding"][..],
            &["receipt_fingerprint"][..],
        ] {
            let mut wire = serde_json::to_value(&exact).unwrap();
            let mut field = &mut wire;
            for segment in path {
                field = &mut field[*segment];
            }
            let forged = format!(
                "{}-foreign",
                field.as_str().expect("forged field is a string")
            );
            *field = serde_json::Value::String(forged);
            forged_wires.push(wire);
        }
        for forged_wire in forged_wires {
            let before = state.clone();
            assert!(
                serde_json::from_value::<SourceReleasePreparedReceipt>(forged_wire).is_err(),
                "R3",
            );
            assert_eq!(state, before, "R3");
        }

        assert!(state.record_source_release_prepared(&exact).unwrap(), "R1");
        assert!(!state.record_source_release_prepared(&exact).unwrap(), "R2");
        let disposing_wire = serde_json::to_value(&state).unwrap();
        assert_eq!(disposing_wire["suspend_phase"], "disposing", "R4");
        assert_eq!(
            disposing_wire["source_release_preparation"],
            serde_json::to_value(&exact).unwrap(),
            "R4 exact preparation is durable",
        );
        assert_eq!(
            serde_json::from_value::<SessionEnvironmentState>(disposing_wire).unwrap(),
            state,
            "R4 new row",
        );
        assert!(
            std::mem::size_of::<SessionEnvironmentState>() <= 448,
            "R5 the durable preparation is indirection-backed without changing its wire",
        );
    }

    #[test]
    fn source_release_disposal_binds_exact_preparation_to_current_successor() {
        // Physical-disposal cause/effect table:
        // | Rule | aggregate phase | prepared A -> current B | Effect |
        // | F1 | Disposing+checkpoint | same lease, renewed expiry | one canonical provider authorization |
        // | F2 | Disposing+checkpoint | strictly higher epoch failover | same physical effect, current execution fence |
        // | F3 | Disposing+checkpoint | foreign same/lower epoch | typed rejection before provider I/O |
        // | F4 | ReadyToDispose/other | any | WrongPhase; preparation cannot delete |
        // The exact renewed preparation fence A and its fingerprint remain
        // immutable across F1/F2; only the aggregate-current successor changes.
        let admitted_lease = realization(1_000);
        let prepared_lease = realization(2_000);
        let mut state = resident();
        let operation = state
            .begin_suspend_at(
                "workspace",
                &crate::state::SessionId::from("s1"),
                7,
                Some(admitted_lease),
                100,
            )
            .unwrap()
            .clone();
        state
            .record_quiescence(
                &QuiescenceReceipt {
                    effect_id: operation.effect_id.clone(),
                    generation_id: generation().id().to_owned(),
                    activity_epoch: 7,
                    live_environment_effects: 0,
                    mcp_generations: Vec::new(),
                },
                &[],
            )
            .unwrap();
        state
            .record_checkpoint(&CheckpointReceipt {
                effect_id: operation.effect_id.clone(),
                generation_id: generation().id().to_owned(),
                checkpoint: checkpoint(&operation),
            })
            .unwrap();
        let ready = state.clone();
        let prepared = source_release_prepared(&operation, &prepared_lease);
        state.record_source_release_prepared(&prepared).unwrap();

        let renewed = crate::state::SessionRealizationLease {
            expires_at_unix_ms: 3_000,
            ..prepared_lease.clone()
        };
        let failover = crate::state::SessionRealizationLease {
            owner: "worker-b".into(),
            runtime_incarnation: "runtime-b".into(),
            epoch: 5,
            expires_at_unix_ms: 2_000,
        };
        let mut physical_operation_id = None;
        for (rule, current) in [("F1", &renewed), ("F2", &failover)] {
            let disposal = state.source_release_disposal(current).unwrap();
            let provider = disposal.sandbox_disposal_authorization().unwrap();
            assert_eq!(
                provider.prepared_effect_fence().operation_id(),
                operation.effect_id,
                "{rule} prepared A",
            );
            assert_eq!(
                provider.prepared_effect_fence().expires_at_unix_ms(),
                prepared_lease.expires_at_unix_ms,
                "{rule} exact renewed A",
            );
            assert_eq!(
                provider.effect_fence().owner(),
                current.owner,
                "{rule} current owner",
            );
            assert_eq!(
                provider.effect_fence().epoch(),
                current.epoch,
                "{rule} current epoch",
            );
            assert_eq!(
                physical_operation_id
                    .get_or_insert_with(|| provider.effect_fence().operation_id().to_owned()),
                provider.effect_fence().operation_id(),
                "{rule} one physical effect",
            );
            assert_eq!(
                provider.preparation_fingerprint(),
                prepared.receipt_fingerprint(),
                "{rule} canonical preparation",
            );
        }

        for rejected in [
            crate::state::SessionRealizationLease {
                owner: "worker-b".into(),
                runtime_incarnation: "runtime-b".into(),
                epoch: prepared_lease.epoch,
                expires_at_unix_ms: 2_000,
            },
            crate::state::SessionRealizationLease {
                owner: "worker-b".into(),
                runtime_incarnation: "runtime-b".into(),
                epoch: prepared_lease.epoch - 1,
                expires_at_unix_ms: 2_000,
            },
            crate::state::SessionRealizationLease {
                expires_at_unix_ms: prepared_lease.expires_at_unix_ms - 1,
                ..prepared_lease.clone()
            },
        ] {
            assert_eq!(
                state.source_release_disposal(&rejected),
                Err(SessionEnvironmentReceiptError::RealizationStale),
                "F3 {rejected:?}",
            );
        }
        assert_eq!(
            ready.source_release_disposal(&renewed),
            Err(SessionEnvironmentReceiptError::Rejected(
                SessionEnvironmentRejection::WrongStep
            )),
            "F4",
        );
    }

    // Cause/effect design: C1=Hibernated, C7=valid checkpoint, C8=driving
    // message at T1 and duplicate at T2. The first transition freezes T1;
    // duplicates join the same stable operation and valid receipt => E5 exactly
    // one Resident binding.
    #[test]
    fn restore_is_idempotent_and_bound_to_checkpoint() {
        let mut state = resident();
        let lease = realization(1_000);
        let suspend = state
            .begin_suspend_at(
                "workspace",
                &crate::state::SessionId::from("s1"),
                7,
                Some(lease.clone()),
                100,
            )
            .unwrap()
            .clone();
        state
            .record_quiescence(
                &QuiescenceReceipt {
                    effect_id: suspend.effect_id.clone(),
                    generation_id: generation().id().to_owned(),
                    activity_epoch: 7,
                    live_environment_effects: 0,
                    mcp_generations: Vec::new(),
                },
                &[],
            )
            .unwrap();
        let checkpoint = checkpoint(&suspend);
        state
            .record_checkpoint(&CheckpointReceipt {
                effect_id: suspend.effect_id.clone(),
                generation_id: generation().id().to_owned(),
                checkpoint,
            })
            .unwrap();
        state
            .record_source_release_prepared(&source_release_prepared(&suspend, &lease))
            .unwrap();
        state
            .complete_suspend(&SourceDisposedReceipt {
                effect_id: suspend.effect_id,
                generation_id: generation().id().to_owned(),
                source_binding: "source".into(),
                terminated: true,
            })
            .unwrap();
        let restore = state
            .begin_restore(
                "workspace",
                &crate::state::SessionId::from("s1"),
                8,
                None,
                100,
            )
            .unwrap()
            .clone();
        assert_eq!(restore.started_at_unix_ms, 100);
        assert_eq!(
            state
                .begin_restore(
                    "workspace",
                    &crate::state::SessionId::from("s1"),
                    8,
                    None,
                    200
                )
                .unwrap(),
            &restore
        );
        state
            .complete_restore(&RestoreReceipt {
                effect_id: restore.effect_id,
                generation_id: generation().id().to_owned(),
                checkpoint_id: "checkpoint".into(),
                binding: "restored".into(),
            })
            .unwrap();
        assert_eq!(state.binding(), Some("restored"));
    }

    // Cause/effect design: C1=Hibernated, C7=expired, C8=driving message.
    // Before application explicitly creates a fresh generation, E7 forbids a
    // restore effect and retains the durable checkpoint evidence.
    #[test]
    fn expired_checkpoint_fails_closed() {
        let operation = SessionEnvironmentOperation::new(
            "workspace",
            &crate::state::SessionId::from("s1"),
            "suspend",
            &generation(),
            1,
            None,
            None,
        );
        let mut state = SessionEnvironmentState::Hibernated {
            checkpoint: checkpoint(&operation),
            generation: generation(),
        };
        assert_eq!(
            state.begin_restore(
                "workspace",
                &crate::state::SessionId::from("s1"),
                2,
                None,
                1_000
            ),
            Err(SessionEnvironmentTransitionError::CheckpointExpired)
        );
        assert!(matches!(state, SessionEnvironmentState::Hibernated { .. }));
    }

    #[test]
    fn every_environment_effect_receipt_axis_fails_closed() {
        // Each row disables exactly one cause in the production admission
        // kernels. The external adapter may fabricate bytes, but no individual
        // identity/fence/completion axis is optional at durable settlement.
        for missing in 0..5 {
            let mut axes = [true; 5];
            axes[missing] = false;
            assert!(
                !quiescence_receipt_admitted(
                    axes[0],
                    axes[1],
                    axes[2],
                    axes[3],
                    if axes[4] { 0 } else { 1 },
                ),
                "quiescence axis {missing}"
            );
        }

        for missing in 0..4 {
            let mut axes = [true; 4];
            axes[missing] = false;
            assert!(
                !source_disposal_receipt_admitted(axes[0], axes[1], axes[2], axes[3]),
                "source-disposal axis {missing}"
            );
            assert!(
                !restore_receipt_admitted(axes[0], axes[1], axes[2], axes[3]),
                "restore axis {missing}"
            );
        }

        for missing in 0..5 {
            let mut axes = [true; 5];
            axes[missing] = false;
            assert!(
                !checkpoint_receipt_admitted(axes[0], axes[1], axes[2], axes[3], axes[4]),
                "checkpoint axis {missing}"
            );
        }

        assert!(quiescence_receipt_admitted(true, true, true, true, 0));
        assert!(checkpoint_receipt_admitted(true, true, true, true, true));
        assert!(source_disposal_receipt_admitted(true, true, true, true));
        assert!(restore_receipt_admitted(true, true, true, true));
    }

    proptest! {
        // Cause/effect design: C4 varies stale epoch/effect/generation evidence;
        // constraint: any one mismatch is sufficient. Receipt rule => E6 and the
        // durable state remains byte-for-byte unchanged.
        #[test]
        fn stale_quiescence_evidence_never_advances(
            wrong_epoch in any::<u64>(),
            wrong_live_count in 1u32..u32::MAX,
        ) {
            let mut state = resident();
            let operation = state
                .begin_suspend_at("workspace", &crate::state::SessionId::from("s1"), 7, None, 100)
                .unwrap()
                .clone();
            let before = state.clone();
            let receipt = QuiescenceReceipt {
                effect_id: operation.effect_id,
                generation_id: generation().id().to_owned(),
                activity_epoch: if wrong_epoch == 7 { 8 } else { wrong_epoch },
                live_environment_effects: wrong_live_count,
                mcp_generations: Vec::new(),
            };
            prop_assert_eq!(
                state.record_quiescence(&receipt, &[]),
                Err(SessionEnvironmentTransitionError::ReceiptMismatch)
            );
            prop_assert_eq!(state, before);
        }
    }

    #[test]
    fn environment_receipt_entrypoints_preserve_exact_coordinate_bytes() {
        // H1-H7 cause/effect table: C1 kind covers all variants and optional
        // generation/transition strings; C2 text is empty/plain/Unicode+NUL,
        // with None distinct from Some(""); C3 realization is absent, renewed,
        // empty, or maximal; C4 configuration/source builders run in either
        // order and repeat; C5 a public receipt's lease changes before an
        // explicit builder refresh. H1 new/from_intent => exact same bytes;
        // H2 C4 => same bytes independent of order and repetition; H3 C5 =>
        // retain the exact new lease, without adding validation/canonicalizing
        // raw coordinates. H4 None/empty/Unicode and zero epochs remain data,
        // not provider authorization. H5-H6 bad wire/intent rejection is owned
        // by the next case. The oracle is the existing verified-intent entry,
        // never another copy of the seven-axis fingerprint tuple.
        let kinds = [
            SessionEnvironmentEffectKind::Create,
            SessionEnvironmentEffectKind::Adopt,
            SessionEnvironmentEffectKind::Rebuild {
                source_generation_id: None,
            },
            SessionEnvironmentEffectKind::Rebuild {
                source_generation_id: Some(String::new()),
            },
            SessionEnvironmentEffectKind::Rebuild {
                source_generation_id: Some("代次/😀\0".into()),
            },
            SessionEnvironmentEffectKind::ResourceProjectionReservation {
                transition_fingerprint: String::new(),
            },
            SessionEnvironmentEffectKind::ResourceProjectionReservation {
                transition_fingerprint: "变更/😀\0".into(),
            },
        ];
        let coordinates = [
            ("", "", None, None),
            ("session", "binding", Some(""), None),
            ("session", "binding", None, Some("")),
            ("session", "binding", Some("source"), Some("environment")),
            (
                "会话/😀\0",
                "绑定/😀\0",
                Some("来源/😀\0"),
                Some("配置/😀\0"),
            ),
        ];
        let leases = [
            None,
            Some(realization(100)),
            Some(realization(200)),
            Some(crate::state::SessionRealizationLease {
                owner: String::new(),
                runtime_incarnation: String::new(),
                epoch: 0,
                expires_at_unix_ms: 0,
            }),
            Some(crate::state::SessionRealizationLease {
                owner: "所有者/😀\0".into(),
                runtime_incarnation: "启动/😀\0".into(),
                epoch: u64::MAX,
                expires_at_unix_ms: u64::MAX,
            }),
        ];
        for kind in kinds {
            for (session, binding, source, environment) in coordinates {
                for lease in &leases {
                    let intent =
                        SessionEnvironmentEffectIntent::new(session, kind.clone(), lease.clone())
                            .for_frozen_coordinates(
                                source.map(str::to_owned),
                                environment.map(str::to_owned),
                            );
                    let expected =
                        SessionEnvironmentReceipt::from_intent(&intent, binding).unwrap();
                    let bytes = serde_json::to_vec(&expected).unwrap();
                    assert_eq!(expected.session_id.as_str(), session, "H1 raw SessionId");
                    assert_eq!(expected.realization, *lease, "H1 exact lease");
                    expected.verify().unwrap();
                    let rebound =
                        SessionEnvironmentReceipt::from_intent(&intent, format!("{binding}-next"))
                            .unwrap();
                    assert_eq!(
                        rebound.effect_id, expected.effect_id,
                        "H1 binding-independent intent"
                    );
                    assert_ne!(
                        rebound.receipt_fingerprint, expected.receipt_fingerprint,
                        "H1 exact binding witness"
                    );
                    for source_first in [false, true] {
                        let mut actual = SessionEnvironmentReceipt::new(
                            session,
                            kind.clone(),
                            binding,
                            lease.clone(),
                        );
                        for step in if source_first { [1, 0] } else { [0, 1] } {
                            actual = match (step, source, environment) {
                                (0, _, Some(value)) => actual.for_environment(value),
                                (1, Some(value), _) => actual.from_binding(value),
                                _ => actual,
                            };
                        }
                        assert_eq!(serde_json::to_vec(&actual).unwrap(), bytes, "H1/H2");
                        if let Some(value) = source {
                            actual = actual.from_binding(value);
                        }
                        if let Some(value) = environment {
                            actual = actual.for_environment(value);
                        }
                        assert_eq!(serde_json::to_vec(&actual).unwrap(), bytes, "H2 repeat");
                    }
                    let mut changed = expected;
                    changed.realization = Some(realization(301));
                    let refreshed = changed.for_environment(environment.unwrap_or(""));
                    let changed_intent = SessionEnvironmentEffectIntent::new(
                        session,
                        kind.clone(),
                        Some(realization(301)),
                    )
                    .for_frozen_coordinates(
                        source.map(str::to_owned),
                        Some(environment.unwrap_or("").to_owned()),
                    );
                    let changed_expected =
                        SessionEnvironmentReceipt::from_intent(&changed_intent, binding).unwrap();
                    assert_eq!(
                        serde_json::to_vec(&refreshed).unwrap(),
                        serde_json::to_vec(&changed_expected).unwrap(),
                        "H3 exact public-field change only on explicit refresh"
                    );
                }
            }
        }
    }

    #[test]
    fn environment_receipt_bad_wire_stays_rejected_without_implicit_refresh() {
        // H5: each of the eight stored fields changes without its witnesses;
        // verify must return the original exact Mismatch and leave bytes alone.
        // Lease-expiry drift preserves the intent id but invalidates the full
        // receipt witness. H6: from_intent rejects a malformed intent rather
        // than repairing it. H7: explicit builders retain their OLD refresh
        // behavior for changed effect/hash witnesses; no read/verify/from_intent
        // gains implicit repair. These input DTOs grant no root/provider right.
        let original = SessionEnvironmentReceipt::new(
            "s1",
            SessionEnvironmentEffectKind::Rebuild {
                source_generation_id: Some("generation".into()),
            },
            "binding",
            Some(realization(100)),
        )
        .for_environment("environment")
        .from_binding("source");
        let original_bytes = serde_json::to_vec(&original).unwrap();
        let changes = [
            ("session_id", serde_json::json!("other")),
            ("effect_id", serde_json::json!("other")),
            ("kind", serde_json::json!("adopt")),
            ("source_binding", serde_json::json!("other")),
            ("environment_fingerprint", serde_json::json!("other")),
            ("binding", serde_json::json!("other")),
            (
                "realization",
                serde_json::to_value(realization(200)).unwrap(),
            ),
            ("receipt_fingerprint", serde_json::json!("other")),
        ];
        for (field, value) in changes {
            let mut wire = serde_json::to_value(&original).unwrap();
            wire[field] = value;
            let corrupt: SessionEnvironmentReceipt = serde_json::from_value(wire).unwrap();
            let before = serde_json::to_vec(&corrupt).unwrap();
            assert_eq!(
                corrupt.verify(),
                Err(SessionEnvironmentReceiptError::Mismatch),
                "H5 {field}"
            );
            assert_eq!(
                serde_json::to_vec(&corrupt).unwrap(),
                before,
                "H5 no repair"
            );
            if field == "realization" {
                assert_eq!(corrupt.effect_id, original.effect_id, "H5 same intent id");
                corrupt.effect_intent().unwrap();
            }
            if matches!(field, "effect_id" | "receipt_fingerprint") {
                assert_eq!(
                    serde_json::to_vec(&corrupt.for_environment("environment")).unwrap(),
                    original_bytes,
                    "H7 original explicit refresh only"
                );
            }
        }
        let mut wire = serde_json::to_value(original.effect_intent().unwrap()).unwrap();
        wire["effect_id"] = serde_json::json!("other");
        let corrupt: SessionEnvironmentEffectIntent = serde_json::from_value(wire).unwrap();
        let before = serde_json::to_vec(&corrupt).unwrap();
        assert_eq!(
            SessionEnvironmentReceipt::from_intent(&corrupt, "binding"),
            Err(SessionEnvironmentReceiptError::Mismatch),
            "H6"
        );
        assert_eq!(
            serde_json::to_vec(&corrupt).unwrap(),
            before,
            "H6 no repair"
        );
    }
}

#[cfg(any(test, kani))]
mod verification;
