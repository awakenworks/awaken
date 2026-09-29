//! P2, prefix monotonicity: a cold replica built after each commit of a
//! history keeps every id the cold replica of the previous commit published,
//! with the same payload and in the same relative order. A cold rebuild that
//! retracts or rewrites an id makes a client's cursor or payload depend on
//! when its replica restarted.
//!
//! Every named history is checked; a history that still violates the property
//! is listed with its timeline class and asserted to violate it, so fixing
//! the class fails this test until the name leaves the list.
//!
//! Consumer: the timeline's append-only guarantee across restarts.

use super::timeline_scenarios::*;

/// Histories whose cold rebuild is still not monotone, with their class.
const KNOWN_VIOLATIONS: &[(&str, &str)] = &[
    ("unaudited_permission_pause", "D"),
    ("child_unaudited_pause", "B/D"),
];

#[tokio::test]
async fn every_named_history_is_monotone_in_its_prefix() {
    // Rules: R1 a history off the list is monotone after every commit; R2 a
    // listed history still violates monotonicity (the list only shrinks).
    let mut unexpected = Vec::new();
    for (name, steps) in ROOT_SCENARIOS {
        judge(
            name,
            prefix_monotone::<RootTimeline>(steps).await,
            &mut unexpected,
        );
    }
    for (name, steps) in CHILD_SCENARIOS {
        judge(
            name,
            prefix_monotone::<ChildTimeline>(steps).await,
            &mut unexpected,
        );
    }
    assert!(unexpected.is_empty(), "{}", unexpected.join("\n"));
}

fn judge(name: &str, result: Result<(), String>, unexpected: &mut Vec<String>) {
    let known = KNOWN_VIOLATIONS.iter().find(|(known, _)| *known == name);
    match (result, known) {
        (Ok(()), None) | (Err(_), Some(_)) => {}
        (Err(violation), None) => unexpected.push(format!("R1 {name}: {violation}")),
        (Ok(()), Some((_, class))) => unexpected.push(format!(
            "R2 {name} (class {class}) is now monotone: remove it from KNOWN_VIOLATIONS"
        )),
    }
}
