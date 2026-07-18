//! The durable run-state fact, committed as part of a [`ThreadCommit`].
//!
//! [`ThreadCommit`]: crate::thread::commit::staged::ThreadCommit

use serde::{Deserialize, Serialize};

/// The durable run fact: a run's committed [`RunState`], the single terminal
/// authority once the run ends. Replay and projection read this fact; they never
/// read a separately stored status or outcome. It rides inside a [`ThreadCommit`]
/// (which already carries the thread id, so this fact omits it — the read-side
/// mirror [`crate::agent::run::Record`] adds `thread_id` to be self-describing).
///
/// [`RunState`]: crate::agent::run::RunState
/// [`ThreadCommit`]: crate::thread::commit::staged::ThreadCommit
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFact {
    pub run_id: crate::agent::run::Id,
    /// Serialized under the legacy `phase` key so existing FS journals and SQL JSON
    /// rows remain readable while the Rust domain language moves to `RunState`.
    #[serde(rename = "phase")]
    pub state: crate::agent::run::RunState,
}
