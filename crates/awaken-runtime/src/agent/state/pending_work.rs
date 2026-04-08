//! PendingWork state key — plugins set this to prevent NaturalEnd.
//!
//! When any plugin has work outstanding (e.g. background tasks still running),
//! it sets `PendingWork` to `true`. The orchestrator checks this at NaturalEnd
//! and enters `Waiting("awaiting_tasks")` instead of terminating.

use crate::state::StateKey;
use serde::{Deserialize, Serialize};

/// Whether the run has outstanding work that should prevent NaturalEnd.
///
/// Plugins write `true` when they have pending work (e.g. background tasks).
/// The orchestrator reads this at NaturalEnd to decide whether to terminate
/// or enter a waiting state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingWorkState {
    pub has_pending: bool,
}

pub struct PendingWorkKey;

impl StateKey for PendingWorkKey {
    const KEY: &'static str = "__runtime.pending_work";
    type Value = PendingWorkState;
    type Update = bool;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.has_pending = update;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_no_pending() {
        let state = PendingWorkState::default();
        assert!(!state.has_pending);
    }

    #[test]
    fn update_sets_pending() {
        let mut state = PendingWorkState::default();
        PendingWorkKey::apply(&mut state, true);
        assert!(state.has_pending);
    }

    #[test]
    fn update_clears_pending() {
        let mut state = PendingWorkState { has_pending: true };
        PendingWorkKey::apply(&mut state, false);
        assert!(!state.has_pending);
    }
}
