//! P3, replica switch: a client that read a warm replica and continues on a
//! fresh cold one pages after every warm id to exactly the warm suffix. The
//! warm replica refreshes after every commit, as a live reader would.
//!
//! Every named history is checked; a damaged Awaiting prefix rejects without
//! publication, then a fresh replica must resume after the healthy suffix.
//!
//! Consumer: event-list cursors and SSE resumption across replicas.

use super::timeline_scenarios::*;

#[tokio::test]
async fn every_warm_cursor_resumes_on_a_cold_replica() {
    // Causes: C1 warm facts issued at each valid prefix; C2 a damaged Awaiting
    // prefix; C3 a later healthy prefix; C4 a ToolUse Message arriving after
    // its audit. Effects: E1 C2 rejects with no partial publication; E2 every
    // issued cursor has the same suffix on a cold replica after C3/C4. Rules:
    // R1 C1+C3 => E2; R2 C1+C2+C3 => E1+E2; R3 C1+C4 => E2.
    // C5 a child Archive commits after its visible period; R4=C1+C5=>E2:
    // a cold replica retains every old child and aggregate cursor/suffix.
    let mut unexpected = Vec::new();
    for (name, steps) in ROOT_SCENARIOS {
        if let Err(broken) = replica_switch_preserves_suffixes::<RootTimeline>(steps).await {
            unexpected.push(format!("{name}: {broken}"));
        }
    }
    for (name, steps) in CHILD_SCENARIOS {
        if let Err(broken) = replica_switch_preserves_suffixes::<ChildTimeline>(steps).await {
            unexpected.push(format!("{name}: {broken}"));
        }
    }
    assert!(unexpected.is_empty(), "{}", unexpected.join("\n"));
}
