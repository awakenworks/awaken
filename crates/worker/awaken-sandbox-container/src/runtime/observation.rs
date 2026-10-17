// @formal surface requirement=runtime.capabilities.capability_floor
// @formal reason: All three container adapters reuse this immutable-incarnation classifier. Partial
// @formal reason: identity, multiple objects, fingerprint mismatch or foreign effect fence fail closed;
// @formal reason: only exact observed absence can report unavailability. Provider observation and durable
// @formal reason: Session adoption retain separate ownership.
//! Shared immutable-incarnation and retained-participant observation rules.
//! Docker, Podman and Kubernetes call this one classifier. Its cause/effect
//! regression tables remain in the parent runtime test module beside the
//! shared realization fixtures; no test or verification path is duplicated.

use super::{
    ExistingRealization, ExistingRealizationPhase, ExistingRealizationRecovery, RuntimeError,
};
use awaken_provisioning_contract as pc;

/// Project one adapter observation into the provider-neutral adoption result.
///
/// Discovery remains adapter-owned, but absence, immutable-incarnation
/// matching, and phase semantics are shared here so Docker, Podman, and
/// Kubernetes cannot independently decide that a transport error or an
/// ambiguous physical object is safe to replace.
pub fn sandbox_observation(
    expected_incarnation: Option<&str>,
    expected_adoption: Option<&pc::SandboxRealizationFingerprint>,
    expected_fingerprint: Option<&pc::SandboxRealizationFingerprint>,
    expected_fence: Option<&pc::SandboxEffectFence>,
    observed: &[ExistingRealization],
    retained_attempt: Option<&crate::ContainerCreateAttempt>,
) -> Result<pc::SandboxObservation, RuntimeError> {
    if expected_incarnation.is_some() != expected_fingerprint.is_some()
        || expected_incarnation.is_some() != expected_adoption.is_some()
    {
        return Ok(pc::SandboxObservation::Incompatible {
            reason:
                "current Sandbox observation requires incarnation, adoption, and realization fingerprints"
                    .into(),
        });
    }
    if expected_incarnation.is_some_and(|value| value.trim().is_empty()) {
        return Ok(pc::SandboxObservation::Incompatible {
            reason: "Sandbox observation requires immutable incarnation evidence".into(),
        });
    }
    let [observed] = observed else {
        return if observed.is_empty() && expected_incarnation.is_some() {
            Ok(pc::SandboxObservation::DefinitivelyUnavailable {
                physical_incarnation: expected_incarnation.map(str::to_owned),
            })
        } else {
            Ok(pc::SandboxObservation::Incompatible {
                reason: format!(
                    "durable Sandbox observation resolved to {} physical realizations without exact current evidence",
                    observed.len()
                ),
            })
        };
    };
    if expected_incarnation.is_some()
        && let Some(expected_fence) = expected_fence
    {
        let Some(observed_fence) = observed.fence.as_ref() else {
            return Ok(pc::SandboxObservation::Incompatible {
                reason: format!(
                    "durable Sandbox incarnation `{}` has no effect-fence evidence",
                    observed.incarnation.identity
                ),
            });
        };
        if !observed_fence.authorizes_successor(expected_fence) {
            return Ok(pc::SandboxObservation::Incompatible {
                reason: format!(
                    "durable Sandbox incarnation `{}` is fenced by a newer or foreign realization lease",
                    observed.incarnation.identity
                ),
            });
        }
    }
    if let Some(expected_adoption) = expected_adoption
        && observed.adoption_fingerprint.as_deref() != Some(expected_adoption.as_str())
    {
        return Ok(pc::SandboxObservation::Incompatible {
            reason: format!(
                "durable Sandbox incarnation `{}` has missing or different adoption fingerprint",
                observed.incarnation.identity
            ),
        });
    }
    if let Some(expected_fingerprint) = expected_fingerprint
        && observed.fingerprint.as_deref() != Some(expected_fingerprint.as_str())
    {
        return Ok(pc::SandboxObservation::Incompatible {
            reason: format!(
                "durable Sandbox incarnation `{}` has missing or different realization fingerprint",
                observed.incarnation.identity
            ),
        });
    }
    if expected_incarnation.is_some_and(|expected| observed.incarnation.identity != expected) {
        return Ok(pc::SandboxObservation::Incompatible {
            reason: format!(
                "durable Sandbox incarnation `{}` resolved to foreign physical incarnation `{}`",
                expected_incarnation.unwrap_or_default(),
                observed.incarnation.identity
            ),
        });
    }
    if expected_incarnation.is_some()
        && observed.recovery == ExistingRealizationRecovery::CurrentAttemptOnly
        && retained_attempt
            .is_none_or(|attempt| observed.attempt_id.as_deref() != Some(attempt.as_str()))
        && matches!(
            observed.phase,
            ExistingRealizationPhase::Creating | ExistingRealizationPhase::Ready
        )
    {
        return Ok(pc::SandboxObservation::Incompatible {
            reason: format!(
                "durable Sandbox incarnation `{}` depends on a prior process-local realization attempt",
                observed.incarnation.identity
            ),
        });
    }
    match observed.phase {
        ExistingRealizationPhase::Creating => {
            Ok(pc::SandboxObservation::Provisioning { deadline: None })
        }
        ExistingRealizationPhase::Ready => Ok(pc::SandboxObservation::Ready),
        ExistingRealizationPhase::Terminal if expected_incarnation.is_some() => {
            Ok(pc::SandboxObservation::Terminal {
                physical_incarnation: observed.incarnation.identity.clone(),
            })
        }
        ExistingRealizationPhase::Terminal => Ok(pc::SandboxObservation::Incompatible {
            reason: "legacy Sandbox observation cannot prove terminal incarnation ownership".into(),
        }),
        ExistingRealizationPhase::Indeterminate => Err(RuntimeError::Backend(format!(
            "Sandbox observation for `{}` is in an indeterminate phase",
            expected_incarnation.unwrap_or("legacy-handle")
        ))),
    }
}
