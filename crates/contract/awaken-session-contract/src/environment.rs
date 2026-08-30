//! Durable identity and continuation state of the execution environment bound
//! to a Session.
//!
//! This module is the sole durable lifecycle authority. Providers own bytes and
//! live handles; they may only advance this state with an exact, verified
//! receipt. Rebuildable processes remain Runtime Host concerns.

/// Stable identity and immutable compatibility facts for one live Sandbox.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SandboxGeneration {
    pub id: String,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub environment_fingerprint: String,
    pub base_image_fingerprint: String,
}

impl SandboxGeneration {
    #[must_use]
    pub fn new(
        session_id: &str,
        created_at_unix_ms: u64,
        expires_at_unix_ms: u64,
        environment_fingerprint: impl Into<String>,
        base_image_fingerprint: impl Into<String>,
    ) -> Self {
        let environment_fingerprint = environment_fingerprint.into();
        let base_image_fingerprint = base_image_fingerprint.into();
        Self {
            id: crate::stable_fingerprint(&(
                "sandbox-generation-v1",
                session_id,
                created_at_unix_ms,
                expires_at_unix_ms,
                environment_fingerprint.as_str(),
                base_image_fingerprint.as_str(),
            )),
            created_at_unix_ms,
            expires_at_unix_ms,
            environment_fingerprint,
            base_image_fingerprint,
        }
    }

    #[must_use]
    pub const fn expired_at(&self, now_unix_ms: u64) -> bool {
        now_unix_ms >= self.expires_at_unix_ms
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuspendPhase {
    Quiescing,
    Uploading,
    ReadyToDispose,
}

/// Representation-free irreversible-effect gate shared by production and the
/// bounded proof harness.
#[must_use]
pub const fn checkpoint_source_disposal_authorized(
    phase: SuspendPhase,
    has_checkpoint: bool,
) -> bool {
    matches!(phase, SuspendPhase::ReadyToDispose) && has_checkpoint
}

/// Closed admission rule for evidence that every live Environment effect has
/// quiesced for the exact suspend operation. String identity comparisons stay
/// at the typed receipt boundary; this heap-free kernel owns their conjunction.
#[must_use]
pub(crate) const fn quiescence_receipt_admitted(
    effect_matches: bool,
    generation_matches: bool,
    activity_epoch_matches: bool,
    mcp_generations_match: bool,
    live_environment_effects: u32,
) -> bool {
    effect_matches
        && generation_matches
        && activity_epoch_matches
        && mcp_generations_match
        && live_environment_effects == 0
}

/// Closed admission rule for a checkpoint created by the exact operation over
/// the exact immutable Environment and base-image generation.
#[must_use]
pub(crate) const fn checkpoint_receipt_admitted(
    effect_matches: bool,
    generation_matches: bool,
    checkpoint_effect_matches: bool,
    environment_fingerprint_matches: bool,
    base_image_fingerprint_matches: bool,
) -> bool {
    effect_matches
        && generation_matches
        && checkpoint_effect_matches
        && environment_fingerprint_matches
        && base_image_fingerprint_matches
}

/// Closed admission rule for the irreversible source-disposal receipt.
#[must_use]
pub(crate) const fn source_disposal_receipt_admitted(
    effect_matches: bool,
    generation_matches: bool,
    source_binding_matches: bool,
    terminated: bool,
) -> bool {
    effect_matches && generation_matches && source_binding_matches && terminated
}

/// Closed admission rule for restoring the exact live generation and
/// checkpoint to a non-empty substrate binding.
#[must_use]
pub(crate) const fn restore_receipt_admitted(
    effect_matches: bool,
    generation_matches: bool,
    checkpoint_matches: bool,
    binding_present: bool,
) -> bool {
    effect_matches && generation_matches && checkpoint_matches && binding_present
}

/// Stable effect identity. Recovery always reuses this value rather than
/// creating another checkpoint or restore authority.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionEnvironmentOperation {
    pub effect_id: String,
    pub activity_epoch: u64,
    pub realization: Option<crate::SessionRealizationLease>,
}

impl SessionEnvironmentOperation {
    #[must_use]
    pub fn new(
        workspace_id: &str,
        session_id: &str,
        kind: &str,
        generation: &SandboxGeneration,
        activity_epoch: u64,
        realization: Option<crate::SessionRealizationLease>,
        checkpoint: Option<&SandboxCheckpointRef>,
    ) -> Self {
        let generation_bytes =
            serde_json::to_vec(generation).expect("SandboxGeneration serializes");
        let activity_epoch_bytes = activity_epoch.to_be_bytes();
        let realization_bytes =
            serde_json::to_vec(&realization).expect("SessionRealizationLease serializes");
        let checkpoint_bytes = checkpoint
            .map(|value| serde_json::to_vec(value).expect("SandboxCheckpointRef serializes"));
        let mut components = vec![
            workspace_id.as_bytes(),
            session_id.as_bytes(),
            kind.as_bytes(),
            generation_bytes.as_slice(),
            activity_epoch_bytes.as_slice(),
            realization_bytes.as_slice(),
        ];
        if let Some(checkpoint_bytes) = checkpoint_bytes.as_deref() {
            components.push(checkpoint_bytes);
        }
        Self {
            effect_id: awaken_agent_contract::collision_resistant_fingerprint(
                "awaken-session-environment-operation-v2",
                &components,
            ),
            activity_epoch,
            realization,
        }
    }
}

pub use awaken_provisioning_contract::{SandboxCheckpointRef, SandboxRestoreRequest};

/// Exact bounds and aggregate identity for one idempotent checkpoint effect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxCheckpointRequest {
    pub workspace_id: String,
    pub session_id: String,
    pub operation: SessionEnvironmentOperation,
    /// Create/adopt/restore effect that owns the physical source. The suspend
    /// operation is a cleanup cause and must never replace this identity.
    pub source_effect_id: String,
    pub source_binding: String,
    pub generation: SandboxGeneration,
    pub format: String,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub max_bytes: u64,
}

