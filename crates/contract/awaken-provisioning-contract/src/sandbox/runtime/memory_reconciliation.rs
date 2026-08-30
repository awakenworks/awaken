//! Process-local acknowledgement of aggregate-owned Memory reconciliation.

use super::super::foundation::{MemoryMaterializationEvidence, SandboxError};
use super::SandboxEffectFence;

#[derive(Debug)]
struct AppliedMemoryReconciliation {
    effect_fence: SandboxEffectFence,
    materializations: Vec<MemoryMaterializationEvidence>,
}

/// Process-local acknowledgement that one aggregate effect has completed every
/// copy-backed Memory obligation for a live Sandbox.
///
/// This is not a durable receipt or Memory authority. The provider continues to
/// own its immutable materialization evidence; the Host owns any repository CAS.
/// The acknowledgement only serializes exact-evidence admission with a provider
/// closure that retires its live Copy guards without invoking their teardown.
#[derive(Debug, Default)]
pub struct MemoryReconciliationAck {
    applied: std::sync::Mutex<Option<AppliedMemoryReconciliation>>,
}

impl MemoryReconciliationAck {
    /// Validate one complete canonical evidence set and run the provider's
    /// synchronous authorization/drain closure at most once for this effect.
    /// The provider acquires any async guard lock before entering this kernel;
    /// keeping the closure synchronous lets the contract remain runtime-neutral.
    pub fn acknowledge(
        &self,
        effect_fence: &SandboxEffectFence,
        expected_materializations: &[MemoryMaterializationEvidence],
        supplied_materializations: &[MemoryMaterializationEvidence],
        authorize_and_drain: impl FnOnce() -> Result<(), SandboxError>,
    ) -> Result<(), SandboxError> {
        validate_live_memory_reconciliation_fence(effect_fence)?;
        MemoryMaterializationEvidence::validate_all(expected_materializations)?;
        MemoryMaterializationEvidence::validate_all(supplied_materializations)?;
        if expected_materializations != supplied_materializations {
            return Err(SandboxError::new(
                "Memory reconciliation evidence differs from the complete provider projection",
            ));
        }
        // Empty evidence says only that no Copy guard exists. It must not record
        // an acknowledgement that could be mistaken for FUSE teardown.
        if expected_materializations.is_empty() {
            return Ok(());
        }

        let mut applied = self.applied.lock().map_err(|_| {
            SandboxError::new("Memory reconciliation acknowledgement lock poisoned")
        })?;
        if let Some(previous) = applied.as_mut() {
            if previous.materializations != expected_materializations {
                return Err(SandboxError::new(
                    "Memory reconciliation replay changed its complete provider evidence",
                ));
            }
            if previous.effect_fence.same_effect_identity(effect_fence) {
                return Ok(());
            }
            if !previous.effect_fence.same_realization_lease(effect_fence) {
                return Err(SandboxError::new(
                    "Memory reconciliation acknowledgement belongs to another realization lease",
                ));
            }
            // An aggregate-authorized successor effect in the same realization
            // lease must re-run provider authorization before rebinding the
            // process-local receipt; Copy draining is idempotent.
            authorize_and_drain()?;
            previous.effect_fence = effect_fence.clone();
            return Ok(());
        }

        authorize_and_drain()?;
        *applied = Some(AppliedMemoryReconciliation {
            effect_fence: effect_fence.clone(),
            materializations: expected_materializations.to_vec(),
        });
        Ok(())
    }

    /// Require the exact process-local acknowledgement before an effectful
    /// provider may mutate the physical Sandbox. Empty evidence is admitted
    /// because it carries no Copy guard; FUSE remains an ordinary disposal
    /// participant and is deliberately not represented here.
    pub fn require_for_disposal(
        &self,
        expected_materializations: &[MemoryMaterializationEvidence],
        effect_fence: &SandboxEffectFence,
    ) -> Result<(), SandboxError> {
        validate_live_memory_reconciliation_fence(effect_fence)?;
        MemoryMaterializationEvidence::validate_all(expected_materializations)?;
        if expected_materializations.is_empty() {
            return Ok(());
        }
        let applied = self.applied.lock().map_err(|_| {
            SandboxError::new("Memory reconciliation acknowledgement lock poisoned")
        })?;
        match applied.as_ref() {
            Some(applied)
                if applied.effect_fence.same_effect_identity(effect_fence)
                    && applied.materializations == expected_materializations =>
            {
                Ok(())
            }
            _ => Err(SandboxError::new(
                "copy-backed Memory must be acknowledged by this aggregate effect before Sandbox disposal",
            )),
        }
    }
}

pub(super) fn validate_live_memory_reconciliation_fence(
    effect_fence: &SandboxEffectFence,
) -> Result<(), SandboxError> {
    let now_unix_ms: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| SandboxError::new(format!("read Sandbox effect time: {error}")))?
        .as_millis()
        .try_into()
        .map_err(|_| SandboxError::new("Sandbox effect time exceeds u64"))?;
    effect_fence.validate_live_at(now_unix_ms)
}

pub(super) fn default_memory_reconciliation_ack(
    materializations: &[MemoryMaterializationEvidence],
) -> Result<(), SandboxError> {
    MemoryMaterializationEvidence::validate_all(materializations)?;
    if materializations.is_empty() {
        Ok(())
    } else {
        Err(SandboxError::new(
            "sandbox provider does not implement copy-backed Memory reconciliation acknowledgement",
        ))
    }
}
