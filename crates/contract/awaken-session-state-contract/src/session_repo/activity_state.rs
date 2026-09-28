//! The Session root's one activity/execution reducer, borrowing its persisted fields.
//!
//! No receipt map or parallel state is stored here. SQL CAS, operation receipts,
//! interval publication and the choice to close Idle remain application-owned.

use super::{SessionExecutionState, SessionExecutionTransitionError};
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;

#[cfg(any(test, kani))]
mod model;

/// Every operation the reducer and its proofs perform on the active member
/// set, and nothing more. Production persists a `BTreeSet<u64>`; the bounded
/// two-admission proofs substitute [`model::TwoSlotMembers`], because
/// symbolically executing the real B-tree twice does not fit in memory
/// (see the owning obligation's `bounds` under `formal/obligations/`). A test in
/// this module checks the model against the real set on two-admission traces.
pub(super) trait ActivityMembers {
    /// Add a member; `true` iff it was absent.
    fn insert(&mut self, epoch: u64) -> bool;
    /// Observation only: the reducer never asks, its proofs and tests do.
    #[cfg(any(test, kani))]
    fn contains(&self, epoch: &u64) -> bool;
    /// Remove a member; `true` iff it was present.
    fn remove(&mut self, epoch: &u64) -> bool;
    #[cfg(any(test, kani))]
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool;
    /// Drop every member, as a terminal transition requires.
    fn clear(&mut self);
}

impl ActivityMembers for BTreeSet<u64> {
    fn insert(&mut self, epoch: u64) -> bool {
        BTreeSet::insert(self, epoch)
    }

    #[cfg(any(test, kani))]
    fn contains(&self, epoch: &u64) -> bool {
        BTreeSet::contains(self, epoch)
    }

    fn remove(&mut self, epoch: &u64) -> bool {
        BTreeSet::remove(self, epoch)
    }

    #[cfg(any(test, kani))]
    fn len(&self) -> usize {
        BTreeSet::len(self)
    }

    fn is_empty(&self) -> bool {
        BTreeSet::is_empty(self)
    }

    fn clear(&mut self) {
        BTreeSet::clear(self);
    }
}

pub(super) struct ActivityState<'a, Members: ActivityMembers = BTreeSet<u64>> {
    pub(super) epoch: &'a mut u64,
    pub(super) active: &'a mut Members,
    pub(super) execution: &'a mut SessionExecutionState,
}

impl<Members: ActivityMembers> ActivityState<'_, Members> {
    pub(super) fn transition_execution(
        &mut self,
        next: SessionExecutionState,
    ) -> Result<bool, SessionExecutionTransitionError> {
        let from = *self.execution;
        if !from.can_transition_to(next) {
            return Err(SessionExecutionTransitionError { from, to: next });
        }
        if from == next {
            return Ok(false);
        }
        if next.is_terminal() {
            self.active.clear();
        }
        *self.execution = next;
        Ok(true)
    }

    pub(super) fn begin_activity_epoch(&mut self) -> Option<u64> {
        let next = self.epoch.checked_add(1)?;
        *self.epoch = next;
        self.active.insert(next);
        Some(next)
    }

    pub(super) fn begin_activity_epoch_at(&mut self, epoch: u64) -> bool {
        if epoch == 0 || epoch <= *self.epoch {
            return false;
        }
        *self.epoch = epoch;
        self.active.insert(epoch)
    }

    pub(super) fn settle_activity_epoch(&mut self, expected_epoch: u64) -> Option<bool> {
        if self.active.is_empty() {
            return (*self.execution == SessionExecutionState::Running
                && expected_epoch != 0
                && expected_epoch == *self.epoch)
                .then_some(true);
        }
        self.active
            .remove(&expected_epoch)
            .then_some(self.active.is_empty())
    }
}

#[cfg(kani)]
mod proofs;
