// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Static owner: Managed projection of committed per-Run model and compaction observations.
// @formal reason: Dynamic path: one Thread recovery snapshot produces stable public span and context
// @formal reason: markers and records child ownership while committed Runtime facts remain authoritative.
// @formal reason: Evidence: session.multiagent.thread_authority and managed projection tests; this is a
// @formal reason: product-requirement classification, not a direct proof claim.
//! Projection of Runtime-owned model and context observations (decided by
//! [`awaken_managed_session_events::run_observation_facts`]).

use super::*;

impl ManagedState {
    /// Project committed per-Run observations from the same recovery prefix as
    /// messages, lifecycle and resume tickets. This is the sole source of
    /// model-request spans and context-compaction markers; live Step results
    /// carry neither field.
    pub(in crate::state::events) fn append_run_observation_projections(
        record: &mut SessionRecord,
        thread_id: &str,
        snapshot: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    ) -> Result<(), StateError> {
        record.refresh_event_index();
        let facts = awaken_managed_session_events::run_observation_facts(thread_id, snapshot, |key| {
            record.event_positions(&record.fact_id(key)).is_some()
        })
        .map_err(projection_error)?;
        for fact in &facts {
            record.publish_fact(fact, None)?;
        }
        Ok(())
    }
}
