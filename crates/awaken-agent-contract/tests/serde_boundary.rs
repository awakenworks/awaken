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
    use awaken_agent_contract::commit::staged;

    assert_boundary::<staged::ThreadCommit>();
    assert_boundary::<staged::CommitRecord>();
}