/// Typed selector for a global count over the canonical Session environment
/// state. This is a read projection only; [`SessionEnvironmentState`] remains
/// the sole durable lifecycle authority.
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
        /// Exact physical owner selected before the suspend operation began.
        /// This is intentionally distinct from `operation.effect_id`.
        #[serde(default)]
        source_effect_id: Box<String>,
        source_binding: String,
        generation: SandboxGeneration,
        suspend_phase: SuspendPhase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkpoint: Option<SandboxCheckpointRef>,
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
    #[must_use]
    pub fn binding(&self) -> Option<&str> {
        match self {
            Self::Resident { binding, .. } => Some(binding),
            Self::Suspending { source_binding, .. } => Some(source_binding),
            Self::Unmaterialized | Self::Hibernated { .. } | Self::Restoring { .. } => None,
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

    /// Project the sole durable `Restoring` tuple into the provider-neutral
    /// request used by restore and terminal orphan cleanup.
    #[must_use]
    pub fn restoring_request(
        &self,
        workspace_id: &str,
        session_id: &str,
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
            session_id: session_id.to_owned(),
            effect_id: operation.effect_id.clone(),
            generation_id: generation.id.clone(),
            checkpoint: checkpoint.clone(),
        })
    }

    pub fn set_resident(&mut self, binding: impl Into<String>) {
        *self = Self::Resident {
            binding: binding.into(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        };
    }

    /// Apply the only typed create/adopt transition for a physical binding.
    /// Exact Resident replay is a no-write result. Continuation phases, a
    /// second create, and an adoption of a different source all fail closed.
    pub fn apply_binding_receipt(
        &mut self,
        receipt: &SessionEnvironmentReceipt,
    ) -> Result<bool, SessionEnvironmentTransitionError> {
        if let Self::Resident {
            binding,
            effect_id: Some(effect_id),
            generation: Some(_),
            ..
        } = self
            && binding == &receipt.binding
            && effect_id == &receipt.effect_id
        {
            return Ok(false);
        }
        match (&mut *self, receipt.kind) {
            (
                Self::Unmaterialized,
                SessionEnvironmentEffectKind::Create | SessionEnvironmentEffectKind::Adopt,
            ) => {
                *self = Self::Resident {
                    binding: receipt.binding.clone(),
                    effect_id: Some(receipt.effect_id.clone()),
                    generation: None,
                    idle_since_unix_ms: None,
                };
                Ok(true)
            }
            (
                Self::Resident {
                    binding,
                    effect_id,
                    idle_since_unix_ms: _,
                    ..
                },
                SessionEnvironmentEffectKind::Adopt,
            ) if binding == &receipt.binding => {
                *effect_id = Some(receipt.effect_id.clone());
                Ok(true)
            }
            (Self::Resident { .. }, SessionEnvironmentEffectKind::Create) => {
                Err(SessionEnvironmentTransitionError::CreateAfterBinding)
            }
            (Self::Resident { .. }, SessionEnvironmentEffectKind::Adopt) => {
                Err(SessionEnvironmentTransitionError::AdoptSourceMismatch)
            }
            (Self::Suspending { .. } | Self::Hibernated { .. } | Self::Restoring { .. }, _) => {
                Err(SessionEnvironmentTransitionError::BindingDuringContinuation)
            }
        }
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

    pub fn begin_suspend(
        &mut self,
        workspace_id: &str,
        session_id: &str,
        activity_epoch: u64,
        realization: Option<crate::SessionRealizationLease>,
    ) -> Result<&SessionEnvironmentOperation, SessionEnvironmentTransitionError> {
        if let Self::Suspending { operation, .. } = self {
            return Ok(operation);
        }
        let Self::Resident {
            binding,
            effect_id: Some(source_effect_id),
            generation: Some(generation),
            ..
        } = self
        else {
            return Err(SessionEnvironmentTransitionError::NotResident);
        };
        let operation = SessionEnvironmentOperation::new(
            workspace_id,
            session_id,
            "suspend",
            generation,
            activity_epoch,
            realization,
            None,
        );
        *self = Self::Suspending {
            operation,
            source_effect_id: Box::new(source_effect_id.clone()),
            source_binding: binding.clone(),
            generation: generation.clone(),
            suspend_phase: SuspendPhase::Quiescing,
            checkpoint: None,
        };
        match self {
            Self::Suspending { operation, .. } => Ok(operation),
            _ => unreachable!(),
        }
    }

    pub fn record_quiescence(
        &mut self,
        receipt: &QuiescenceReceipt,
        expected_mcp_generations: &[crate::McpGenerationRef],
    ) -> Result<bool, SessionEnvironmentTransitionError> {
        let Self::Suspending {
            operation,
            generation,
            suspend_phase,
            ..
        } = self
        else {
            return Err(SessionEnvironmentTransitionError::NotSuspending);
        };
        receipt.verify(operation, generation, expected_mcp_generations)?;
        if *suspend_phase != SuspendPhase::Quiescing {
            return Ok(false);
        }
        *suspend_phase = SuspendPhase::Uploading;
        Ok(true)
    }

    pub fn record_checkpoint(
        &mut self,
        receipt: &CheckpointReceipt,
    ) -> Result<bool, SessionEnvironmentTransitionError> {
        let Self::Suspending {
            operation,
            generation,
            suspend_phase,
            checkpoint,
            ..
        } = self
        else {
            return Err(SessionEnvironmentTransitionError::NotSuspending);
        };
        receipt.verify(operation, generation)?;
        if *suspend_phase == SuspendPhase::ReadyToDispose {
            return if checkpoint.as_ref() == Some(&receipt.checkpoint) {
                Ok(false)
            } else {
                Err(SessionEnvironmentTransitionError::ReceiptMismatch)
            };
        }
        if *suspend_phase != SuspendPhase::Uploading {
            return Err(SessionEnvironmentTransitionError::WrongPhase);
        }
        *checkpoint = Some(receipt.checkpoint.clone());
        *suspend_phase = SuspendPhase::ReadyToDispose;
        Ok(true)
    }

    pub fn complete_suspend(
        &mut self,
        receipt: &SourceDisposedReceipt,
    ) -> Result<bool, SessionEnvironmentTransitionError> {
        let (phase, has_checkpoint) = match self {
            Self::Suspending {
                suspend_phase,
                checkpoint,
                ..
            } => (*suspend_phase, checkpoint.is_some()),
            Self::Hibernated { .. } => return Ok(false),
            _ => return Err(SessionEnvironmentTransitionError::WrongPhase),
        };
        if !checkpoint_source_disposal_authorized(phase, has_checkpoint) {
            return Err(SessionEnvironmentTransitionError::WrongPhase);
        }
        let Self::Suspending {
            operation,
            source_binding,
            generation,
            suspend_phase: SuspendPhase::ReadyToDispose,
            checkpoint: Some(checkpoint),
            ..
        } = self
        else {
            unreachable!("source-disposal gate proved exact suspend shape")
        };
        receipt.verify(operation, generation, source_binding)?;
        *self = Self::Hibernated {
            checkpoint: checkpoint.clone(),
            generation: generation.clone(),
        };
        Ok(true)
    }

    pub fn begin_restore(
        &mut self,
        workspace_id: &str,
        session_id: &str,
        activity_epoch: u64,
        realization: Option<crate::SessionRealizationLease>,
        now_unix_ms: u64,
    ) -> Result<&SessionEnvironmentOperation, SessionEnvironmentTransitionError> {
        if let Self::Restoring { operation, .. } = self {
            return Ok(operation);
        }
        let Self::Hibernated {
            checkpoint,
            generation,
        } = self
        else {
            return Err(SessionEnvironmentTransitionError::NotHibernated);
        };
        if checkpoint.expired_at(now_unix_ms) || generation.expired_at(now_unix_ms) {
            return Err(SessionEnvironmentTransitionError::CheckpointExpired);
        }
        let operation = SessionEnvironmentOperation::new(
            workspace_id,
            session_id,
            "restore",
            generation,
            activity_epoch,
            realization,
            Some(checkpoint),
        );
        *self = Self::Restoring {
            operation,
            checkpoint: checkpoint.clone(),
            generation: generation.clone(),
        };
        match self {
            Self::Restoring { operation, .. } => Ok(operation),
            _ => unreachable!(),
        }
    }

    pub fn complete_restore(
        &mut self,
        receipt: &RestoreReceipt,
    ) -> Result<bool, SessionEnvironmentTransitionError> {
        let Self::Restoring {
            operation,
            checkpoint,
            generation,
        } = self
        else {
            return Err(SessionEnvironmentTransitionError::NotRestoring);
        };
        receipt.verify(operation, generation, checkpoint)?;
        *self = Self::Resident {
            binding: receipt.binding.clone(),
            effect_id: Some(operation.effect_id.clone()),
            generation: Some(generation.clone()),
            idle_since_unix_ms: None,
        };
        Ok(true)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEnvironmentEffectKind {
    Create,
    Adopt,
}

/// Secret-free evidence that one exact owner created or adopted the Session
/// environment. The durable binding is committed only after this receipt passes
/// the aggregate's realization fence.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionEnvironmentReceipt {
    pub session_id: String,
    pub effect_id: String,
    pub kind: SessionEnvironmentEffectKind,
    pub binding: String,
    pub realization: Option<crate::SessionRealizationLease>,
    pub receipt_fingerprint: String,
}

impl SessionEnvironmentReceipt {
    #[must_use]
    pub fn new(
        session_id: impl Into<String>,
        kind: SessionEnvironmentEffectKind,
        binding: impl Into<String>,
        realization: Option<crate::SessionRealizationLease>,
    ) -> Self {
        let session_id = session_id.into();
        let binding = binding.into();
        let effect_id = crate::stable_fingerprint(&(
            "session-environment-v1",
            &session_id,
            kind,
            realization.as_ref().map(|lease| {
                (
                    lease.owner.as_str(),
                    lease.runtime_incarnation.as_str(),
                    lease.epoch,
                )
            }),
        ));
        let receipt_fingerprint =
            crate::stable_fingerprint(&(&session_id, &effect_id, kind, &binding, &realization));
        Self {
            session_id,
            effect_id,
            kind,
            binding,
            realization,
            receipt_fingerprint,
        }
    }

    pub fn verify(&self) -> Result<(), SessionEnvironmentReceiptError> {
        let expected = Self::new(
            self.session_id.clone(),
            self.kind,
            self.binding.clone(),
            self.realization.clone(),
        );
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
    /// Exact durable MCP owners supplied by the Session application and proven
    /// quiescent by Runtime. Runtime must echo this requested set; it may not
    /// infer desired ownership from process-local projections.
    #[serde(default)]
    pub mcp_generations: Vec<crate::McpGenerationRef>,
}

impl QuiescenceReceipt {
    pub fn verify(
        &self,
        operation: &SessionEnvironmentOperation,
        generation: &SandboxGeneration,
        expected_mcp_generations: &[crate::McpGenerationRef],
    ) -> Result<(), SessionEnvironmentTransitionError> {
        if quiescence_receipt_admitted(
            self.effect_id == operation.effect_id,
            self.generation_id == generation.id,
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
            self.generation_id == generation.id,
            self.checkpoint.suspend_effect_id == operation.effect_id,
            self.checkpoint.environment_fingerprint == generation.environment_fingerprint,
            self.checkpoint.base_image_fingerprint == generation.base_image_fingerprint,
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
            self.generation_id == generation.id,
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
            self.generation_id == generation.id,
            self.checkpoint_id == checkpoint.id,
            !self.binding.is_empty(),
        ) {
            Ok(())
        } else {
            Err(SessionEnvironmentTransitionError::ReceiptMismatch)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionEnvironmentTransitionError {
    #[error("Session environment is not a generated resident environment")]
    NotResident,
    #[error("Session environment is not suspending")]
    NotSuspending,
    #[error("Session environment is not hibernated")]
    NotHibernated,
    #[error("Session environment is not restoring")]
    NotRestoring,
    #[error("Session environment operation is in the wrong phase")]
    WrongPhase,
    #[error("Session environment receipt does not match its exact effect")]
    ReceiptMismatch,
    #[error("Session environment checkpoint is expired")]
    CheckpointExpired,
    #[error("Session environment cannot create a second physical binding")]
    CreateAfterBinding,
    #[error("Session environment adoption does not match its resident source binding")]
    AdoptSourceMismatch,
    #[error("Session environment binding cannot change during continuation")]
    BindingDuringContinuation,
    #[error("terminal Session cannot accept a physical environment binding")]
    BindingAfterTerminal,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionEnvironmentReceiptError {
    #[error("Session environment receipt does not match its exact effect")]
    Mismatch,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn generation() -> SandboxGeneration {
        SandboxGeneration::new("s1", 10, 1_000, "env", "image")
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

    fn realization() -> crate::SessionRealizationLease {
        crate::SessionRealizationLease {
            owner: "owner-a".into(),
            runtime_incarnation: "runtime-a".into(),
            epoch: 3,
            expires_at_unix_ms: 900,
        }
    }

    fn restore_effect(
        workspace_id: &str,
        session_id: &str,
        generation: &SandboxGeneration,
        activity_epoch: u64,
        realization: Option<crate::SessionRealizationLease>,
        checkpoint: &SandboxCheckpointRef,
    ) -> String {
        SessionEnvironmentOperation::new(
            workspace_id,
            session_id,
            "restore",
            generation,
            activity_epoch,
            realization,
            Some(checkpoint),
        )
        .effect_id
    }

    // Environment-operation identity decision table. Causes are the complete
    // authoritative tuple: workspace, Session, kind, every generation field,
    // activity epoch, every realization field, and (for restore) every
    // checkpoint field. Effects: R1 exact replay reuses one canonical id;
    // R2-R8 any changed axis changes it; R9/R10 already-durable legacy restore
    // and suspend operations replay opaquely; R11 length framing prevents tuple
    // boundary aliasing. No legacy id can be newly derived by this writer.
    #[test]
    fn environment_operation_effect_is_workspace_bound_complete_and_legacy_replay_safe() {
        let generation = generation();
        let suspend = SessionEnvironmentOperation::new(
            "workspace-a",
            "s1",
            "suspend",
            &generation,
            6,
            Some(realization()),
            None,
        );
        let checkpoint = checkpoint(&suspend);
        let expected = restore_effect(
            "workspace-a",
            "s1",
            &generation,
            7,
            Some(realization()),
            &checkpoint,
        );
        assert_eq!(
            restore_effect(
                "workspace-a",
                "s1",
                &generation,
                7,
                Some(realization()),
                &checkpoint,
            ),
            expected,
            "R1"
        );
        assert!(
            awaken_agent_contract::collision_resistant_fingerprint_digest(&expected).is_some(),
            "R1 canonical writer"
        );
        assert_ne!(
            restore_effect(
                "workspace-b",
                "s1",
                &generation,
                7,
                Some(realization()),
                &checkpoint,
            ),
            expected,
            "R2 workspace"
        );
        assert_ne!(
            restore_effect(
                "workspace-a",
                "s2",
                &generation,
                7,
                Some(realization()),
                &checkpoint,
            ),
            expected,
            "R3 Session"
        );
        assert_ne!(
            SessionEnvironmentOperation::new(
                "workspace-a",
                "s1",
                "restore-other",
                &generation,
                7,
                Some(realization()),
                Some(&checkpoint),
            )
            .effect_id,
            expected,
            "R4 kind"
        );

        let mut generation_variants = Vec::new();
        for field in 0..5 {
            let mut changed = generation.clone();
            match field {
                0 => changed.id.push_str("-other"),
                1 => changed.created_at_unix_ms += 1,
                2 => changed.expires_at_unix_ms += 1,
                3 => changed.environment_fingerprint.push_str("-other"),
                _ => changed.base_image_fingerprint.push_str("-other"),
            }
            generation_variants.push(changed);
        }
        for changed in generation_variants {
            assert_ne!(
                restore_effect(
                    "workspace-a",
                    "s1",
                    &changed,
                    7,
                    Some(realization()),
                    &checkpoint,
                ),
                expected,
                "R5 complete generation"
            );
        }
        assert_ne!(
            restore_effect(
                "workspace-a",
                "s1",
                &generation,
                8,
                Some(realization()),
                &checkpoint,
            ),
            expected,
            "R6 activity epoch"
        );

        let mut realization_variants = Vec::new();
        for field in 0..4 {
            let mut changed = realization();
            match field {
                0 => changed.owner.push_str("-other"),
                1 => changed.runtime_incarnation.push_str("-other"),
                2 => changed.epoch += 1,
                _ => changed.expires_at_unix_ms += 1,
            }
            realization_variants.push(changed);
        }
        for changed in realization_variants {
            assert_ne!(
                restore_effect(
                    "workspace-a",
                    "s1",
                    &generation,
                    7,
                    Some(changed),
                    &checkpoint,
                ),
                expected,
                "R7 complete realization"
            );
        }
        assert_ne!(
            restore_effect("workspace-a", "s1", &generation, 7, None, &checkpoint),
            expected,
            "R7 realization presence"
        );

        let mut checkpoint_variants = Vec::new();
        for field in 0..10 {
            let mut changed = checkpoint.clone();
            match field {
                0 => changed.id.push_str("-other"),
                1 => changed.format.push_str("-other"),
                2 => changed.digest.push_str("-other"),
                3 => changed.size_bytes += 1,
                4 => changed.created_at_unix_ms += 1,
                5 => changed.expires_at_unix_ms += 1,
                6 => changed.environment_fingerprint.push_str("-other"),
                7 => changed.base_image_fingerprint.push_str("-other"),
                8 => changed.excluded_mounts.push("/other".into()),
                _ => changed.suspend_effect_id.push_str("-other"),
            }
            checkpoint_variants.push(changed);
        }
        for changed in checkpoint_variants {
            assert_ne!(
                restore_effect(
                    "workspace-a",
                    "s1",
                    &generation,
                    7,
                    Some(realization()),
                    &changed,
                ),
                expected,
                "R8 complete checkpoint"
            );
        }

        let legacy = SessionEnvironmentOperation {
            effect_id: "fnv1a64:0123456789abcdef".into(),
            activity_epoch: 1,
            realization: None,
        };
        let mut restoring = SessionEnvironmentState::Restoring {
            operation: legacy.clone(),
            checkpoint: checkpoint.clone(),
            generation: generation.clone(),
        };
        let restoring_before = restoring.clone();
        assert_eq!(
            restoring
                .begin_restore(
                    "different-workspace",
                    "different-session",
                    99,
                    Some(realization()),
                    u64::MAX,
                )
                .unwrap(),
            &legacy,
            "R9"
        );
        assert_eq!(restoring, restoring_before, "R9 opaque durable replay");

        let mut suspending = SessionEnvironmentState::Suspending {
            operation: legacy.clone(),
            source_effect_id: Box::new("create".into()),
            source_binding: "source".into(),
            generation: generation.clone(),
            suspend_phase: SuspendPhase::Quiescing,
            checkpoint: None,
        };
        let suspending_before = suspending.clone();
        assert_eq!(
            suspending
                .begin_suspend(
                    "different-workspace",
                    "different-session",
                    99,
                    Some(realization()),
                )
                .unwrap(),
            &legacy,
            "R10"
        );
        assert_eq!(suspending, suspending_before, "R10 opaque durable replay");

        assert_ne!(
            restore_effect("ab", "c", &generation, 7, Some(realization()), &checkpoint,),
            restore_effect("a", "bc", &generation, 7, Some(realization()), &checkpoint,),
            "R11 length framing"
        );
    }

    #[test]
    fn binding_receipts_follow_the_closed_create_adopt_decision_table() {
        // Cause/effect table: C1 Unmaterialized; C2 generated Resident; C3
        // continuation phase; C4 Create; C5 Adopt same/different binding;
        // C6 exact effect+binding+generation replay. Effects: E1 install an
        // ungenerated candidate for the aggregate-owned generation assignment;
        // E2 update only same-source Adopt effect while preserving generation;
        // E3 exact no-write replay; E4 typed denial with byte-identical state.
        // Rules: C1+(C4|C5)=>E1, C2+C5(same)=>E2, C2+C6=>E3,
        // C2+C4 or C2+C5(different) or C3=>E4.
        let create = SessionEnvironmentReceipt::new(
            "s1",
            SessionEnvironmentEffectKind::Create,
            "source",
            None,
        );
        let adopt = SessionEnvironmentReceipt::new(
            "s1",
            SessionEnvironmentEffectKind::Adopt,
            "source",
            None,
        );
        for receipt in [&create, &adopt] {
            let mut state = SessionEnvironmentState::Unmaterialized;
            assert_eq!(state.apply_binding_receipt(receipt), Ok(true), "E1");
            assert!(matches!(
                state,
                SessionEnvironmentState::Resident {
                    ref binding,
                    effect_id: Some(ref effect_id),
                    generation: None,
                    ..
                } if binding == "source" && effect_id == &receipt.effect_id
            ));
        }

        let mut exact = SessionEnvironmentState::Resident {
            binding: create.binding.clone(),
            effect_id: Some(create.effect_id.clone()),
            generation: Some(generation()),
            idle_since_unix_ms: Some(9),
        };
        let before = exact.clone();
        assert_eq!(exact.apply_binding_receipt(&create), Ok(false), "E3");
        assert_eq!(exact, before, "E3 byte-identical");
        assert_eq!(exact.apply_binding_receipt(&adopt), Ok(true), "E2");
        assert_eq!(exact.effect_id(), Some(adopt.effect_id.as_str()), "E2");
        assert_eq!(exact.generation(), Some(&generation()), "E2");
        assert_eq!(exact.idle_since_unix_ms(), Some(9), "E2");

        let mut second_create = exact.clone();
        let before = second_create.clone();
        assert_eq!(
            second_create.apply_binding_receipt(&create),
            Err(SessionEnvironmentTransitionError::CreateAfterBinding),
            "E4"
        );
        assert_eq!(second_create, before, "E4 byte-identical");
        let wrong_adopt = SessionEnvironmentReceipt::new(
            "s1",
            SessionEnvironmentEffectKind::Adopt,
            "different",
            None,
        );
        let mut wrong_source = exact;
        let before = wrong_source.clone();
        assert_eq!(
            wrong_source.apply_binding_receipt(&wrong_adopt),
            Err(SessionEnvironmentTransitionError::AdoptSourceMismatch),
            "E4"
        );
        assert_eq!(wrong_source, before, "E4 byte-identical");

        let operation = SessionEnvironmentOperation::new(
            "workspace-a",
            "s1",
            "suspend",
            &generation(),
            7,
            None,
            None,
        );
        let checkpoint = checkpoint(&operation);
        let continuation_states = [
            SessionEnvironmentState::Suspending {
                operation: operation.clone(),
                source_effect_id: Box::new(create.effect_id.clone()),
                source_binding: create.binding.clone(),
                generation: generation(),
                suspend_phase: SuspendPhase::Quiescing,
                checkpoint: None,
            },
            SessionEnvironmentState::Hibernated {
                checkpoint: checkpoint.clone(),
                generation: generation(),
            },
            SessionEnvironmentState::Restoring {
                operation,
                checkpoint,
                generation: generation(),
            },
        ];
        for mut state in continuation_states {
            let before = state.clone();
            assert_eq!(
                state.apply_binding_receipt(&adopt),
                Err(SessionEnvironmentTransitionError::BindingDuringContinuation),
                "E4"
            );
            assert_eq!(state, before, "E4 byte-identical");
        }
    }

    #[test]
    fn legacy_suspending_rows_decode_without_fabricating_a_source_effect() {
        // Cause/effect rules: C1 a source-aware durable row is Suspending; C2
        // its Rust source identity is heap-indirected; C3 its JSON contains the
        // field. Effect E1 serde keeps the wire value a plain JSON string. C4 a
        // pre-source-identity row has no `source_effect_id`; E2 decode it with
        // an explicit empty legacy marker while preserving operation,
        // generation, binding, and phase byte-for-byte. Runtime projection owns
        // the corresponding `LegacyDirect` interpretation; no generation or
        // physical effect is reconstructed here.
        let mut state = resident();
        state.begin_suspend("workspace-a", "s1", 7, None).unwrap();
        let (
            expected_operation,
            expected_source_binding,
            expected_generation,
            expected_suspend_phase,
        ) = match &state {
            SessionEnvironmentState::Suspending {
                operation,
                source_binding,
                generation,
                suspend_phase,
                ..
            } => (
                operation.clone(),
                source_binding.clone(),
                generation.clone(),
                *suspend_phase,
            ),
            _ => unreachable!("C1 state"),
        };
        let mut encoded = serde_json::to_value(state).unwrap();
        assert!(
            encoded
                .get("source_effect_id")
                .is_some_and(serde_json::Value::is_string),
            "C1-C3/E1 boxed Rust provenance must remain a JSON string"
        );
        encoded
            .as_object_mut()
            .expect("tagged Session Environment state")
            .remove("source_effect_id");
        let decoded: SessionEnvironmentState = serde_json::from_value(encoded).unwrap();
        assert!(matches!(
            &decoded,
            SessionEnvironmentState::Suspending {
                source_effect_id,
                operation,
                source_binding,
                generation,
                suspend_phase,
                checkpoint: None,
            } if source_effect_id.is_empty()
                && operation == &expected_operation
                && source_binding == &expected_source_binding
                && generation == &expected_generation
                && suspend_phase == &expected_suspend_phase
        ));
    }

    // Cause/effect design: C1=Resident, C2=no live effect, C4=current epoch,
    // C5=checkpoint succeeds, C6=source termination proven; constraint: each
    // receipt is bound to one operation+generation. R3 => E2 then E4.
    #[test]
    fn suspend_advances_only_in_durable_effect_order() {
        let mut state = resident();
        let operation = state
            .begin_suspend("workspace-a", "s1", 7, None)
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
                    generation_id: generation().id,
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
                generation_id: generation().id,
                checkpoint,
            })
            .unwrap();
        state
            .complete_suspend(&SourceDisposedReceipt {
                effect_id: operation.effect_id,
                generation_id: generation().id,
                source_binding: "source".into(),
                terminated: true,
            })
            .unwrap();
        assert!(matches!(state, SessionEnvironmentState::Hibernated { .. }));
    }

    // Cause/effect design: C1=Uploading and C6=dispose requested before a
    // checkpoint reference exists. FMECA irreversible-loss control => E3 retain
    // source and reject the transition.
    #[test]
    fn source_cannot_be_disposed_before_checkpoint_reference() {
        let mut state = resident();
        let operation = state
            .begin_suspend("workspace-a", "s1", 7, None)
            .unwrap()
            .clone();
        state
            .record_quiescence(
                &QuiescenceReceipt {
                    effect_id: operation.effect_id.clone(),
                    generation_id: generation().id,
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
                generation_id: generation().id,
                source_binding: "source".into(),
                terminated: true,
            }),
            Err(SessionEnvironmentTransitionError::WrongPhase)
        );
        assert_eq!(state.binding(), Some("source"));
    }

    // Cause/effect design: C1=Hibernated, C7=valid checkpoint, C8=driving
    // message. Duplicates join the same stable operation; valid receipt => E5
    // exactly one Resident binding.
    #[test]
    fn restore_is_idempotent_and_bound_to_checkpoint() {
        let mut state = resident();
        let suspend = state
            .begin_suspend("workspace-a", "s1", 7, None)
            .unwrap()
            .clone();
        state
            .record_quiescence(
                &QuiescenceReceipt {
                    effect_id: suspend.effect_id.clone(),
                    generation_id: generation().id,
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
                generation_id: generation().id,
                checkpoint,
            })
            .unwrap();
        state
            .complete_suspend(&SourceDisposedReceipt {
                effect_id: suspend.effect_id,
                generation_id: generation().id,
                source_binding: "source".into(),
                terminated: true,
            })
            .unwrap();
        let restore = state
            .begin_restore("workspace-a", "s1", 8, None, 100)
            .unwrap()
            .clone();
        assert_eq!(
            state
                .begin_restore("workspace-a", "s1", 8, None, 100)
                .unwrap(),
            &restore
        );
        state
            .complete_restore(&RestoreReceipt {
                effect_id: restore.effect_id,
                generation_id: generation().id,
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
            "workspace-a",
            "s1",
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
            state.begin_restore("workspace-a", "s1", 2, None, 1_000),
            Err(SessionEnvironmentTransitionError::CheckpointExpired)
        );
        assert!(matches!(state, SessionEnvironmentState::Hibernated { .. }));
    }

    #[test]
    fn every_environment_effect_receipt_axis_fails_closed() {
        // Each row disables exactly one cause in the production admission
        // kernels. The external adapter may fabricate bytes, but no individual
        // identity/fence/completion axis is optional at durable settlement.
        // Quiescence and checkpoint each have five axes; source disposal and
        // restore each have four. Keeping the arities separate prevents a
        // fifth unrelated cause from becoming a false mutation for a 4-axis
        // kernel.
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

    #[test]
    fn quiescence_receipt_requires_the_exact_requested_mcp_generation_set() {
        let expected = vec![crate::McpGenerationRef {
            session_id: "s1".into(),
            attachment_id: crate::McpAttachmentId("docs".into()),
            generation: crate::McpGeneration(3),
            runtime_incarnation: "runtime-1".into(),
            lease_epoch: 4,
            lease_expires_at_unix_ms: 5,
        }];
        let mut state = resident();
        let operation = state
            .begin_suspend("workspace-a", "s1", 7, None)
            .unwrap()
            .clone();
        let receipt = QuiescenceReceipt {
            effect_id: operation.effect_id,
            generation_id: generation().id,
            activity_epoch: 7,
            live_environment_effects: 0,
            mcp_generations: expected.clone(),
        };

        // Cause/effect decision table:
        // | Rule | C2 requested set | receipt proof | Effect |
        // | M1 | empty | one exact generation | reject, state unchanged |
        // | M2 | same exact generation | same | advance to Uploading |
        let before = state.clone();
        assert_eq!(
            state.record_quiescence(&receipt, &[]),
            Err(SessionEnvironmentTransitionError::ReceiptMismatch),
            "M1"
        );
        assert_eq!(state, before, "M1");
        assert!(state.record_quiescence(&receipt, &expected).unwrap(), "M2");
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
                .begin_suspend("workspace-a", "s1", 7, None)
                .unwrap()
                .clone();
            let before = state.clone();
            let receipt = QuiescenceReceipt {
                effect_id: operation.effect_id,
                generation_id: generation().id,
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
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn source_disposal_requires_ready_phase_and_checkpoint() {
        let phase = match kani::any::<u8>() % 3 {
            0 => SuspendPhase::Quiescing,
            1 => SuspendPhase::Uploading,
            _ => SuspendPhase::ReadyToDispose,
        };
        let has_checkpoint = kani::any::<bool>();
        if checkpoint_source_disposal_authorized(phase, has_checkpoint) {
            assert!(matches!(phase, SuspendPhase::ReadyToDispose));
            assert!(has_checkpoint);
        }
    }

    #[kani::proof]
    fn quiescence_receipt_requires_exact_operation_epoch_and_zero_live_effects() {
        let effect_matches = kani::any::<bool>();
        let generation_matches = kani::any::<bool>();
        let activity_epoch_matches = kani::any::<bool>();
        let mcp_generations_match = kani::any::<bool>();
        let live_environment_effects = kani::any::<u32>();
        let admitted = quiescence_receipt_admitted(
            effect_matches,
            generation_matches,
            activity_epoch_matches,
            mcp_generations_match,
            live_environment_effects,
        );
        assert_eq!(
            admitted,
            effect_matches
                && generation_matches
                && activity_epoch_matches
                && mcp_generations_match
                && live_environment_effects == 0
        );
    }

    #[kani::proof]
    fn checkpoint_receipt_requires_every_immutable_generation_axis() {
        let effect_matches = kani::any::<bool>();
        let generation_matches = kani::any::<bool>();
        let checkpoint_effect_matches = kani::any::<bool>();
        let environment_fingerprint_matches = kani::any::<bool>();
        let base_image_fingerprint_matches = kani::any::<bool>();
        let admitted = checkpoint_receipt_admitted(
            effect_matches,
            generation_matches,
            checkpoint_effect_matches,
            environment_fingerprint_matches,
            base_image_fingerprint_matches,
        );
        assert_eq!(
            admitted,
            effect_matches
                && generation_matches
                && checkpoint_effect_matches
                && environment_fingerprint_matches
                && base_image_fingerprint_matches
        );
    }

    #[kani::proof]
    fn source_disposal_receipt_requires_exact_binding_and_termination() {
        let effect_matches = kani::any::<bool>();
        let generation_matches = kani::any::<bool>();
        let source_binding_matches = kani::any::<bool>();
        let terminated = kani::any::<bool>();
        let admitted = source_disposal_receipt_admitted(
            effect_matches,
            generation_matches,
            source_binding_matches,
            terminated,
        );
        assert_eq!(
            admitted,
            effect_matches && generation_matches && source_binding_matches && terminated
        );
    }

    #[kani::proof]
    fn restore_receipt_requires_exact_checkpoint_and_nonempty_binding() {
        let effect_matches = kani::any::<bool>();
        let generation_matches = kani::any::<bool>();
        let checkpoint_matches = kani::any::<bool>();
        let binding_present = kani::any::<bool>();
        let admitted = restore_receipt_admitted(
            effect_matches,
            generation_matches,
            checkpoint_matches,
            binding_present,
        );
        assert_eq!(
            admitted,
            effect_matches && generation_matches && checkpoint_matches && binding_present
        );
    }
}
