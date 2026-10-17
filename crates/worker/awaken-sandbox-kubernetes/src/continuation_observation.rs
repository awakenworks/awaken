// @formal entrypoint resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
// @formal entrypoint sandbox_continuation.ephemeral_never_allocates_or_references_a_claim
//! Classification of one combined Pod/claim continuation observation before
//! the observer or the cleanup path interprets it.

use awaken_sandbox_container::{ContainerObservationPurpose, RuntimeError};

use super::{backend, continuation};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContinuationObservationDisposition {
    Live,
    RetainedOriginal,
    Disposing,
    Incompatible,
}

#[derive(Clone, Copy)]
pub(crate) struct ContinuationObservationEvidence<'a> {
    pub(crate) purpose: ContainerObservationPurpose,
    pub(crate) pod_present: bool,
    pub(crate) pod_deleting: bool,
    pub(crate) cleanup_gate_held: bool,
    pub(crate) original_terminal_guard: bool,
    pub(crate) expected_uid: Option<&'a str>,
    pub(crate) observed_uid: Option<&'a str>,
    pub(crate) observed_terminating: bool,
    pub(crate) effect_fenced: bool,
    pub(crate) effect_fenced_total_absence: bool,
}

/// Classify the one combined Pod/PVC continuation fact before either the
/// observer or cleanup path interprets it. The cleanup finalizer plus its
/// validated provenance annotation is the provider's durable physical
/// serialization gate; only the aggregate's typed authorization is evidence that
/// source-durability participants completed. A terminating exact claim is the
/// response-loss evidence after that gate was released. Neither a stable name
/// nor matching claim content is authority on its own.
pub(crate) fn continuation_observation_disposition(
    evidence: ContinuationObservationEvidence<'_>,
) -> Result<ContinuationObservationDisposition, RuntimeError> {
    let ContinuationObservationEvidence {
        purpose,
        pod_present,
        pod_deleting,
        cleanup_gate_held,
        original_terminal_guard,
        expected_uid,
        observed_uid,
        observed_terminating,
        effect_fenced,
        effect_fenced_total_absence,
    } = evidence;
    if expected_uid != observed_uid {
        if !pod_present
            && expected_uid.is_some()
            && observed_uid.is_none()
            && effect_fenced_total_absence
        {
            return Ok(ContinuationObservationDisposition::Live);
        }
        return Ok(ContinuationObservationDisposition::Incompatible);
    }

    if !pod_present && expected_uid.is_some() {
        // The same exact live claim is recovery evidence, not completed cleanup.
        // Only the existing deletion admission owns the unsafe-live-claim rule.
        if purpose == ContainerObservationPurpose::Recovery && !observed_terminating {
            return Ok(ContinuationObservationDisposition::Live);
        }
        return if observed_terminating && effect_fenced {
            Ok(ContinuationObservationDisposition::Disposing)
        } else if effect_fenced
            && continuation::absent_pod_claim_admission(true, true, true, observed_terminating)
                == continuation::AbsentPodClaimAdmission::UnsafeLiveClaim
        {
            Err(backend(continuation::ORPHANED_LIVE_CLAIM_ERROR))
        } else {
            Ok(ContinuationObservationDisposition::Incompatible)
        };
    }

    if pod_present && cleanup_gate_held {
        return if effect_fenced && expected_uid.is_some() {
            Ok(ContinuationObservationDisposition::Disposing)
        } else {
            Err(backend(
                "Kubernetes continuation cleanup gate has no exact fenced claim authority",
            ))
        };
    }

    if pod_present && pod_deleting {
        if purpose == ContainerObservationPurpose::TerminalCleanup
            && original_terminal_guard
            && !observed_terminating
            && effect_fenced
            && expected_uid.is_some()
        {
            // Retained source, not an acquired cleanup interlock or absence.
            return Ok(ContinuationObservationDisposition::RetainedOriginal);
        }
        return if observed_terminating && effect_fenced && expected_uid.is_some() {
            Ok(ContinuationObservationDisposition::Disposing)
        } else {
            Err(backend(
                "Kubernetes Sandbox Pod deletion is indeterminate before continuation cleanup",
            ))
        };
    }

    if observed_terminating {
        return Err(backend(
            "Kubernetes continuation PVC is terminating without exact cleanup evidence",
        ));
    }

    Ok(ContinuationObservationDisposition::Live)
}

#[cfg(kani)]
/// @obligation resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
/// @kani profile=solver-kissat-v1
#[kani::proof]
fn absent_pod_observation_never_confuses_recovery_with_completed_cleanup() {
    // KCO2 proof calls the production classifier for both purposes over every
    // absent/exact/foreign claim, termination, fence and total-absence bit.
    // KPV5 checks purpose routing and API effects; this finite string quotient
    // proves classification, not API linearizability or external storage bytes.
    use ContainerObservationPurpose::{Recovery, TerminalCleanup};
    use ContinuationObservationDisposition::{Disposing, Incompatible, Live};
    let claim = kani::any::<u8>();
    kani::assume(claim < 3);
    let observed_uid = match claim {
        0 => None,
        1 => Some("p"),
        _ => Some("q"),
    };
    let terminating = kani::any::<bool>();
    let fenced = kani::any::<bool>();
    let total_absence = kani::any::<bool>() && fenced;
    let evidence = ContinuationObservationEvidence {
        purpose: Recovery,
        pod_present: false,
        pod_deleting: false,
        cleanup_gate_held: false,
        original_terminal_guard: false,
        expected_uid: Some("p"),
        observed_uid,
        observed_terminating: terminating,
        effect_fenced: fenced,
        effect_fenced_total_absence: total_absence,
    };
    let recovery = continuation_observation_disposition(evidence);
    let terminal = continuation_observation_disposition(ContinuationObservationEvidence {
        purpose: TerminalCleanup,
        ..evidence
    });
    if claim == 1 && !terminating {
        assert_eq!(recovery.unwrap(), Live);
        assert!(!matches!(terminal, Ok(Live | Disposing)));
    } else if claim == 1 && terminating && fenced {
        assert_eq!(recovery.unwrap(), Disposing);
        assert_eq!(terminal.unwrap(), Disposing);
    } else if claim == 0 && total_absence {
        assert_eq!(recovery.unwrap(), Live);
        assert_eq!(terminal.unwrap(), Live);
    } else {
        assert_eq!(recovery.unwrap(), Incompatible);
        assert_eq!(terminal.unwrap(), Incompatible);
    }
}

/// Claim API access is owned only by an enabled continuation policy or an
/// already-persisted claim incarnation. A PVC-free deployment must not require
/// ambient PVC read permission merely to observe its ephemeral Pod.
pub(crate) fn continuation_claim_observation_required(
    continuation_configured: bool,
    exact_claim_expected: bool,
) -> bool {
    continuation_configured || exact_claim_expected
}

#[cfg(kani)]
/// @obligation sandbox_continuation.ephemeral_never_allocates_or_references_a_claim
/// @kani profile=solver-kissat-v1
/// @proof-input-debt free_symbol continuation_claim_observation_required continuation_configured
/// @proof-input-debt free_symbol continuation_claim_observation_required exact_claim_expected
#[kani::proof]
fn disabled_continuation_has_no_claim_api_obligation_without_durable_evidence() {
    let continuation_configured: bool = kani::any();
    let exact_claim_expected: bool = kani::any();
    let required =
        continuation_claim_observation_required(continuation_configured, exact_claim_expected);
    assert_eq!(required, continuation_configured || exact_claim_expected);
    if !continuation_configured && !exact_claim_expected {
        assert!(!required);
    }
}
