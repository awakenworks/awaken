// @formal surface requirement=session.lifecycle.session_state
// @formal reason: The Session candidate owns usage-cursor recovery, cumulative reconciliation, reach
// @formal reason: transitions and limits through the existing budget state. Arithmetic proofs retain
// @formal reason: their original scope; this requirement link does not claim physical usage delivery.
use super::{
    ManagedBudgetUsageCursor, SessionAggregateCommandError, SessionBudgetOperations,
    SessionBudgetState,
};
use crate::budget::{BudgetReachTransition, ManagedListPriceError};

impl SessionBudgetOperations<'_> {
    /// Recover the historical neutral cursor only from this root's budget fact.
    pub fn seed_legacy_usage(&mut self) -> bool {
        let state = &mut self.aggregate.state;
        if state.usage_cursor != ManagedBudgetUsageCursor::default() {
            return false;
        }
        let Some(legacy) = state.budget.usage_cursor() else {
            return false;
        };
        state.usage_cursor = legacy.clone();
        true
    }

    /// Advance the root's neutral usage and configured budget together. A
    /// rejected rewind/arithmetic overflow leaves both projections unchanged.
    pub fn reconcile_cumulative_usage(
        &mut self,
        next: ManagedBudgetUsageCursor,
    ) -> Result<bool, ManagedListPriceError> {
        if !next.is_at_least(&self.aggregate.state.usage_cursor) {
            return Err(ManagedListPriceError::InvalidSnapshot(
                "cumulative usage cannot move backwards".into(),
            ));
        }
        let mut budget = self.aggregate.state.budget.clone();
        let changed = budget.reconcile_cumulative_usage(next.clone())?;
        self.aggregate.state.budget = budget;
        self.aggregate.state.usage_cursor = next;
        Ok(changed)
    }

    /// Retain the existing exact budget-crossing provenance.
    pub fn record_reach_transition(
        &mut self,
    ) -> Result<Option<BudgetReachTransition>, ManagedListPriceError> {
        self.aggregate.state.budget.record_reach_transition()
    }

    /// Adjust an existing budget while preserving accounting and price pins.
    /// None removes the threshold permanently; absence/removal cannot be undone.
    pub fn update_limit(&mut self, limit: Option<u64>) -> Result<(), SessionAggregateCommandError> {
        if self.aggregate.state.is_terminal()
            || self.aggregate.state.execution != super::SessionExecutionState::Idle
        {
            return Err(SessionAggregateCommandError::ConfigurationNotIdle);
        }
        let next = match (&self.aggregate.state.budget, limit) {
            (SessionBudgetState::Absent, _) => {
                return Err(SessionAggregateCommandError::BudgetAbsent);
            }
            (SessionBudgetState::Removed { .. }, Some(_)) => {
                return Err(SessionAggregateCommandError::BudgetRemoved);
            }
            (SessionBudgetState::Removed { .. }, None) => return Ok(()),
            (
                SessionBudgetState::Active {
                    consumed_numerator,
                    usage_cursor,
                    snapshot,
                    reach_transitions,
                    ..
                },
                Some(max_list_cost_minor),
            ) => {
                let threshold = u128::from(max_list_cost_minor)
                    * SessionBudgetState::MICROS_PER_MINOR_USD
                    * SessionBudgetState::COST_DENOMINATOR;
                if threshold <= *consumed_numerator {
                    return Err(SessionAggregateCommandError::BudgetNotAboveConsumed);
                }
                SessionBudgetState::Active {
                    max_list_cost_minor,
                    consumed_numerator: *consumed_numerator,
                    usage_cursor: usage_cursor.clone(),
                    snapshot: snapshot.clone(),
                    reach_transitions: reach_transitions.clone(),
                }
            }
            (
                SessionBudgetState::Active {
                    consumed_numerator,
                    usage_cursor,
                    snapshot,
                    reach_transitions,
                    ..
                },
                None,
            ) => SessionBudgetState::Removed {
                consumed_numerator: *consumed_numerator,
                usage_cursor: usage_cursor.clone(),
                snapshot: snapshot.clone(),
                reach_transitions: reach_transitions.clone(),
            },
        };
        self.aggregate.state.budget = next;
        Ok(())
    }
}
