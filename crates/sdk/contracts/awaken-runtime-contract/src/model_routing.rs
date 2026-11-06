// @formal entrypoint config.inference_access_resolved_once_at_publication
// @formal entrypoint model_route.fallback_preserves_the_complete_publication_pin
//! Exact selection from the immutable, publication-pinned model route set.
//!
//! A route is the complete [`ResolvedModelCandidate`](crate::resolved::ResolvedModelCandidate), not merely its model id.
//! Returning a reference into the published primary/fallback collection makes it
//! impossible for selection to reconstruct a candidate by mixing binding,
//! provisioning, credential, endpoint, or capability fields from two routes.

/// Select one complete publication-pinned candidate by its stable ordinal.
///
/// Ordinal zero is the primary route. Ordinal `n + 1` is fallback `n`. An
/// out-of-range ordinal fails closed instead of substituting another route.
#[must_use]
pub(crate) fn pinned_candidate_at<'a, T>(
    primary: &'a T,
    fallbacks: &'a [T],
    ordinal: usize,
) -> Option<&'a T> {
    if ordinal == 0 {
        Some(primary)
    } else {
        fallbacks.get(ordinal - 1)
    }
}

#[cfg(test)]
mod exhaustive {
    use super::pinned_candidate_at;

    /// @exhaustive ordinal 0..=3 (4) = 4
    /// @obligation config.inference_access_resolved_once_at_publication
    /// @obligation model_route.fallback_preserves_the_complete_publication_pin
    #[test]
    fn fallback_selection_preserves_the_complete_publication_pin() {
        // Test design — Causes: ordinal is primary, either published fallback,
        // or the first out-of-range value. Effects: selection returns the exact
        // referenced publication member or fails closed. Constraint: the
        // selector may not reconstruct or mix any route field. Decision rules:
        // R1 ordinal 0 -> primary; R2 ordinal 1/2 -> matching fallback;
        // R3 ordinal 3 -> None.
        // The production selector is payload-transparent: it never decodes or
        // reconstructs T. Prove its actual address selection without expanding
        // unrelated ModelBinding/String admission (proved by its own owner).
        // The ordinary test above instantiates this same function with complete
        // admitted ModelCandidates. This harness retains the same ordinal and
        // address-identity assertions; no duplicate Boolean selector is used.
        let primary = 1_u8;
        let fallbacks = [2_u8, 3_u8];
        for ordinal in 0..=fallbacks.len() + 1 {
            let selected = pinned_candidate_at(&primary, &fallbacks, ordinal);
            let exact = match (ordinal, selected) {
                (0, Some(candidate)) => std::ptr::eq(candidate, &primary),
                (1, Some(candidate)) => std::ptr::eq(candidate, &fallbacks[0]),
                (2, Some(candidate)) => std::ptr::eq(candidate, &fallbacks[1]),
                (_, None) => true,
                _ => false,
            };
            assert!(exact);
        }
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use crate::resolved::{ModelBinding, ResolvedModelCandidate};

    #[test]
    fn selection_returns_the_complete_real_candidate_or_none() {
        // Cause/effect table: P1 ordinal 0 -> primary; P2 ordinals 1/2 ->
        // corresponding fallback; P3 first out-of-range and usize::MAX -> None.
        // Distinct admitted model coordinates witness payload preservation;
        // reference identity proves no binding/provisioning fields are mixed.
        let candidate = |model| {
            ResolvedModelCandidate::host(
                ModelBinding::parse("provider", model, "native")
                    .expect("admitted literal test binding"),
            )
        };
        let primary = candidate("primary");
        let fallbacks = [candidate("fallback-one"), candidate("fallback-two")];
        for (ordinal, expected) in [(0, &primary), (1, &fallbacks[0]), (2, &fallbacks[1])] {
            let selected = pinned_candidate_at(&primary, &fallbacks, ordinal).unwrap();
            assert!(
                std::ptr::eq(selected, expected),
                "P1/P2 exact complete member"
            );
            assert_eq!(
                selected.binding().identity(),
                expected.binding().identity(),
                "P1/P2 coordinates"
            );
            assert_eq!(
                selected.provisioning(),
                expected.provisioning(),
                "P1/P2 provisioning"
            );
        }
        for ordinal in [3, usize::MAX] {
            assert!(
                pinned_candidate_at(&primary, &fallbacks, ordinal).is_none(),
                "P3"
            );
        }
    }
}
