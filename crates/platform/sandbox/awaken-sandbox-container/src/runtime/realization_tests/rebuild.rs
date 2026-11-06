// @formal surface exclude=test_only
// @formal owner crates/platform/sandbox/awaken-sandbox-container/src/runtime.rs
// @formal reason: Child of the existing cfg(test) realization_tests module. It owns the source/result
// @formal reason: decision-table regression cases and reuses the parent test fixtures; production Rebuild
// @formal reason: admission remains in runtime/realization.rs and the shared
// @formal reason: existing_realization_decision. This test module is not a production safety
// @formal reason: implementation.
use super::*;

#[test]
fn rebuilt_result_converges_without_relaxing_source_or_effect_fences() {
    /* Cause/effect table RB. C1 selected object is retained source/new result;
     * C2 exact continuation; C3 reconstructible participants; C4 same effect;
     * C5 Creating/Ready/Terminal/Indeterminate. RB1 exact C2+C3+terminal source
     * => ReplaceExact; RB2 exact C2+C3+C4+new Creating => ConvergeCreating;
     * RB3 same prerequisites+new Ready => ReuseReady; RB4 every other combination
     * => reject. Enumerate the complete 64-row quotient. This tests the shared
     * production decision used by prepare AND activate, not only the new gate.
     * RB5 individually corrupt fingerprint/adoption/incarnation/lease/expiry
     * of an otherwise valid prepared result => reject with no adapter mutation.
     * K8s distributed_control_e2e supplies the actual create -> activate ->
     * committed binding sequence and independently checks retained PVC bytes. */
    let exact = fingerprint("exact-rebuild");
    let fence = awaken_provisioning_contract::SandboxEffectFence::new(
        "rebuild", "owner", "runtime", 7, 100,
    )
    .unwrap();
    let old_fence =
        awaken_provisioning_contract::SandboxEffectFence::new("create", "owner", "runtime", 7, 100)
            .unwrap();
    let intent = crate::ContainerRealizationIntent::Rebuild {
        source_incarnation: "incarnation-1".into(),
        source_runtime_handle: None, // adapter owns the real PVC -> evidence mapping
    };
    let attempt = awaken_provisioning_contract::SandboxCreateAttempt::new("rebuild-attempt");
    for source in [false, true] {
        for continuity in [false, true] {
            for reconstructible in [false, true] {
                for same_effect in [false, true] {
                    for phase in [
                        ExistingRealizationPhase::Creating,
                        ExistingRealizationPhase::Ready,
                        ExistingRealizationPhase::Terminal,
                        ExistingRealizationPhase::Indeterminate,
                    ] {
                        let mut value = observed(Some(exact.clone()), phase);
                        if !source {
                            value.incarnation.identity = "replacement-uid".into();
                        }
                        value.fence = Some(if same_effect { &fence } else { &old_fence }.clone());
                        value.recovery = if reconstructible {
                            ExistingRealizationRecovery::Reconstructible
                        } else {
                            ExistingRealizationRecovery::CurrentAttemptOnly
                        };
                        let result = realization_decision_for_test(
                            Some(&exact),
                            Some(&exact),
                            Some(&fence),
                            &intent,
                            &attempt,
                            if continuity {
                                RebuildContinuityEvidence::ExactContinuation
                            } else {
                                RebuildContinuityEvidence::Unavailable
                            },
                            std::slice::from_ref(&value),
                        );
                        let expected =
                            match (continuity && reconstructible, source, same_effect, phase) {
                                (true, true, _, ExistingRealizationPhase::Terminal) => {
                                    Some(ExistingRealizationDecision::ReplaceExact(value))
                                }
                                (true, false, true, ExistingRealizationPhase::Creating) => {
                                    Some(ExistingRealizationDecision::ConvergeCreating(value))
                                }
                                (true, false, true, ExistingRealizationPhase::Ready) => {
                                    Some(ExistingRealizationDecision::ReuseReady(value))
                                }
                                _ => None,
                            };
                        assert_eq!(
                            result.ok(),
                            expected,
                            "RB1-4 source={source} continuity={continuity} reconstructible={reconstructible} effect={same_effect} phase={phase:?}"
                        );
                    }
                }
            }
        }
    }
    for fault in [
        "fingerprint",
        "adoption",
        "incarnation",
        "owner",
        "runtime",
        "epoch",
        "expiry",
        "missing-fence",
    ] {
        let mut value = observed(Some(exact.clone()), ExistingRealizationPhase::Creating);
        value.incarnation.identity = "replacement-uid".into();
        value.recovery = ExistingRealizationRecovery::Reconstructible;
        value.fence = Some(fence.clone());
        match fault {
            "fingerprint" => value.fingerprint = None,
            "adoption" => value.adoption_fingerprint = None,
            "incarnation" => value.incarnation.identity.clear(),
            "owner" => {
                let fence = value.fence.as_mut().unwrap();
                *fence = fence
                    .with_persisted(|w| w.owner = "foreign".into())
                    .unwrap();
            }
            "runtime" => {
                let fence = value.fence.as_mut().unwrap();
                *fence = fence
                    .with_persisted(|w| w.runtime_incarnation = "foreign".into())
                    .unwrap();
            }
            "epoch" => {
                let fence = value.fence.as_mut().unwrap();
                *fence = fence.with_persisted(|w| w.epoch += 1).unwrap();
            }
            "expiry" => {
                let fence = value.fence.as_mut().unwrap();
                *fence = fence.with_persisted(|w| w.expires_at_unix_ms += 1).unwrap();
            }
            "missing-fence" => value.fence = None,
            _ => unreachable!(),
        }
        assert!(
            realization_decision_for_test(
                Some(&exact),
                Some(&exact),
                Some(&fence),
                &intent,
                &attempt,
                RebuildContinuityEvidence::ExactContinuation,
                &[value]
            )
            .is_err(),
            "RB5 {fault}"
        );
    }
}
