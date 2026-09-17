//! Collection-independent agreement kernel for live ACP capability evidence.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapabilityEvidenceAgreement {
    Empty,
    One,
    Conflict,
}

/// Once two different profiles have been observed, later duplicates cannot
/// restore availability.
#[must_use]
pub(crate) const fn merge_capability_evidence(
    current: CapabilityEvidenceAgreement,
    matches_selected: bool,
) -> CapabilityEvidenceAgreement {
    match current {
        CapabilityEvidenceAgreement::Empty => CapabilityEvidenceAgreement::One,
        CapabilityEvidenceAgreement::One if matches_selected => CapabilityEvidenceAgreement::One,
        CapabilityEvidenceAgreement::One | CapabilityEvidenceAgreement::Conflict => {
            CapabilityEvidenceAgreement::Conflict
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn incompatible_capability_evidence_never_recovers_to_available() {
    let state = match kani::any::<u8>() % 3 {
        0 => CapabilityEvidenceAgreement::Empty,
        1 => CapabilityEvidenceAgreement::One,
        _ => CapabilityEvidenceAgreement::Conflict,
    };
    let matches_selected = kani::any::<bool>();
    let next = merge_capability_evidence(state, matches_selected);
    if state == CapabilityEvidenceAgreement::Conflict {
        assert_eq!(next, CapabilityEvidenceAgreement::Conflict);
    }
    if state == CapabilityEvidenceAgreement::One && !matches_selected {
        assert_eq!(next, CapabilityEvidenceAgreement::Conflict);
    }
    if next == CapabilityEvidenceAgreement::One {
        assert!(state == CapabilityEvidenceAgreement::Empty || matches_selected);
    }
}
