// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Static owner: Managed projection of committed per-Run model and compaction observations.
// @formal reason: Dynamic path: one Thread recovery snapshot produces stable public span and context
// @formal reason: markers and records child ownership while committed Runtime facts remain authoritative.
// @formal reason: Evidence: session.multiagent.thread_authority and managed projection tests; this is a
// @formal reason: product-requirement classification, not a direct proof claim.
//! Projection of Runtime-owned model and context observations.

use super::*;
use awaken_managed_session_events::{FactKey, FactProvenance, FactRole};

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
        use awaken_runtime_contract::compaction::RunCompactionMarker;
        use awaken_runtime_contract::llm::ModelRequestObservation;

        let at = |role, provenance| FactKey::new(thread_id, role, provenance);
        for run in &snapshot.runs {
            if RunCompactionMarker::is_recorded(&snapshot.state, &run.id.0) {
                let key = at(
                    FactRole::RunContextCompacted,
                    FactProvenance::RunState {
                        run_id: run.id.0.clone(),
                    },
                );
                let id = record.fact_id(&key);
                if !record.has_event_id(&id) {
                    if thread_id != record.session.id {
                        record.event_thread_owners.insert(id, thread_id.to_string());
                    }
                    record.append_fact_once(key, OutboundKind::ThreadContextCompacted {});
                }
            }

            for audit in snapshot
                .events
                .iter()
                .filter(|audit| audit.run_id == run.id)
            {
                let Some(observation) =
                    ModelRequestObservation::from_record(audit).map_err(|error| {
                        StateError::Run(RunError::internal(format!(
                            "decode committed model request observation: {error}"
                        )))
                    })?
                else {
                    continue;
                };
                let provenance = || FactProvenance::Audit {
                    run_id: run.id.0.clone(),
                    sequence: audit.sequence,
                };
                let start = at(FactRole::ModelRequestStart, provenance());
                let end = at(FactRole::ModelRequestEnd, provenance());
                let start_id = record.fact_id(&start);
                let end_id = record.fact_id(&end);
                if record.has_event_id(&end_id) {
                    continue;
                }
                if thread_id != record.session.id {
                    record
                        .event_thread_owners
                        .insert(start_id.clone(), thread_id.to_string());
                    record
                        .event_thread_owners
                        .insert(end_id, thread_id.to_string());
                }
                record.push_fact(start, OutboundKind::SpanModelRequestStart {});
                record.push_fact(
                    end,
                    OutboundKind::SpanModelRequestEnd {
                        model_request_start_id: start_id,
                        is_error: Some(observation.is_error),
                        model_usage: SpanModelUsage::from(observation.usage),
                    },
                );
            }
        }
        Ok(())
    }
}
