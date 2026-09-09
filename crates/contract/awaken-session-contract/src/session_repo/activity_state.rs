//! The Session root's one activity/execution reducer, borrowing its persisted fields.
//!
//! No receipt map or parallel state is stored here. SQL CAS, operation receipts,
//! interval publication and the choice to close Idle remain application-owned.

use super::{SessionExecutionState, SessionExecutionTransitionError};
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;

pub(super) struct ActivityState<'a> {
    pub(super) epoch: &'a mut u64,
    pub(super) active: &'a mut BTreeSet<u64>,
    pub(super) execution: &'a mut SessionExecutionState,
}

impl ActivityState<'_> {
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
