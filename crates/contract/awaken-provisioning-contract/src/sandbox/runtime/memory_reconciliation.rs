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
        authority: &dyn crate::CurrentExecutionAuthority,
        expected_materializations: &[MemoryMaterializationEvidence],
        supplied_materializations: &[MemoryMaterializationEvidence],
        authorize_and_drain: impl FnOnce() -> Result<(), SandboxError>,
    ) -> Result<(), SandboxError> {
        // Replays and empty projections must cross the same live authority
        // boundary. The caller has already acquired any asynchronous mount
        // lock; no copied pre-await observation can authorize Copy retirement.
        authorize_memory_ack(effect_fence, authority, || {
            self.acknowledge_current(
                effect_fence,
                expected_materializations,
                supplied_materializations,
                authorize_and_drain,
            )
        })
    }

    fn acknowledge_current(
        &self,
        effect_fence: &SandboxEffectFence,
        expected_materializations: &[MemoryMaterializationEvidence],
        supplied_materializations: &[MemoryMaterializationEvidence],
        authorize_and_drain: impl FnOnce() -> Result<(), SandboxError>,
    ) -> Result<(), SandboxError> {
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

/// One guarded admission for both the shared kernel and the fail-closed
/// provider default. Durable expiry stays in Control coordinates. This edge
/// consumes local current proof rather than comparing unrelated wall clocks.
pub(super) fn authorize_memory_ack(
    asserted: &SandboxEffectFence,
    authority: &dyn crate::CurrentExecutionAuthority,
    apply: impl FnOnce() -> Result<(), SandboxError>,
) -> Result<(), SandboxError> {
    asserted.validate_identity()?;
    let mut apply = Some(apply);
    authority
        .with_current_effect(asserted, &mut |current, remaining| {
            if remaining.is_zero()
                || !asserted.same_effect_identity(current)
                || current.expires_at_unix_ms < asserted.expires_at_unix_ms
            {
                return Err(std::io::ErrorKind::PermissionDenied.into());
            }
            let apply = apply.take().ok_or(std::io::ErrorKind::PermissionDenied)?;
            apply().map_err(std::io::Error::other)
        })
        .map_err(|error| SandboxError::new(error.to_string()))?;
    if apply.is_some() {
        return Err(SandboxError::new(
            "Memory authority omitted the acknowledgement",
        ));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CurrentExecutionAuthority, test_support::EffectAuthority};
    use std::{io, time::Duration};

    struct FaultyAuthority(&'static str);
    impl CurrentExecutionAuthority for FaultyAuthority {
        fn with_current(&self, _: &mut dyn FnMut(Duration) -> io::Result<()>) -> io::Result<()> {
            panic!("an execution-only grant cannot authorize a Memory effect")
        }
        fn with_current_effect(
            &self,
            asserted: &SandboxEffectFence,
            effect: &mut dyn FnMut(&SandboxEffectFence, Duration) -> io::Result<()>,
        ) -> io::Result<()> {
            let mut current = asserted.clone();
            let mut remaining = Duration::from_secs(1);
            match self.0 {
                "omitted" => return Ok(()),
                "zero" => remaining = Duration::ZERO,
                "operation" => current.operation_id.push('x'),
                "generation" => current.epoch += 1,
                "expiry" => current.expires_at_unix_ms -= 1,
                "twice" | "after-effect-error" => {}
                _ => unreachable!(),
            }
            effect(&current, remaining)?;
            if self.0 == "after-effect-error" {
                return Err(io::Error::other("lost response"));
            }
            effect(&current, remaining)
        }
    }

    #[test]
    fn ack_rejects_invalid_grants_and_retains_committed_state_on_response_loss() {
        // Cause/effect table: C1 empty/new/already-acked evidence; C2 authority
        // omits callback / supplies zero budget or wrong fence / invokes twice /
        // errors after committing. E1 every faulty grant returns Err, including
        // empty and cached paths; E2 invalid-before-effect has no drain/receipt;
        // E3 after-effect failures retain committed receipt for idempotent retry.
        // Authority and ack guards span the synchronous drain (no post-check gap).
        let fence = SandboxEffectFence::new("op", "owner", "boot", 1, 100).unwrap();
        let valid = EffectAuthority::new(&fence);
        let evidence = [MemoryMaterializationEvidence::new("store", "/memory", vec![]).unwrap()];
        for state in ["empty", "new", "replay"] {
            for fault in [
                "omitted",
                "zero",
                "operation",
                "generation",
                "expiry",
                "twice",
                "after-effect-error",
            ] {
                let ack = MemoryReconciliationAck::default();
                let materials = if state == "empty" {
                    &[][..]
                } else {
                    evidence.as_slice()
                };
                let mut drains = 0;
                if state == "replay" {
                    ack.acknowledge(&fence, &valid, materials, materials, || {
                        drains += 1;
                        Ok(())
                    })
                    .unwrap();
                }
                assert!(
                    ack.acknowledge(
                        &fence,
                        &FaultyAuthority(fault),
                        materials,
                        materials,
                        || {
                            drains += 1;
                            Ok(())
                        }
                    )
                    .is_err(),
                    "{state}/{fault}"
                );
                let committed = state == "replay"
                    || (state == "new" && matches!(fault, "twice" | "after-effect-error"));
                assert_eq!(
                    ack.applied.lock().unwrap().is_some(),
                    committed,
                    "{state}/{fault}"
                );
                assert_eq!(drains, usize::from(committed), "{state}/{fault}");
                ack.acknowledge(&fence, &valid, materials, materials, || {
                    assert!(valid.current.try_lock().is_err());
                    assert!(ack.applied.try_lock().is_err());
                    drains += 1;
                    Ok(())
                })
                .unwrap();
                assert_eq!(
                    drains,
                    usize::from(state != "empty"),
                    "retry {state}/{fault}"
                );
            }
        }
    }

    #[test]
    fn concurrent_exact_acknowledgements_serialize_one_copy_drain() {
        // C1 two callers start together for one exact effect/evidence; C2 the
        // first or second caller wins the existing authority/ack mutexes.
        // Both orders => two successes but exactly one Copy drain/receipt.
        // No schedule-specific sleep or independent acknowledgement store.
        use std::sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
        };
        let fence = SandboxEffectFence::new("op", "owner", "boot", 1, 100).unwrap();
        let authority = EffectAuthority::new(&fence);
        let evidence = [MemoryMaterializationEvidence::new("store", "/memory", vec![]).unwrap()];
        let ack = MemoryReconciliationAck::default();
        let start = Barrier::new(3);
        let drains = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let call = || {
                start.wait();
                ack.acknowledge(&fence, &authority, &evidence, &evidence, || {
                    assert!(authority.current.try_lock().is_err());
                    assert!(ack.applied.try_lock().is_err());
                    drains.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .unwrap();
            };
            let first = scope.spawn(call);
            let second = scope.spawn(call);
            start.wait();
            first.join().unwrap();
            second.join().unwrap();
        });
        assert_eq!(drains.load(Ordering::SeqCst), 1);
        assert_eq!(
            ack.applied.lock().unwrap().as_ref().unwrap().effect_fence,
            fence
        );
    }

    #[test]
    fn provider_drain_failure_does_not_publish_ack_or_supersede_prior_effect() {
        // F1 first drain fails => no ack; exact retry may commit once.
        // F2 successor drain fails => predecessor ack remains byte-identical;
        // successor retry alone may rebind it. Error is not an acknowledgement.
        let first = SandboxEffectFence::new("first", "owner", "boot", 1, 100).unwrap();
        let next = SandboxEffectFence::new("next", "owner", "boot", 1, 100).unwrap();
        let evidence = [MemoryMaterializationEvidence::new("store", "/memory", vec![]).unwrap()];
        let ack = MemoryReconciliationAck::default();
        for (index, fence) in [&first, &next].into_iter().enumerate() {
            let authority = EffectAuthority::new(fence);
            assert!(
                ack.acknowledge(fence, &authority, &evidence, &evidence, || {
                    Err(SandboxError::new("provider drain failed"))
                })
                .is_err()
            );
            assert_eq!(
                ack.applied
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|ack| ack.effect_fence.clone()),
                (index == 1).then(|| first.clone())
            );
            ack.acknowledge(fence, &authority, &evidence, &evidence, || Ok(()))
                .unwrap();
            assert_eq!(
                ack.applied.lock().unwrap().as_ref().unwrap().effect_fence,
                *fence
            );
        }
    }
}
