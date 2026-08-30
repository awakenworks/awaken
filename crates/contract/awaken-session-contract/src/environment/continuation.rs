use super::*;


impl SessionEnvironmentOperation {

    /// Project this exact operation through its aggregate-owned realization
    /// lease. `None` is explicit legacy evidence and never authorizes a
    /// provider effect; callers decide whether their lifecycle can retain it or
    /// must fail closed.
    pub fn sandbox_effect_fence(
        &self,
    ) -> Result<
        Option<awaken_provisioning_contract::SandboxEffectFence>,
        awaken_provisioning_contract::SandboxError,
    > {
        self.realization
            .as_ref()
            .map(|lease| lease.sandbox_effect_fence(self.effect_id.as_str()))
            .transpose()
    }
}

/// Closed aggregate projection for the live half of one checkpoint-source
/// release. The operation remains the canonical suspend identity; the exact
/// current lease records which renewable realization actually completed the
/// source-dependent effects.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceReleasePreparationEffect {
    operation: SessionEnvironmentOperation,
    lease: crate::SessionRealizationLease,
}

impl SourceReleasePreparationEffect {
    pub(crate) fn new(
        operation: SessionEnvironmentOperation,
        lease: crate::SessionRealizationLease,
    ) -> Result<Self, SessionEnvironmentReceiptError> {
        let Some(admitted_lease) = operation.realization.as_ref() else {
            return Err(SessionEnvironmentReceiptError::RealizationStale);
        };
        if !crate::realization_lease_generation_authorizes(&lease, admitted_lease) {
            return Err(SessionEnvironmentReceiptError::RealizationStale);
        }
        lease
            .sandbox_effect_fence(operation.effect_id.as_str())
            .map_err(|_| SessionEnvironmentReceiptError::RealizationStale)?;
        Ok(Self { operation, lease })
    }

    #[must_use]
    pub const fn operation(&self) -> &SessionEnvironmentOperation {
        &self.operation
    }

    #[must_use]
    pub const fn lease(&self) -> &crate::SessionRealizationLease {
        &self.lease
    }

    /// Lower the root-owned preparation effect once at the Runtime boundary.
    /// No adapter may rebuild this fence from a local lease cache.
    pub fn sandbox_effect_fence(
        &self,
    ) -> Result<
        awaken_provisioning_contract::SandboxEffectFence,
        awaken_provisioning_contract::SandboxError,
    > {
        self.lease
            .sandbox_effect_fence(self.operation.effect_id.as_str())
    }

    fn verify(
        &self,
        operation: &SessionEnvironmentOperation,
    ) -> Result<(), SessionEnvironmentTransitionError> {
        if self.operation != *operation
            || Self::new(self.operation.clone(), self.lease.clone()).is_err()
        {
            return Err(SessionEnvironmentTransitionError::ReceiptMismatch);
        }
        Ok(())
    }
}

/// Canonical proof that every live durability participant completed for one
/// checkpoint source before the aggregate authorizes physical deletion.
///
/// The closed effect preserves the Runtime work assertion while the separate
/// provider-prepared fence binds the exact durable physical predecessor.
/// Provider-local modes and partial participant receipts remain outside the
/// aggregate. The receipt is stored in the existing Suspending state, not a
/// second receipt store or cleanup state machine.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceReleasePreparedReceipt {
    preparation: SourceReleasePreparationEffect,
    provider_prepared_effect_fence: awaken_provisioning_contract::SandboxEffectFence,
    generation_id: String,
    source_binding: String,
    receipt_fingerprint: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceReleasePreparedReceiptWire {
    preparation: SourceReleasePreparationEffect,
    #[serde(default)]
    provider_prepared_effect_fence: Option<awaken_provisioning_contract::SandboxEffectFence>,
    generation_id: String,
    source_binding: String,
    receipt_fingerprint: String,
}

impl<'de> serde::Deserialize<'de> for SourceReleasePreparedReceipt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = SourceReleasePreparedReceiptWire::deserialize(deserializer)?;
        let asserted_effect_fence = wire
            .preparation
            .sandbox_effect_fence()
            .map_err(serde::de::Error::custom)?;
        let receipt = Self {
            preparation: wire.preparation,
            provider_prepared_effect_fence: wire
                .provider_prepared_effect_fence
                .unwrap_or(asserted_effect_fence),
            generation_id: wire.generation_id,
            source_binding: wire.source_binding,
            receipt_fingerprint: wire.receipt_fingerprint,
        };
        receipt.verify_shape().map_err(serde::de::Error::custom)?;
        Ok(receipt)
    }
}

impl SourceReleasePreparedReceipt {
    pub fn try_new(
        preparation: SourceReleasePreparationEffect,
        provider_prepared_effect_fence: awaken_provisioning_contract::SandboxEffectFence,
        generation: &SandboxGeneration,
        source_binding: &str,
    ) -> Result<Self, SessionEnvironmentReceiptError> {
        let asserted_effect_fence = preparation
            .sandbox_effect_fence()
            .map_err(|_| SessionEnvironmentReceiptError::RealizationStale)?;
        provider_prepared_effect_fence
            .validate_identity()
            .map_err(|_| SessionEnvironmentReceiptError::Mismatch)?;
        if !asserted_effect_fence.same_realization_lease(&provider_prepared_effect_fence)
            || !(asserted_effect_fence.authorizes_effect_successor(&provider_prepared_effect_fence)
                || provider_prepared_effect_fence
                    .authorizes_effect_successor(&asserted_effect_fence))
        {
            return Err(SessionEnvironmentReceiptError::Mismatch);
        }
        let receipt_fingerprint = crate::stable_fingerprint(&(
            "session-environment-source-release-prepared-v3",
            &preparation,
            &provider_prepared_effect_fence,
            generation.id.as_str(),
            source_binding,
        ));
        Ok(Self {
            preparation,
            provider_prepared_effect_fence,
            generation_id: generation.id.clone(),
            source_binding: source_binding.to_string(),
            receipt_fingerprint,
        })
    }

