use std::collections::BTreeSet;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::contract::event::AgentEvent;
use crate::contract::event_store::{EventScope, EventStoreError, EventVisibility, FidelityClass};

use super::{NormalizedCanonicalEvent, ScopedAgentEventNormalizer};

#[derive(Debug, Default)]
pub(super) struct CompactionObservation {
    in_flight: Option<CompactionInFlight>,
    completed_fingerprints: BTreeSet<String>,
    failed_fingerprints: BTreeSet<String>,
    skipped_fingerprints: BTreeSet<String>,
    cancelled_fingerprints: BTreeSet<String>,
    max_total_compactions: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompactionInFlight {
    task_id: String,
    boundary_message_id: Option<String>,
}

impl ScopedAgentEventNormalizer {
    pub(super) fn context_compaction_events(
        &self,
        event: &AgentEvent,
    ) -> Result<Vec<NormalizedCanonicalEvent>, EventStoreError> {
        let AgentEvent::StateSnapshot { snapshot } = event else {
            return Ok(Vec::new());
        };
        let Some(state) = snapshot
            .get("extensions")
            .and_then(|extensions| extensions.get("__context_compaction"))
        else {
            return Ok(Vec::new());
        };

        let mut planned = Vec::new();
        {
            let mut observation = self.compaction.lock();
            reset_after_clear(state, &mut observation);
            let previous_in_flight = observation.in_flight.clone();
            self.plan_compaction_completed(state, &mut observation, &mut planned);
            self.plan_compaction_failed(state, &mut observation, &mut planned);
            self.plan_compaction_skipped(state, &mut observation, &mut planned);
            self.plan_compaction_cancelled(
                state,
                previous_in_flight.as_ref(),
                &mut observation,
                &mut planned,
            );
            self.plan_compaction_started(state, &mut observation, &mut planned);
        }

        planned
            .into_iter()
            .map(|(kind, payload)| {
                self.build_internal(
                    FidelityClass::DomainEvent,
                    kind,
                    self.compaction_scopes(),
                    payload,
                )
            })
            .collect()
    }

    fn compaction_scopes(&self) -> Vec<EventScope> {
        vec![EventScope::thread(self.context.thread_id.clone())]
    }

    fn build_internal(
        &self,
        fidelity: FidelityClass,
        event_kind: &str,
        scopes: Vec<EventScope>,
        payload: Value,
    ) -> Result<NormalizedCanonicalEvent, EventStoreError> {
        let mut event = self.build(fidelity, event_kind, scopes, payload)?;
        event.draft.visibility = EventVisibility::Internal;
        event.draft.correlation_id = None;
        Ok(event)
    }

