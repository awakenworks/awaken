//! P3, replica switch: a client that read a warm replica and continues on a
//! fresh cold one pages after every warm id to exactly the warm suffix. The
//! warm replica refreshes after every commit, as a live reader would.
//!
//! Every named history is checked; a history that still breaks the switch
//! is listed with its timeline class and asserted to break it, so fixing the
//! class fails this test until the name leaves the list.
//!
//! Consumer: event-list cursors and SSE resumption across replicas.

use super::timeline_scenarios::*;

/// Histories whose warm cursors still do not resume on a cold replica.
const KNOWN_BREAKS: &[(&str, &str)] = &[
    ("unaudited_permission_pause", "D"),
    ("cancelled_pause", "E"),
    ("cancelled_pause_without_activity", "A/E"),
    ("child_unaudited_pause", "B/D"),
];

#[tokio::test]
async fn every_warm_cursor_resumes_on_a_cold_replica() {
    // Rules: R1 a history off the list resumes every warm cursor on a cold
    // replica to the warm suffix; R2 a listed history still breaks.
    let mut unexpected = Vec::new();
    for (name, steps) in ROOT_SCENARIOS {
        judge(
            name,
            replica_switch_preserves_suffixes::<RootTimeline>(steps).await,
            &mut unexpected,
        );
    }
    for (name, steps) in CHILD_SCENARIOS {
        judge(
            name,
            replica_switch_preserves_suffixes::<ChildTimeline>(steps).await,
            &mut unexpected,
        );
    }
    assert!(unexpected.is_empty(), "{}", unexpected.join("\n"));
}

fn judge(name: &str, result: Result<(), String>, unexpected: &mut Vec<String>) {
    let known = KNOWN_BREAKS.iter().find(|(known, _)| *known == name);
    match (result, known) {
        (Ok(()), None) | (Err(_), Some(_)) => {}
        (Err(broken), None) => unexpected.push(format!("R1 {name}: {broken}")),
        (Ok(()), Some((_, class))) => unexpected.push(format!(
            "R2 {name} (class {class}) now resumes: remove it from KNOWN_BREAKS"
        )),
    }
}
