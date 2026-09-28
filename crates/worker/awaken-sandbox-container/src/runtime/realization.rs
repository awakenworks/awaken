//! The Rebuild gate distinguishes its retained source from its own new result.
//! Fingerprints, effect monotonicity and physical identity remain checked by
//! the shared existing-realization decision before any adapter effect.
use super::{
    ExistingRealization, ExistingRealizationDecision, ExistingRealizationPhase,
    ExistingRealizationRecovery, RebuildContinuityEvidence, RuntimeError,
};
use awaken_provisioning_contract as pc;
use pc::FenceToken as _;

const fn observation_admitted(
    exact_continuation: bool,
    reconstructible: bool,
    is_source: bool,
    same_effect: bool,
    phase: ExistingRealizationPhase,
) -> bool {
    exact_continuation
        && reconstructible
        && if is_source {
            matches!(phase, ExistingRealizationPhase::Terminal)
        } else {
            same_effect
                && matches!(
                    phase,
                    ExistingRealizationPhase::Creating | ExistingRealizationPhase::Ready
                )
        }
}

/// Decide how one exact immutable realization may be recovered.
///
/// Adapters own discovery and effects only. They must pass every object selected
/// by the stable Session-scope identity; this function owns the 0/1/>1,
/// fingerprint, and phase policy so no backend grows a different retry rule.
#[cfg(any(test, feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) fn existing_realization_decision(
    context: &crate::ContainerRealizationContext<'_>,
    expected: Option<&pc::SandboxRealizationFingerprint>,
    rebuild_continuity: RebuildContinuityEvidence,
    observed: &[ExistingRealization],
) -> Result<ExistingRealizationDecision, RuntimeError> {
    let [observed] = observed else {
        return if observed.is_empty() {
            match context.intent {
                crate::ContainerRealizationIntent::Create => {
                    Ok(ExistingRealizationDecision::Create)
                }
                crate::ContainerRealizationIntent::Rebuild { .. }
                    if rebuild_continuity == RebuildContinuityEvidence::ExactContinuation =>
                {
                    Ok(ExistingRealizationDecision::Create)
                }
                crate::ContainerRealizationIntent::Rebuild { .. } => Err(RuntimeError::Backend(
                    "absent Sandbox rebuild has no exact retained continuation evidence".into(),
                )),
            }
        } else {
            Err(RuntimeError::Backend(format!(
                "stable Sandbox scope resolved to {} physical realizations",
                observed.len()
            )))
        };
    };
    let Some(expected_fence) = context.effect_fence else {
        return Err(RuntimeError::Backend(format!(
            "existing Sandbox realization `{}` cannot be recovered without a durable effect fence",
            observed.locator
        )));
    };
    let Some(observed_fence) = observed.fence.as_ref() else {
        return Err(RuntimeError::Backend(format!(
            "existing Sandbox realization `{}` has no durable effect fence",
            observed.locator
        )));
    };
    if let crate::ContainerRealizationIntent::Rebuild {
        source_incarnation, ..
    } = context.intent
        && !observation_admitted(
            rebuild_continuity == RebuildContinuityEvidence::ExactContinuation,
            observed.recovery == ExistingRealizationRecovery::Reconstructible,
            observed.incarnation.identity == *source_incarnation,
            observed_fence.same_effect_identity(expected_fence),
            observed.phase,
        )
    {
        return Err(RuntimeError::Backend(format!(
            "existing Sandbox realization `{}` is not the exact reconstructible terminal rebuild source",
            observed.locator
        )));
    }
    let current_attempt_owned = observed.recovery
        == ExistingRealizationRecovery::CurrentAttemptOnly
        && observed.attempt_id.as_deref() == Some(context.attempt.as_str());
    if observed.recovery == ExistingRealizationRecovery::CurrentAttemptOnly
        && !current_attempt_owned
    {
        return Err(RuntimeError::Backend(format!(
            "existing Sandbox realization `{}` depends on an unrecoverable prior process-local attempt",
            observed.locator
        )));
    }
    if !observed_fence.authorizes_successor(expected_fence)
        || (observed_fence.epoch == expected_fence.epoch
            && matches!(context.intent, crate::ContainerRealizationIntent::Create)
            && !observed_fence.same_effect_identity(expected_fence))
    {
        return Err(RuntimeError::Backend(format!(
            "existing Sandbox realization `{}` is owned by a newer or conflicting effect",
            observed.locator
        )));
    }
    if observed_fence.succession(expected_fence) == Some(pc::FenceSuccession::Reassignment) {
        if observed.phase == ExistingRealizationPhase::Indeterminate {
            return Err(RuntimeError::Backend(format!(
                "existing Sandbox realization `{}` cannot be replaced by this effect",
                observed.locator
            )));
        }
        if observed.incarnation.identity.trim().is_empty() {
            return Err(RuntimeError::Backend(format!(
                "existing Sandbox realization `{}` has no immutable incarnation evidence",
                observed.locator
            )));
        }
        return Ok(ExistingRealizationDecision::ReplaceExact(observed.clone()));
    }
    if observed.adoption_fingerprint.as_deref() != Some(context.adoption_fingerprint.as_str()) {
        return Err(RuntimeError::Backend(format!(
            "existing Sandbox realization `{}` has missing or different adoption evidence",
            observed.locator
        )));
    }
    let Some(expected) = expected else {
        if observed.phase == ExistingRealizationPhase::Indeterminate {
            return Err(RuntimeError::Backend(format!(
                "existing Sandbox realization `{}` is in an indeterminate phase",
                observed.locator
            )));
        }
        return Ok(ExistingRealizationDecision::ValidateExisting(
            observed.clone(),
        ));
    };
    if observed.fingerprint.as_deref() != Some(expected.as_str()) {
        return Err(RuntimeError::Backend(format!(
            "existing Sandbox realization `{}` has missing or different immutable evidence",
            observed.locator
        )));
    }
    if observed.incarnation.identity.trim().is_empty() {
        return Err(RuntimeError::Backend(format!(
            "existing Sandbox realization `{}` has no immutable incarnation evidence",
            observed.locator
        )));
    }
    let reusable =
        observed.recovery == ExistingRealizationRecovery::Reconstructible || current_attempt_owned;
    Ok(match observed.phase {
        ExistingRealizationPhase::Creating if reusable => {
            ExistingRealizationDecision::ConvergeCreating(observed.clone())
        }
        ExistingRealizationPhase::Ready if reusable => {
            ExistingRealizationDecision::ReuseReady(observed.clone())
        }
        ExistingRealizationPhase::Creating
        | ExistingRealizationPhase::Ready
        | ExistingRealizationPhase::Terminal => {
            ExistingRealizationDecision::ReplaceExact(observed.clone())
        }
        ExistingRealizationPhase::Indeterminate => {
            return Err(RuntimeError::Backend(format!(
                "existing Sandbox realization `{}` is in an indeterminate phase",
                observed.locator
            )));
        }
    })
}

