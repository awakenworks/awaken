use serde::{Deserialize, Serialize};

/// The durable run-projection fact: the committed [`Phase`] of a run, which is
/// the single terminal authority once the run ends. Replay and projection read
/// this fact; they never read a separately stored status or outcome.
///
/// [`Phase`]: crate::agent::run::Phase
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    pub run_id: crate::agent::run::Id,
    pub phase: crate::agent::run::Phase,
}
