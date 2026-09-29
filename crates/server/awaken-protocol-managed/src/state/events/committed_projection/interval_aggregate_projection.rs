// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
//! Canonical aggregate projections for persisted Session runtime intervals,
//! planned by [`awaken_managed_session_events::plan_interval_aggregate`].

use super::*;
use awaken_managed_session_events::{FactKey, IntervalStep};

impl ManagedState {
    pub(super) fn append_aggregate_running_once(record: &mut SessionRecord, key: FactKey) {
        record.append_fact_once(key, OutboundKind::SessionStatusRunning {});
    }

    pub(super) fn replace_interval_aggregate_projections(
        record: &mut SessionRecord,
        persisted: &awaken_session_state_contract::PersistedSession,
        lifecycle_events: &[RunLifecycleEvent],
        historical_pending: &std::collections::HashMap<
            awaken_agent_contract::RunLifecycleCursor,
            Pending,
        >,
        price_snapshot: Option<&awaken_session_budget_contract::ManagedListPriceSnapshot>,
    ) -> Result<(), StateError> {
        let plan = awaken_managed_session_events::plan_interval_aggregate(
            persisted,
            lifecycle_events,
            historical_pending,
        )
        .map_err(projection_error)?;
        if let Some(replaced) = plan.replaced {
            let replaced_ids = replaced
                .iter()
                .map(|key| record.fact_id(key))
                .collect::<std::collections::HashSet<_>>();
            record
                .events
                .retain(|event| !replaced_ids.contains(&event.id));
            record.rebuild_event_page_index();
        }
        for step in &plan.steps {
            match step {
                IntervalStep::AppendOnce(fact) => {
                    record.publish_fact_once(fact, price_snapshot)?;
                }
                IntervalStep::Close { usage, idle } => {
                    record.publish_fact(usage, price_snapshot)?;
                    record.publish_fact(idle, price_snapshot)?;
                }
            }
        }
        Ok(())
    }
}