    fn plan_compaction_started(
        &self,
        state: &Value,
        observation: &mut CompactionObservation,
        planned: &mut Vec<(&'static str, Value)>,
    ) {
        let Some(in_flight) = parse_in_flight(state) else {
            observation.in_flight = None;
            return;
        };
        if observation.in_flight.as_ref() == Some(&in_flight) {
            return;
        }
        let payload = json!({
            "thread_id": self.context.thread_id.as_str(),
            "task_id": in_flight.task_id.clone(),
            "boundary_message_id": in_flight.boundary_message_id.clone().unwrap_or_default(),
            "started_at_ms": state.get("in_flight").and_then(|value| value.get("started_at_ms")).and_then(Value::as_u64).unwrap_or(0),
        });
        observation.in_flight = Some(in_flight);
        planned.push(("ContextCompactionStarted", payload));
    }

    fn plan_compaction_completed(
        &self,
        state: &Value,
        observation: &mut CompactionObservation,
        planned: &mut Vec<(&'static str, Value)>,
    ) {
        let Some(boundaries) = state.get("boundaries").and_then(Value::as_array) else {
            return;
        };
        let total_compactions = state
            .get("total_compactions")
            .and_then(Value::as_u64)
            .unwrap_or(boundaries.len() as u64);
        for (index, boundary) in boundaries.iter().enumerate() {
            let pre_tokens = boundary
                .get("pre_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let post_tokens = boundary
                .get("post_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let timestamp_ms = boundary
                .get("timestamp_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let summary_hash = boundary
                .get("summary")
                .and_then(Value::as_str)
                .map(sha256_hex)
                .unwrap_or_else(|| sha256_hex(""));
            let task_id = non_blank_string(boundary, "task_id");
            let boundary_message_id = non_blank_string(boundary, "boundary_message_id");
            let fingerprint = format!(
                "completed/{}/{}/{index}/{timestamp_ms}/{pre_tokens}/{post_tokens}/{summary_hash}",
                task_id.as_deref().unwrap_or_default(),
                boundary_message_id.as_deref().unwrap_or_default()
            );
            if !observation.completed_fingerprints.insert(fingerprint) {
                continue;
            }
            let payload = json!({
                "thread_id": self.context.thread_id.as_str(),
                "boundary_index": index,
                "compaction_ordinal": index + 1,
                "total_compactions": total_compactions,
                "pre_tokens": pre_tokens,
                "post_tokens": post_tokens,
                "timestamp_ms": timestamp_ms,
                "summary_hash": summary_hash,
            });
            let mut payload = payload;
            if let Some(task_id) = task_id {
                payload["task_id"] = Value::String(task_id);
            }
            if let Some(boundary_message_id) = boundary_message_id {
                payload["boundary_message_id"] = Value::String(boundary_message_id);
            }
            planned.push(("ContextCompactionCompleted", payload));
        }
    }

    fn plan_compaction_failed(
        &self,
        state: &Value,
        observation: &mut CompactionObservation,
        planned: &mut Vec<(&'static str, Value)>,
    ) {
        let Some(failures) = state.get("failures").and_then(Value::as_array) else {
            return;
        };
        for (index, failure) in failures.iter().enumerate() {
            let task_id = non_blank_string(failure, "task_id");
            let boundary_message_id =
                non_blank_string(failure, "boundary_message_id").unwrap_or_default();
            let error =
                non_blank_string(failure, "error").unwrap_or_else(|| "unknown error".into());
            let timestamp_ms = failure
                .get("timestamp_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let fingerprint = format!(
                "failed/{index}/{timestamp_ms}/{}/{boundary_message_id}/{error}",
                task_id.as_deref().unwrap_or_default()
            );
            if !observation.failed_fingerprints.insert(fingerprint) {
                continue;
            }
            let mut payload = json!({
                "thread_id": self.context.thread_id.as_str(),
                "failure_index": index,
                "boundary_message_id": boundary_message_id,
                "error": error,
                "timestamp_ms": timestamp_ms,
            });
            if let Some(task_id) = task_id {
                payload["task_id"] = Value::String(task_id);
            }
            planned.push(("ContextCompactionFailed", payload));
        }
    }

    fn plan_compaction_skipped(
        &self,
        state: &Value,
        observation: &mut CompactionObservation,
        planned: &mut Vec<(&'static str, Value)>,
    ) {
        let Some(skipped_entries) = state.get("skipped").and_then(Value::as_array) else {
            return;
        };
        for (index, skipped) in skipped_entries.iter().enumerate() {
            let task_id = non_blank_string(skipped, "task_id");
            let boundary_message_id =
                non_blank_string(skipped, "boundary_message_id").unwrap_or_default();
            let reason = non_blank_string(skipped, "reason").unwrap_or_else(|| "unknown".into());
            let pre_tokens = skipped
                .get("pre_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let post_tokens = skipped
                .get("post_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let savings_ratio_ppm = skipped
                .get("savings_ratio_ppm")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let min_savings_ratio_ppm = skipped
                .get("min_savings_ratio_ppm")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let timestamp_ms = skipped
                .get("timestamp_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let fingerprint = format!(
                "skipped/{index}/{timestamp_ms}/{}/{boundary_message_id}/{reason}/{pre_tokens}/{post_tokens}/{savings_ratio_ppm}/{min_savings_ratio_ppm}",
                task_id.as_deref().unwrap_or_default()
            );
            if !observation.skipped_fingerprints.insert(fingerprint) {
                continue;
            }
            let mut payload = json!({
                "thread_id": self.context.thread_id.as_str(),
                "skip_index": index,
                "boundary_message_id": boundary_message_id,
                "reason": reason,
                "pre_tokens": pre_tokens,
                "post_tokens": post_tokens,
                "savings_ratio_ppm": savings_ratio_ppm,
                "min_savings_ratio_ppm": min_savings_ratio_ppm,
                "timestamp_ms": timestamp_ms,
            });
            if let Some(task_id) = task_id {
                payload["task_id"] = Value::String(task_id);
            }
            planned.push(("ContextCompactionSkipped", payload));
        }
    }

    fn plan_compaction_cancelled(
        &self,
        state: &Value,
        previous: Option<&CompactionInFlight>,
        observation: &mut CompactionObservation,
        planned: &mut Vec<(&'static str, Value)>,
    ) {
        let Some(previous) = previous else {
            return;
        };
        if parse_in_flight(state).as_ref() == Some(previous)
            || state_has_terminal_for_in_flight(state, previous)
        {
            return;
        }

        let fingerprint = format!(
            "cancelled/{}/{}",
            previous.task_id,
            previous.boundary_message_id.as_deref().unwrap_or_default()
        );
        if !observation.cancelled_fingerprints.insert(fingerprint) {
            return;
        }

        let mut payload = json!({
            "thread_id": self.context.thread_id.as_str(),
            "task_id": previous.task_id,
            "cancelled_at_ms": state.get("timestamp_ms").and_then(Value::as_u64).unwrap_or(0),
        });
        if let Some(boundary_message_id) = previous.boundary_message_id.clone() {
            payload["boundary_message_id"] = Value::String(boundary_message_id);
        }
        planned.push(("ContextCompactionCancelled", payload));
    }
}

fn reset_after_clear(state: &Value, observation: &mut CompactionObservation) {
    let total_compactions = state
        .get("total_compactions")
        .and_then(Value::as_u64)
        .unwrap_or(observation.max_total_compactions);
    if total_compactions < observation.max_total_compactions {
        observation.in_flight = None;
        observation.completed_fingerprints.clear();
        observation.failed_fingerprints.clear();
        observation.skipped_fingerprints.clear();
        observation.cancelled_fingerprints.clear();
    }
    observation.max_total_compactions = total_compactions;
}

fn parse_in_flight(state: &Value) -> Option<CompactionInFlight> {
    let in_flight = state.get("in_flight")?;
    let task_id = non_blank_string(in_flight, "task_id")?;
    Some(CompactionInFlight {
        task_id,
        boundary_message_id: non_blank_string(in_flight, "boundary_message_id"),
    })
}

fn state_has_terminal_for_in_flight(state: &Value, in_flight: &CompactionInFlight) -> bool {
    state
        .get("boundaries")
        .and_then(Value::as_array)
        .is_some_and(|boundaries| {
            boundaries
                .iter()
                .any(|boundary| compaction_entry_matches_in_flight(boundary, in_flight))
        })
        || state
            .get("failures")
            .and_then(Value::as_array)
            .is_some_and(|failures| {
                failures
                    .iter()
                    .any(|failure| compaction_entry_matches_in_flight(failure, in_flight))
            })
        || state
            .get("skipped")
            .and_then(Value::as_array)
            .is_some_and(|skipped_entries| {
                skipped_entries
                    .iter()
                    .any(|skipped| compaction_entry_matches_in_flight(skipped, in_flight))
            })
}

fn compaction_entry_matches_in_flight(entry: &Value, in_flight: &CompactionInFlight) -> bool {
    let task_id = non_blank_string(entry, "task_id");
    let boundary_message_id = non_blank_string(entry, "boundary_message_id");
    match (
        task_id.as_deref(),
        boundary_message_id.as_deref(),
        in_flight.boundary_message_id.as_deref(),
    ) {
        (Some(task_id), Some(boundary_message_id), Some(expected_boundary)) => {
            task_id == in_flight.task_id && boundary_message_id == expected_boundary
        }
        (Some(task_id), _, _) => task_id == in_flight.task_id,
        (_, Some(boundary_message_id), Some(expected_boundary)) => {
            boundary_message_id == expected_boundary
        }
        _ => false,
    }
}

fn non_blank_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}
