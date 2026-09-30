// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
//! Canonical aggregate projections for persisted Session runtime intervals,
//! selected by the sole [`awaken_managed_session_events::plan_aggregate`].

use super::*;
use awaken_managed_session_events::{AggregatePlan, FactKey};

impl ManagedState {
    pub(super) fn append_aggregate_running_once(record: &mut SessionRecord, key: FactKey) {
        record.append_fact_once(key, OutboundKind::SessionStatusRunning {});
    }

    pub(super) fn append_interval_aggregate_projections(
        record: &mut SessionRecord,
        plan: &AggregatePlan,
        price_snapshot: Option<&awaken_session_budget_contract::ManagedListPriceSnapshot>,
    ) -> Result<(), StateError> {
        for close in &plan.interval_closes {
            record.publish_fact(&close.usage, price_snapshot)?;
            record.publish_fact(&close.idle, price_snapshot)?;
        }
        Ok(())
    }
}
