//! Architecture fitness function: commit boundary values are plain serializable
//! data.
//!
//! INVARIANTS G1 / G13: durable runtime writes go through `CommitCoordinator` as
//! a staged `ThreadCommit`; that value (and everything it carries — facts,
//! messages, state commands, event drafts) must be serializable data, never a
//! handle. `Serialize + DeserializeOwned` is that guarantee and is checked at
//! compile time: a non-serializable field stops this from compiling. Enforced by
//! `cargo check --workspace --all-targets` (pre-commit) and `cargo test` (push).

use serde::Serialize;
use serde::de::DeserializeOwned;

fn assert_boundary<T: Serialize + DeserializeOwned>() {}

#[test]
fn commit_boundary_values_are_plain_serializable_data() {
    use awaken_agent_contract::thread::commit::staged;

    assert_boundary::<staged::ThreadCommit>();
    assert_boundary::<staged::CommitRecord>();
}

// Phase is durable truth: every variant must survive a serde round trip, and
// the mid-flight `Running` shape is pinned so step commits stay readable.
#[test]
fn phase_variants_round_trip_and_running_wire_shape_is_stable() {
    use awaken_agent_contract::agent::run::{EndCause, Phase};

    for phase in [
        Phase::Running,
        Phase::Waiting,
        Phase::Ended(EndCause::NaturalEnd),
    ] {
        let json = serde_json::to_string(&phase).expect("serialize");
        let back: Phase = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(phase, back);
    }
    assert_eq!(
        serde_json::to_string(&Phase::Running).expect("serialize"),
        "\"Running\""
    );
}