    #[must_use]
    pub const fn preparation(&self) -> &SourceReleasePreparationEffect {
        &self.preparation
    }

    #[must_use]
    pub const fn provider_prepared_effect_fence(
        &self,
    ) -> &awaken_provisioning_contract::SandboxEffectFence {
        &self.provider_prepared_effect_fence
    }

    #[must_use]
    pub fn generation_id(&self) -> &str {
        &self.generation_id
    }

    #[must_use]
    pub fn source_binding(&self) -> &str {
        &self.source_binding
    }

    #[must_use]
    pub fn receipt_fingerprint(&self) -> &str {
        &self.receipt_fingerprint
    }

    fn verify_shape(&self) -> Result<(), SessionEnvironmentTransitionError> {
        self.preparation.verify(&self.preparation.operation)?;
        let asserted_effect_fence = self
            .preparation
            .sandbox_effect_fence()
            .map_err(|_| SessionEnvironmentTransitionError::ReceiptMismatch)?;
        self.provider_prepared_effect_fence
            .validate_identity()
            .map_err(|_| SessionEnvironmentTransitionError::ReceiptMismatch)?;
        if !asserted_effect_fence.same_realization_lease(&self.provider_prepared_effect_fence)
            || !(asserted_effect_fence
                .authorizes_effect_successor(&self.provider_prepared_effect_fence)
                || self
                    .provider_prepared_effect_fence
                    .authorizes_effect_successor(&asserted_effect_fence))
        {
            return Err(SessionEnvironmentTransitionError::ReceiptMismatch);
        }
        let canonical_fingerprint = crate::stable_fingerprint(&(
            "session-environment-source-release-prepared-v3",
            &self.preparation,
            &self.provider_prepared_effect_fence,
            self.generation_id.as_str(),
            self.source_binding.as_str(),
        ));
        let legacy_fingerprint = crate::stable_fingerprint(&(
            "session-environment-source-release-prepared-v2",
            &self.preparation,
            self.generation_id.as_str(),
            self.source_binding.as_str(),
        ));
        if self.receipt_fingerprint != canonical_fingerprint
            && !(self.provider_prepared_effect_fence == asserted_effect_fence
                && self.receipt_fingerprint == legacy_fingerprint)
        {
            return Err(SessionEnvironmentTransitionError::ReceiptMismatch);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_release_wire_decodes_only_the_exact_legacy_predecessor_shape() {
        // Wire decision table SW1. C1 P is omitted/present; C2 P is the asserted
        // fence or a distinct renewal; C3 fingerprint is legacy v2/current v3.
        // SW1a omitted+exact+v2 decodes P=assertion for old writers; SW1b
        // present P uses v3; SW1c distinct P with v2 is rejected. The domain
        // receipt always owns one concrete P, never an optional second truth.
        let lease = crate::SessionRealizationLease {
            owner: "source-wire-owner".into(),
            runtime_incarnation: "source-wire-runtime".into(),
            epoch: 4,
            expires_at_unix_ms: 10_000,
        };
        let generation = SandboxGeneration::new(
            "source-wire",
            1,
            90_000,
            "source-wire-environment",
            "source-wire-image",
        );
        let operation = SessionEnvironmentOperation::new(
            "workspace",
            "source-wire",
            "suspend",
            &generation,
            1,
            Some(lease.clone()),
            None,
        );
        let preparation = SourceReleasePreparationEffect::new(operation, lease.clone()).unwrap();
        let asserted = preparation.sandbox_effect_fence().unwrap();
        let mut legacy = SourceReleasePreparedReceipt::try_new(
            preparation.clone(),
            asserted.clone(),
            &generation,
            "source-wire-binding",
        )
        .unwrap();
        legacy.receipt_fingerprint = crate::stable_fingerprint(&(
            "session-environment-source-release-prepared-v2",
            &preparation,
            generation.id.as_str(),
            "source-wire-binding",
        ));
        let mut legacy_wire = serde_json::to_value(&legacy).unwrap();
        legacy_wire
            .as_object_mut()
            .unwrap()
            .remove("provider_prepared_effect_fence");
        let decoded: SourceReleasePreparedReceipt = serde_json::from_value(legacy_wire).unwrap();
        assert_eq!(decoded.provider_prepared_effect_fence(), &asserted, "SW1a",);

        let renewed = crate::SessionRealizationLease {
            expires_at_unix_ms: 20_000,
            ..lease
        }
        .sandbox_effect_fence(preparation.operation.effect_id.as_str())
        .unwrap();
        let mut forged = serde_json::to_value(&legacy).unwrap();
        forged["provider_prepared_effect_fence"] = serde_json::to_value(renewed).unwrap();
        assert!(
            serde_json::from_value::<SourceReleasePreparedReceipt>(forged).is_err(),
            "SW1c",
        );
    }
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
    }}


impl SessionEnvironmentState {

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
    }}
