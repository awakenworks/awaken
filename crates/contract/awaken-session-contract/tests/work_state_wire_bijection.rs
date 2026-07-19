//! Formal verification of the `WorkState` ⇄ wire-string bijection (ADR-0059 / issue C:
//! correct-by-construction). `as_str` and `from_wire` are inverses co-located in one
//! `impl`, so they cannot drift; this pins that as a checked law and — crucially — that an
//! UNRECOGNIZED string parses to `None` (fail-closed) rather than a silent valid variant,
//! which is what stops a corrupt/newer-schema durable row from being re-claimed and
//! double-executed. Exhaustive over the (finite) variant set = a complete proof of the
//! round-trip; property-based over arbitrary strings for the fail-closed direction.

use awaken_session_contract::work_queue::WorkState;
use proptest::prelude::*;

/// The complete variant set. If a variant is added, `as_str` grows an arm (compiler-
/// forced) and this list must grow too — the test then proves the new variant round-trips.
const ALL: &[WorkState] = &[
    WorkState::Queued,
    WorkState::Starting,
    WorkState::Active,
    WorkState::Stopping,
    WorkState::Stopped,
];

/// ROUND-TRIP (exhaustive = complete proof): `from_wire(as_str(v)) == Some(v)` for every
/// variant. The inverse is exact, so a state persisted then read back is never altered.
#[test]
fn as_str_then_from_wire_is_identity_for_every_variant() {
    for &v in ALL {
        assert_eq!(
            WorkState::from_wire(v.as_str()),
            Some(v),
            "{v:?} did not round-trip"
        );
    }
    // And every variant's string is distinct (no two variants collide on the wire).
    let mut seen = std::collections::BTreeSet::new();
    for &v in ALL {
        assert!(seen.insert(v.as_str()), "duplicate wire string for {v:?}");
    }
}

proptest! {
    /// FAIL-CLOSED: any string that is NOT one of the five wire tokens parses to `None`,
    /// never a silent variant. This is the property that prevents a corrupt persisted
    /// state from being mistaken for `Queued` (re-claimable). Only the exact known tokens
    /// yield `Some`.
    #[test]
    fn an_unknown_string_is_none(s in "\\PC{0,24}") {
        let known = ALL.iter().any(|v| v.as_str() == s);
        prop_assert_eq!(WorkState::from_wire(&s).is_some(), known,
            "from_wire disagreed with membership for {:?}", s);
    }
}
