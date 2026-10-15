// @formal entrypoint session_execution.activity_receipts_are_unique_durable_and_exactly_settled
//! The Session root's one activity/execution reducer, borrowing its persisted fields.
//!
//! No receipt map or parallel state is stored here. SQL CAS, operation receipts,
//! interval publication and the choice to close Idle remain application-owned.

use super::{SessionExecutionState, SessionExecutionTransitionError};
#[cfg(test)]
mod tests;

use awaken_agent_contract::agent::{run::Id as RunId, thread::Id as ThreadId};
use awaken_lease_contract::fence::Epoch;
use std::collections::BTreeSet;

/// One original, pre-effect activity admission. Data alone grants no execution.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRunActivityAdmissionRecord {
    pub(super) operation_id: String,
    pub(super) activity_epoch: Epoch,
    pub(super) thread_id: ThreadId,
    pub(super) run_id: RunId,
}

impl SessionRunActivityAdmissionRecord {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub fn activity_epoch(&self) -> Epoch {
        self.activity_epoch
    }
    pub fn thread_id(&self) -> &ThreadId {
        &self.thread_id
    }
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }
}

/// The logical effects after settling one exact activity epoch.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionActivitySettlementPlan {
    Stutter,
    RetainExecution,
    CloseRunningInterval,
}

impl SessionActivitySettlementPlan {
    /// Derive the effect from the aggregate's epoch-settlement observation.
    /// `None` denotes an unknown or duplicate epoch; `Some(false)` leaves other
    /// activities active, while `Some(true)` settles the final exact activity.
    /// Only the final observation during Running may close its interval.
    pub const fn from_epoch_settlement(
        settlement: Option<bool>,
        execution: SessionExecutionState,
    ) -> Self {
        session_activity_settlement_plan(
            settlement.is_some(),
            matches!(settlement, Some(true)),
            matches!(execution, SessionExecutionState::Running),
        )
    }
}

/// Decide the logical effect after the aggregate's exact epoch set has applied
/// a removal. Only removal of the final current epoch may close Running.
pub const fn session_activity_settlement_plan(
    exact_epoch_removed: bool,
    last_active: bool,
    execution_running: bool,
) -> SessionActivitySettlementPlan {
    if !exact_epoch_removed {
        SessionActivitySettlementPlan::Stutter
    } else if last_active && execution_running {
        SessionActivitySettlementPlan::CloseRunningInterval
    } else {
        SessionActivitySettlementPlan::RetainExecution
    }
}

/// Require complete original membership before interpreting absent work as
/// fresh. This is stronger observation eligibility; the historical optional
/// replay law below deliberately remains unchanged. No evidence is repaired.
pub fn validate_complete_run_activity(
    session: &super::PersistedSession,
    operation: Option<&str>,
    epoch: Option<u64>,
    thread: &awaken_agent_contract::ThreadId,
    run: &awaken_agent_contract::RunId,
) -> Result<(), super::SessionMutationValidationError> {
    let (records, unrecorded) = session
        .run_activities()
        .ok_or(super::SessionMutationValidationError::ActivityHistoryMismatch)?;
    if unrecorded || epoch == Some(0) {
        return Err(super::SessionMutationValidationError::ActivityHistoryMismatch);
    }
    let record = records.iter().find(|record| {
        operation.map_or_else(
            || epoch.is_some_and(|epoch| record.activity_epoch().get() == epoch),
            |operation| record.operation_id() == operation,
        )
    });
    match (epoch, record) {
        (None, None) => Ok(()),
        (Some(epoch), Some(record))
            if record.activity_epoch().get() == epoch
                && record.thread_id() == thread
                && record.run_id() == run =>
        {
            Ok(())
        }
        _ => Err(super::SessionMutationValidationError::ActivityHistoryMismatch),
    }
}

/// Retained facts of the existing Root, not another activity/child registry.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionRunActivities {
    pub(super) records: Vec<SessionRunActivityAdmissionRecord>,
    pub(super) unrecorded: bool,
}

/// Cause supplied only by the existing Root reducers. A registered Event wake
/// drives an already-retained batch; it is not anonymous execution admission.
pub(super) enum ActivityAdmissionSource<'a> {
    Named(&'a str, &'a ThreadId, &'a RunId),
    Anonymous,
    RegisteredEventWake,
}

impl SessionRunActivities {
    pub(super) const fn fresh() -> Self {
        Self {
            records: Vec::new(),
            unrecorded: false,
        }
    }

    pub(super) fn valid(&self, activity_epoch: Option<Epoch>) -> bool {
        let mut operations = BTreeSet::new();
        let mut previous = None;
        self.records.iter().all(|record| {
            let valid = previous.is_none_or(|previous| record.activity_epoch > previous)
                && activity_epoch.is_some_and(|maximum| record.activity_epoch <= maximum)
                && !record.operation_id.trim().is_empty()
                && !record.thread_id.0.trim().is_empty()
                && !record.run_id.0.trim().is_empty()
                && operations.insert(record.operation_id.as_str());
            previous = Some(record.activity_epoch);
            valid
        })
    }
}

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
    pub(super) history: &'a mut Option<Box<SessionRunActivities>>,
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
        self.begin_activity_epoch_at(next, ActivityAdmissionSource::Anonymous)
            .then_some(next)
    }

    pub(super) fn begin_activity_epoch_at(
        &mut self,
        epoch: u64,
        source: ActivityAdmissionSource<'_>,
    ) -> bool {
        let Ok(admission_epoch) = Epoch::try_from(epoch) else {
            return false;
        };
        if epoch <= *self.epoch {
            return false;
        }
        if let ActivityAdmissionSource::Named(operation, thread, run) = &source
            && (operation.trim().is_empty()
                || thread.0.trim().is_empty()
                || run.0.trim().is_empty()
                || self.history.as_deref().is_some_and(|history| {
                    history
                        .records
                        .iter()
                        .any(|record| record.operation_id == *operation)
                }))
        {
            return false;
        }
        if !self.active.insert(epoch) {
            return false;
        }
        *self.epoch = epoch;
        if let Some(history) = self.history.as_deref_mut() {
            match source {
                ActivityAdmissionSource::Named(operation, thread, run) => {
                    history.records.push(SessionRunActivityAdmissionRecord {
                        operation_id: operation.to_owned(),
                        activity_epoch: admission_epoch,
                        thread_id: thread.clone(),
                        run_id: run.clone(),
                    })
                }
                ActivityAdmissionSource::Anonymous => history.unrecorded = true,
                ActivityAdmissionSource::RegisteredEventWake => {}
            }
        }
        true
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
