use super::*;

/// @obligation session_environment.source_disposal_requires_a_verified_checkpoint
#[kani::proof]
fn source_preparation_and_disposal_require_distinct_durable_phases() {
    let phase = match kani::any::<u8>() % 4 {
        0 => SuspendPhase::Quiescing,
        1 => SuspendPhase::Uploading,
        2 => SuspendPhase::ReadyToDispose,
        _ => SuspendPhase::Disposing,
    };
    let has_checkpoint = kani::any::<bool>();
    let has_preparation = kani::any::<bool>();
    if checkpoint_source_preparation_authorized(phase, has_checkpoint, has_preparation) {
        assert!(matches!(phase, SuspendPhase::ReadyToDispose));
        assert!(has_checkpoint);
        assert!(!has_preparation);
        assert!(!checkpoint_source_disposal_authorized(
            phase,
            has_checkpoint,
            has_preparation,
        ));
    }
    if checkpoint_source_disposal_authorized(phase, has_checkpoint, has_preparation) {
        assert!(matches!(phase, SuspendPhase::Disposing));
        assert!(has_checkpoint);
        assert!(has_preparation);
        assert!(!checkpoint_source_preparation_authorized(
            phase,
            has_checkpoint,
            has_preparation,
        ));
    }
}

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

/// @obligation session_environment.checkpoint_receipt_requires_every_immutable_generation_axis
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

/// @obligation session_environment.source_disposal_requires_a_verified_checkpoint
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

/// @obligation session_environment.restore_receipt_requires_exact_checkpoint_and_nonempty_binding
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
