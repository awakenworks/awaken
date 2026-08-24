//! Projection of Runtime-owned model and context observations.

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
        use awaken_runtime_contract::compaction::RunCompactionMarker;
        use awaken_runtime_contract::llm::ModelRequestObservation;

        for run in &snapshot.runs {
            if RunCompactionMarker::is_recorded(&snapshot.state, &run.id.0) {
                let id = managed_multiagent_event_id(
                    &record.session.id,
                    thread_id,
                    "run-context-compacted",
                    ManagedMultiagentEventProvenance::RunState { run_id: &run.id.0 },
                );
                if !record.events.iter().any(|event| event.id == id) {
                    if thread_id != record.session.id {
                        record
                            .event_thread_owners
                            .insert(id.clone(), thread_id.to_string());
                    }
                    record.events.push(Event {
                        id,
                        kind: OutboundKind::ThreadContextCompacted {},
                        processed_at: Some(PROCESSED_AT.to_string()),
                    });
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
                let provenance = || ManagedMultiagentEventProvenance::Audit {
                    run_id: &run.id.0,
                    sequence: audit.sequence,
                };
                let start_id = managed_multiagent_event_id(
                    &record.session.id,
                    thread_id,
                    "model-request-start",
                    provenance(),
                );
                let end_id = managed_multiagent_event_id(
                    &record.session.id,
                    thread_id,
                    "model-request-end",
                    provenance(),
                );
                if record.events.iter().any(|event| event.id == end_id) {
                    continue;
                }
                if thread_id != record.session.id {
                    record
                        .event_thread_owners
                        .insert(start_id.clone(), thread_id.to_string());
                    record
                        .event_thread_owners
                        .insert(end_id.clone(), thread_id.to_string());
                }
                record.events.extend([
                    Event {
                        id: start_id.clone(),
                        kind: OutboundKind::SpanModelRequestStart {},
                        processed_at: Some(PROCESSED_AT.to_string()),
                    },
                    Event {
                        id: end_id,
                        kind: OutboundKind::SpanModelRequestEnd {
                            model_request_start_id: start_id,
                            is_error: Some(observation.is_error),
                            model_usage: SpanModelUsage {
                                input_tokens: observation.usage.prompt_tokens,
                                output_tokens: observation.usage.completion_tokens,
                                cache_read_input_tokens: observation.usage.cache_read_tokens,
                                cache_creation_input_tokens: observation
                                    .usage
                                    .cache_creation_tokens,
                                speed: None,
                            },
                        },
                        processed_at: Some(PROCESSED_AT.to_string()),
                    },
                ]);
            }
        }
        Ok(())
    }
}
