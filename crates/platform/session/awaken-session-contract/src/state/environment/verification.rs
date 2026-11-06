#[cfg(kani)]
use super::*;

#[cfg(kani)]
/// @obligation session_environment.quiescence_receipt_requires_exact_operation_epoch_mcp_set_and_zero_live_effects
#[kani::proof]
fn quiescence_receipt_requires_exact_operation_epoch_mcp_set_and_zero_live_effects() {
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

#[cfg(test)]
mod exhaustive {
    use super::super::*;

    const EVERY_SUSPEND_PHASE: [SuspendPhase; 4] = [
        SuspendPhase::Quiescing,
        SuspendPhase::Uploading,
        SuspendPhase::ReadyToDispose,
        SuspendPhase::Disposing,
    ];

    const fn listed_suspend_phase(phase: SuspendPhase) -> usize {
        match phase {
            SuspendPhase::Quiescing => 0,
            SuspendPhase::Uploading => 1,
            SuspendPhase::ReadyToDispose => 2,
            SuspendPhase::Disposing => 3,
        }
    }

    const BOOLS: [bool; 2] = [false, true];

    #[test]
    fn every_suspend_phase_is_listed() {
        for (index, phase) in EVERY_SUSPEND_PHASE.into_iter().enumerate() {
            assert_eq!(listed_suspend_phase(phase), index);
        }
    }

    /// @exhaustive SuspendPhase(4) x bool(2) x bool(2) = 16
    /// @obligation session_environment.source_disposal_requires_a_verified_checkpoint
    #[test]
    fn source_preparation_and_disposal_require_distinct_durable_phases() {
        let mut cases = 0;
        for phase in EVERY_SUSPEND_PHASE {
            for has_checkpoint in BOOLS {
                for has_preparation in BOOLS {
                    if checkpoint_source_preparation_authorized(
                        phase,
                        has_checkpoint,
                        has_preparation,
                    ) {
                        assert!(matches!(phase, SuspendPhase::ReadyToDispose));
                        assert!(has_checkpoint);
                        assert!(!has_preparation);
                        assert!(!checkpoint_source_disposal_authorized(
                            phase,
                            has_checkpoint,
                            has_preparation,
                        ));
                    }
                    if checkpoint_source_disposal_authorized(phase, has_checkpoint, has_preparation)
                    {
                        assert!(matches!(phase, SuspendPhase::Disposing));
                        assert!(has_checkpoint);
                        assert!(has_preparation);
                        assert!(!checkpoint_source_preparation_authorized(
                            phase,
                            has_checkpoint,
                            has_preparation,
                        ));
                    }
                    cases += 1;
                }
            }
        }
        assert_eq!(cases, 16);
    }

    /// @exhaustive bool(2) x bool(2) x bool(2) x bool(2) x bool(2) = 32
    /// @obligation session_environment.checkpoint_receipt_requires_every_immutable_generation_axis
    #[test]
    fn checkpoint_receipt_requires_every_immutable_generation_axis() {
        let mut cases = 0;
        for effect_matches in BOOLS {
            for generation_matches in BOOLS {
                for checkpoint_effect_matches in BOOLS {
                    for environment_fingerprint_matches in BOOLS {
                        for base_image_fingerprint_matches in BOOLS {
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
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 32);
    }

    /// @exhaustive bool(2) x bool(2) x bool(2) x bool(2) = 16
    /// @obligation session_environment.source_disposal_requires_a_verified_checkpoint
    #[test]
    fn source_disposal_receipt_requires_exact_binding_and_termination() {
        let mut cases = 0;
        for effect_matches in BOOLS {
            for generation_matches in BOOLS {
                for source_binding_matches in BOOLS {
                    for terminated in BOOLS {
                        let admitted = source_disposal_receipt_admitted(
                            effect_matches,
                            generation_matches,
                            source_binding_matches,
                            terminated,
                        );
                        assert_eq!(
                            admitted,
                            effect_matches
                                && generation_matches
                                && source_binding_matches
                                && terminated
                        );
                        cases += 1;
                    }
                }
            }
        }
        assert_eq!(cases, 16);
    }

    /// @exhaustive bool(2) x bool(2) x bool(2) x bool(2) = 16
    /// @obligation session_environment.restore_receipt_requires_exact_checkpoint_and_nonempty_binding
    #[test]
    fn restore_receipt_requires_exact_checkpoint_and_nonempty_binding() {
        let mut cases = 0;
        for effect_matches in BOOLS {
            for generation_matches in BOOLS {
                for checkpoint_matches in BOOLS {
                    for binding_present in BOOLS {
                        let admitted = restore_receipt_admitted(
                            effect_matches,
                            generation_matches,
                            checkpoint_matches,
                            binding_present,
                        );
                        assert_eq!(
                            admitted,
                            effect_matches
                                && generation_matches
                                && checkpoint_matches
                                && binding_present
                        );
                        cases += 1;
                    }
                }
            }
        }
        assert_eq!(cases, 16);
    }
}
