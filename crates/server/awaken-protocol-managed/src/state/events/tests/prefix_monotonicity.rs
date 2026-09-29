//! P2, prefix monotonicity: a cold replica built after each commit of a
//! history keeps every id the cold replica of the previous commit published,
//! with the same payload and at the same position. A cold rebuild that
//! inserts a new durable fact before an already listable frontier makes a
//! client's after-cursor page omit the new fact even if old ids stay ordered.
//!
//! Every named history is checked, including damaged Awaiting prefixes that
//! must reject without publishing and recover at the next healthy prefix.
//!
//! Consumer: the timeline's append-only guarantee across restarts.

use super::timeline_scenarios::*;

#[tokio::test]
async fn every_named_history_is_monotone_in_its_prefix() {
    // Causes: C1 the previous cold read listed a durable prefix; C2 the next
    // commit can add an audit, lifecycle, child, or ToolUse Message; C3 a
    // processed timestamp can fill a prior None. Effects: E1 ids, kinds and
    // positions of C1 remain; E2 only C3 may update an old timestamp; E3 any
    // new durable fact appends after C1's frontier. Rules: R1 C1+C2 => E1+E3;
    // R2 C1+C3 => E1+E2; R3 missing exact Awaiting audit => typed rejection,
    // unchanged memo, then normal projection when a healthy prefix commits.
    // `late_root_tool_message` and `late_child_tool_message` exercise later
    // Message commits after Awaiting. C4 an absorbing child Archive commits
    // after that child's complete period; R4=C1+C4 => E1+E3, because current
    // Archive cannot retract earlier child/aggregate lifecycle facts.
    let mut unexpected = Vec::new();
    for (name, steps) in ROOT_SCENARIOS {
        if let Err(violation) = prefix_monotone::<RootTimeline>(steps).await {
            unexpected.push(format!("{name}: {violation}"));
        }
    }
    for (name, steps) in CHILD_SCENARIOS {
        if let Err(violation) = prefix_monotone::<ChildTimeline>(steps).await {
            unexpected.push(format!("{name}: {violation}"));
        }
    }
    assert!(unexpected.is_empty(), "{}", unexpected.join("\n"));
}