#[cfg(kani)]
/// @obligation resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
/// @kani profile=solver-kissat-v1 features=k8s
#[kani::proof]
fn rebuild_activation_requires_exact_continuation_and_effect() {
    // Finite quotient of every source/result, continuity, recoverability,
    // effect identity and phase combination. This invokes the production gate,
    // not a parallel state machine. Shared tests own fingerprint/lease mapping;
    // the real K8s lane owns prepare -> activate, API UID and PVC continuity.
    let continuity: bool = kani::any();
    let recoverable: bool = kani::any();
    let source: bool = kani::any();
    let effect: bool = kani::any();
    let state: u8 = kani::any();
    kani::assume(state < 4);
    let phase = match state {
        0 => ExistingRealizationPhase::Creating,
        1 => ExistingRealizationPhase::Ready,
        2 => ExistingRealizationPhase::Terminal,
        _ => ExistingRealizationPhase::Indeterminate,
    };
    let admitted = observation_admitted(continuity, recoverable, source, effect, phase);
    assert!(!admitted || (continuity && recoverable));
    assert!(
        !admitted
            || if source {
                state == 2
            } else {
                effect && state < 2
            }
    );
    if continuity && recoverable && !source && effect && state < 2 {
        assert!(
            admitted,
            "an exact prepared Rebuild result remains activatable"
        );
    }
    if continuity && recoverable && source && state == 2 {
        assert!(admitted, "an exact terminal source remains replaceable");
    }
}
