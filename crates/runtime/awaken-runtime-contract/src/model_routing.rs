//! Exact selection from the immutable, publication-pinned model route set.
//!
//! A route is the complete [`ResolvedModelCandidate`], not merely its model id.
//! Returning a reference into the published primary/fallback collection makes it
//! impossible for selection to reconstruct a candidate by mixing binding,
//! provisioning, credential, endpoint, or capability fields from two routes.

use crate::resolved::ResolvedModelCandidate;

/// Select one complete publication-pinned candidate by its stable ordinal.
///
/// Ordinal zero is the primary route. Ordinal `n + 1` is fallback `n`. An
/// out-of-range ordinal fails closed instead of substituting another route.
#[must_use]
pub fn pinned_candidate_at<'a>(
    primary: &'a ResolvedModelCandidate,
    fallbacks: &'a [ResolvedModelCandidate],
    ordinal: usize,
) -> Option<&'a ResolvedModelCandidate> {
    if ordinal == 0 {
        Some(primary)
    } else {
        fallbacks.get(ordinal - 1)
    }
}

/// Representation-free conjunction used by exact route identity checks.
/// Keeping the four axes explicit prevents later code from accidentally
/// treating a shared model id as a complete route identity.
#[must_use]
pub const fn route_pin_axes_are_exact(
    provider_identity_matches: bool,
    model_matches: bool,
    backend_matches: bool,
    provisioning_matches: bool,
) -> bool {
    provider_identity_matches && model_matches && backend_matches && provisioning_matches
}

/// Compare every top-level identity axis of two complete route pins.
#[must_use]
pub fn exact_candidate_identity(
    left: &ResolvedModelCandidate,
    right: &ResolvedModelCandidate,
) -> bool {
    route_pin_axes_are_exact(
        left.binding().provider_identity_ref == right.binding().provider_identity_ref,
        left.binding().model_ref == right.binding().model_ref,
        left.binding().backend_ref == right.binding().backend_ref,
        left.provisioning() == right.provisioning(),
    )
}

#[cfg(kani)]
mod kani_proofs {
    use super::{pinned_candidate_at, route_pin_axes_are_exact};
    use crate::resolved::{ModelBinding, ResolvedModelCandidate};

    fn candidate() -> ResolvedModelCandidate {
        ResolvedModelCandidate::host(ModelBinding::new("provider", "model", "native"))
    }

    #[kani::proof]
    fn fallback_selection_preserves_the_complete_publication_pin() {
        let primary = candidate();
        let fallbacks = [candidate(), candidate()];
        let ordinal: usize = kani::any();
        kani::assume(ordinal <= fallbacks.len() + 1);

        let selected = pinned_candidate_at(&primary, &fallbacks, ordinal);
        let exact = match (ordinal, selected) {
            (0, Some(candidate)) => std::ptr::eq(candidate, &primary),
            (1, Some(candidate)) => std::ptr::eq(candidate, &fallbacks[0]),
            (2, Some(candidate)) => std::ptr::eq(candidate, &fallbacks[1]),
            (_, None) => true,
            _ => false,
        };
        assert!(exact);

        // Candidate internals are immaterial to an address-preserving selector;
        // avoid expanding their unrelated collection destructors in CBMC.
        std::mem::forget(primary);
        std::mem::forget(fallbacks);
    }

    #[kani::proof]
    fn every_route_pin_axis_participates_in_exact_identity() {
        let mut axes = [true; 4];
        let axis: u8 = kani::any();
        kani::assume(axis < 4);
        axes[usize::from(axis)] = false;

        assert!(!route_pin_axes_are_exact(
            axes[0], axes[1], axes[2], axes[3]
        ));
    }
}
