//! Budget-reach pauses the accepted committed prefix exposes, and the usage
//! and idle facts each one publishes.

use std::collections::HashMap;

use awaken_agent_contract::{RunLifecycleCursor, RunLifecycleEvent};
use awaken_session_application_contract::Pending;
use awaken_session_contract::PersistedSession;
use awaken_session_contract::SessionUsage;

use crate::{
    DerivedSessionEvent, EventIdentity, FactProvenance, ManagedEventType, ProjectionError,
    SessionEventKind, StopFact, budget_reach_projection_close_cursor,
};

/// One budget-reach pause the accepted committed prefix exposes: what the
/// Session had used when it crossed its cap, that usage's rounded public list
/// cost, and the cap.
///
/// Consumer: the protocol adapter's aggregate close, which publishes each
/// reach once, by generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetReach {
    pub generation: u64,
    pub usage: SessionUsage,
    pub list_cost_minor: Option<u64>,
    pub max_list_cost_minor: u64,
}

impl BudgetReach {
    /// Every recorded reach of `persisted` whose public pause the accepted
    /// lifecycle anchors. A crossing hidden behind an answerable ticket cannot
    /// be inferred from the current Session status; it stays withheld until a
    /// committed pause exposes it. A recorded usage that cannot be read fails
    /// the candidate.
    pub fn exposed(
        persisted: &PersistedSession,
        lifecycle: &[RunLifecycleEvent],
        historical_pending: &HashMap<RunLifecycleCursor, Pending>,
    ) -> Result<Vec<Self>, ProjectionError> {
        persisted
            .budget
            .reach_transitions()
            .iter()
            .filter(|transition| {
                budget_reach_projection_close_cursor(
                    persisted,
                    transition,
                    lifecycle,
                    historical_pending,
                )
                .is_some()
            })
            .map(|transition| {
                Ok(Self {
                    generation: transition.generation,
                    usage: transition
                        .usage_cursor
                        .to_session_usage()
                        .map_err(|error| ProjectionError::corrupt(error.to_string()))?,
                    list_cost_minor: transition.public_list_cost_minor(),
                    max_list_cost_minor: transition.max_list_cost_minor,
                })
            })
            .collect()
    }

    /// The usage then the budget idle this reach publishes on `session_id`'s
    /// aggregate.
    #[must_use]
    pub fn facts(&self, session_id: &str) -> [DerivedSessionEvent; 2] {
        let key = |role| {
            EventIdentity::new(
                session_id,
                role,
                FactProvenance::BudgetReach {
                    generation: self.generation,
                },
            )
        };
        [
            DerivedSessionEvent::new(
                key(ManagedEventType::BudgetReachUsage),
                SessionEventKind::BudgetReachUsage {
                    usage: self.usage.clone(),
                    list_cost_minor: self.list_cost_minor,
                    max_list_cost_minor: self.max_list_cost_minor,
                },
            ),
            DerivedSessionEvent::new(
                key(ManagedEventType::BudgetReachStatusIdle),
                SessionEventKind::SessionIdle {
                    stop: StopFact::BudgetReached,
                    calls_owner: session_id.to_string(),
                },
            ),
        ]
    }
}
