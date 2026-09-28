// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Process-local acknowledgement of aggregate-owned Memory reconciliation.

use super::super::foundation::{MemoryMaterializationEvidence, SandboxError};
use super::SandboxEffectFence;
use crate::{SandboxEffectGuard, with_current_sandbox_effect};

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
        authorize_and_drain: impl FnOnce(&SandboxEffectGuard<'_>) -> Result<(), SandboxError>,
    ) -> Result<(), SandboxError> {
        // Replays and empty projections must cross the same live authority
        // boundary. The caller has already acquired any asynchronous mount
        // lock; no copied pre-await observation can authorize Copy retirement.
        with_current_sandbox_effect(effect_fence, authority, |admitted| {
            self.acknowledge_current(
                effect_fence,
                admitted,
                expected_materializations,
                supplied_materializations,
                authorize_and_drain,
            )
        })
    }

    fn acknowledge_current(
        &self,
        effect_fence: &SandboxEffectFence,
        admitted: &SandboxEffectGuard<'_>,
        expected_materializations: &[MemoryMaterializationEvidence],
        supplied_materializations: &[MemoryMaterializationEvidence],
        authorize_and_drain: impl FnOnce(&SandboxEffectGuard<'_>) -> Result<(), SandboxError>,
    ) -> Result<(), SandboxError> {
        MemoryMaterializationEvidence::validate_all(expected_materializations)?;
        MemoryMaterializationEvidence::validate_all(supplied_materializations)?;
        if expected_materializations != supplied_materializations {
            return Err(SandboxError::conflict(
                "Memory reconciliation evidence differs from the complete provider projection",
            ));
        }
        // Empty evidence says only that no Copy guard exists. It must not record
        // an acknowledgement that could be mistaken for FUSE teardown.
        if expected_materializations.is_empty() {
            return Ok(());
        }

        let mut applied = self.applied.lock().map_err(|_| {
            SandboxError::backend(
                "acknowledge_current",
                "Memory reconciliation acknowledgement lock poisoned",
            )
        })?;
        if let Some(previous) = applied.as_mut() {
            if previous.materializations != expected_materializations {
                return Err(SandboxError::conflict(
                    "Memory reconciliation replay changed its complete provider evidence",
                ));
            }
            if previous.effect_fence.same_effect_identity(effect_fence) {
                return Ok(());
            }
            if !previous.effect_fence.same_realization_lease(effect_fence) {
                return Err(SandboxError::fence_rejected(
                    "Memory reconciliation acknowledgement belongs to another realization lease",
                ));
            }
            // An aggregate-authorized successor effect in the same realization
            // lease must re-run provider authorization before rebinding the
            // process-local receipt; Copy draining is idempotent.
            authorize_and_drain(admitted)?;
            previous.effect_fence = effect_fence.clone();
            return Ok(());
        }

        authorize_and_drain(admitted)?;
        *applied = Some(AppliedMemoryReconciliation {
            effect_fence: effect_fence.clone(),
            materializations: expected_materializations.to_vec(),
        });
        Ok(())
    }

    /// Require the exact process-local acknowledgement before an effectful
    /// provider may mutate the physical Sandbox. Empty evidence is admitted
    /// because it carries no Copy guard; FUSE remains an ordinary disposal
    /// participant and is deliberately not represented here. Run synchronous
    /// preparation under the SAME current authority as the evidence check.
    /// Async providers must repeat this gate after waiting; success is not an
    /// asynchronous backend commit grant.
    pub fn require_for_disposal<T>(
        &self,
        expected_materializations: &[MemoryMaterializationEvidence],
        effect_fence: &SandboxEffectFence,
        authority: &dyn crate::CurrentExecutionAuthority,
        prepare: impl FnOnce(&SandboxEffectGuard<'_>) -> Result<T, SandboxError>,
    ) -> Result<T, SandboxError> {
        with_current_sandbox_effect(effect_fence, authority, |admitted| {
            self.require_current_for_disposal(expected_materializations, effect_fence)?;
            prepare(admitted)
        })
    }

    fn require_current_for_disposal(
        &self,
        expected_materializations: &[MemoryMaterializationEvidence],
        effect_fence: &SandboxEffectFence,
    ) -> Result<(), SandboxError> {
        MemoryMaterializationEvidence::validate_all(expected_materializations)?;
        if expected_materializations.is_empty() {
            return Ok(());
        }
        let applied = self.applied.lock().map_err(|_| {
            SandboxError::backend(
                "require_current_for_disposal",
                "Memory reconciliation acknowledgement lock poisoned",
            )
        })?;
        match applied.as_ref() {
            Some(applied)
                if applied.effect_fence.same_effect_identity(effect_fence)
                    && applied.materializations == expected_materializations =>
            {
                Ok(())
            }
            _ => Err(SandboxError::not_ready(
                "copy-backed Memory must be acknowledged by this aggregate effect before Sandbox disposal",
            )),
        }
    }
}

pub(super) fn default_memory_reconciliation_ack(
    materializations: &[MemoryMaterializationEvidence],
) -> Result<(), SandboxError> {
    MemoryMaterializationEvidence::validate_all(materializations)?;
    if materializations.is_empty() {
        Ok(())
    } else {
        Err(SandboxError::unsupported(
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

    #[test]
    fn disposal_preparation_uses_current_authority_for_empty_missing_and_cached_evidence() {
        // Cause/effect graph: C1 empty/missing/acknowledged/mismatched Copy
        // evidence; C2 current/revoked proof; C3 past/future Control expiry.
        // R1 current+(empty|acknowledged) => run preparation exactly once under
        // the held authority, independent of Worker UTC. R2 revoked OR missing
        // OR mismatched => zero preparation. R3 preparation failure preserves
        // the acknowledgement for exact retry. The complete 4x2x2 table is
        // executable; empty Memory is NOT permission to bypass current proof.
        for expiry in [100, u64::MAX] {
            let fence = SandboxEffectFence::new("op", "owner", "boot", 1, expiry).unwrap();
            for state in ["empty", "missing", "acknowledged", "mismatched"] {
                for live in [false, true] {
                    let ack = MemoryReconciliationAck::default();
                    let authority = EffectAuthority::new(&fence);
                    let mut expected = vec![
                        MemoryMaterializationEvidence::new("store", "/memory", vec![]).unwrap(),
                    ];
                    if state == "empty" {
                        expected.clear();
                    }
                    if matches!(state, "acknowledged" | "mismatched") {
                        ack.acknowledge(&fence, &authority, &expected, &expected, |_| Ok(()))
                            .unwrap();
                    }
                    if state == "mismatched" {
                        expected = vec![
                            MemoryMaterializationEvidence::new("other", "/memory", vec![]).unwrap(),
                        ];
                    }
                    if !live {
                        *authority.current.lock().unwrap() = None;
                    }
                    let mut calls = 0;
                    let result = ack.require_for_disposal(&expected, &fence, &authority, |_| {
                        assert!(
                            authority.current.try_lock().is_err(),
                            "R1 held authority spans preparation"
                        );
                        calls += 1;
                        Ok(17)
                    });
                    let admitted = live && matches!(state, "empty" | "acknowledged");
                    assert_eq!(result.is_ok(), admitted, "{expiry}/{state}/{live}");
                    assert_eq!(calls, usize::from(admitted));
                    if admitted {
                        assert_eq!(result.unwrap(), 17);
                        assert!(
                            ack.require_for_disposal(
                                &expected,
                                &fence,
                                &authority,
                                |_| Err::<(), _>(SandboxError::backend("disposal_preparation_uses_current_authority_for_empty_missing_and_cached_evidence", "prepare failed"))
                            )
                            .is_err()
                        );
                        ack.require_for_disposal(&expected, &fence, &authority, |_| Ok(()))
                            .unwrap();
                    }
                }
            }
        }
    }

    #[test]
    fn disposal_preparation_rejects_faulty_authority_and_preserves_post_commit_uncertainty() {
        // R4 omitted/zero/wrong operation/generation/regressed proof => no
        // preparation; R5 duplicate callback or error after callback => one
        // preparation, but Err, never an invented successful receipt. Cross
        // empty/cached evidence so cached success cannot bypass the callback.
        let fence = SandboxEffectFence::new("op", "owner", "boot", 1, 100).unwrap();
        for cached in [false, true] {
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
                let expected = if cached {
                    vec![MemoryMaterializationEvidence::new("store", "/memory", vec![]).unwrap()]
                } else {
                    vec![]
                };
                ack.acknowledge(
                    &fence,
                    &EffectAuthority::new(&fence),
                    &expected,
                    &expected,
                    |_| Ok(()),
                )
                .unwrap();
                let mut calls = 0;
                assert!(
                    ack.require_for_disposal(&expected, &fence, &FaultyAuthority(fault), |_| {
                        calls += 1;
                        Ok(())
                    })
                    .is_err()
                );
                assert_eq!(
                    calls,
                    usize::from(matches!(fault, "twice" | "after-effect-error")),
                    "{cached}/{fault}"
                );
                ack.require_for_disposal(&expected, &fence, &EffectAuthority::new(&fence), |_| {
                    Ok(())
                })
                .unwrap();
            }
        }
    }
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
                "operation" => {
                    current = current
                        .with_persisted(|w| w.operation_id.push('x'))
                        .unwrap()
                }
                "generation" => current = current.with_persisted(|w| w.epoch += 1).unwrap(),
                "expiry" => {
                    current = current
                        .with_persisted(|w| w.expires_at_unix_ms -= 1)
                        .unwrap()
                }
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
                    ack.acknowledge(&fence, &valid, materials, materials, |_| {
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
                        |_| {
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
                ack.acknowledge(&fence, &valid, materials, materials, |_| {
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
                ack.acknowledge(&fence, &authority, &evidence, &evidence, |_| {
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
                ack.acknowledge(fence, &authority, &evidence, &evidence, |_| {
                    Err(SandboxError::backend(
                        "provider_drain_failure_does_not_publish_ack_or_supersede_prior_effect",
                        "provider drain failed",
                    ))
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
            ack.acknowledge(fence, &authority, &evidence, &evidence, |_| Ok(()))
                .unwrap();
            assert_eq!(
                ack.applied.lock().unwrap().as_ref().unwrap().effect_fence,
                *fence
            );
        }
    }
}
