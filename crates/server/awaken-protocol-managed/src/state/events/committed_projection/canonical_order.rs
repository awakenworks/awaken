// @formal entrypoint session.runtime_projection_historical_pending_precedes_terminal_projection
// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
// @formal entrypoint session_time.event_order_is_independent_of_wall_time
// @formal evidence session_event_batch.idempotent_receipt_is_admission_stable
//! Canonical ordering and time projection for the disposable Managed event vector.
//!
//! Every durable event is ordered by the timeline fact behind it: the recorded
//! [`awaken_managed_session_events::FactKey`] of a multiagent event, the reversible
//! identity of a tool use, or the admission of an inbound command, resolved by
//! [`OrderIndex`] over the candidate's committed prefix. This module owns only
//! the Managed concerns around that order: live overlays and DTO timestamps.

use super::*;
use awaken_managed_session_events::{CanonicalOrder, FactProvenance, FactRole, OrderIndex};
use std::collections::HashMap;

pub(super) struct CanonicalizationEvidence<'a> {
    pub(super) outcome_projections: &'a [AnchoredOutcomeProjection],
    pub(super) index: &'a OrderIndex<'a>,
    pub(super) coordination: &'a awaken_managed_session_events::CommittedProjection,
}

fn retain_earliest<K: std::hash::Hash + Eq>(
    orders: &mut HashMap<K, CanonicalOrder>,
    key: K,
    order: CanonicalOrder,
) {
    orders
        .entry(key)
        .and_modify(|current| *current = (*current).min(order))
        .or_insert(order);
}

/// The same committed coordinate lookup serves the early child activity fold
/// and the final canonical sort. Fact keys and qualified tool ids are the
/// existing identity owners; a published Event position is never an input.
fn keyed_event_order(
    record: &SessionRecord,
    index: &OrderIndex<'_>,
    event: &Event,
    present_types: &std::collections::HashSet<&str>,
) -> Option<CanonicalOrder> {
    if let Some(tool) = decode_managed_tool_event_id(&event.id) {
        let thread_id = internal_thread_id(&record.session.id, tool.thread_id);
        return index.tool_order(&thread_id, tool.source_id, tool.call_id);
    }
    let key = record.checkpoint.progress.fact_keys.get(&event.id)?;
    let authenticated = match (&key.role, &key.provenance) {
        (FactRole::Transcript(role), FactProvenance::AssistantResponse { .. }) => {
            event.kind.type_str() == role.as_wire_type()
        }
        (FactRole::Transcript(role), _) => present_types.contains(role.as_wire_type()),
        _ => true,
    };
    authenticated.then(|| index.fact_order(key)).flatten()
}

/// Attach each live overlay to its explicit retained predecessor, or the
/// nearest earlier retained durable event. Consume the issued order once;
/// leading/unanchored overlays must not repeatedly rescan their whole prefix.
fn group_live_overlays<'a>(
    events: impl IntoIterator<Item = &'a Event>,
    durable_ids: &std::collections::HashSet<&str>,
    hints: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<Option<String>, Vec<Event>> {
    let mut result = std::collections::HashMap::<Option<String>, Vec<Event>>::new();
    let mut predecessor = None;
    for event in events {
        if !is_transient_event_id(&event.id) {
            if durable_ids.contains(event.id.as_str()) {
                predecessor = Some(event.id.as_str());
            }
            continue;
        }
        let anchor = hints
            .get(&event.id)
            .filter(|candidate| durable_ids.contains(candidate.as_str()))
            .map(String::as_str)
            .or(predecessor)
            .map(str::to_owned);
        result.entry(anchor).or_default().push(event.clone());
    }
    result
}

impl ManagedState {
    pub(super) fn canonicalize_committed_events(
        record: &mut SessionRecord,
        evidence: CanonicalizationEvidence<'_>,
    ) -> Result<(), StateError> {
        let CanonicalizationEvidence {
            outcome_projections,
            index,
            coordination,
        } = evidence;
        let session_id = record.session.id.clone();

        // An evaluation's public completion time is its evaluation command's.
        for projection in outcome_projections {
            let CommittedOutcomeProjection::Completed(report) = &projection.terminal else {
                continue;
            };
            for iteration in &report.iterations {
                let Some(source_commit_cursor) = projection
                    .evaluation_commit_cursors
                    .get(&iteration.iteration)
                    .copied()
                else {
                    continue;
                };
                if let Some(evaluation) =
                    record
                        .session
                        .outcome_evaluations
                        .iter_mut()
                        .find(|evaluation| {
                            evaluation.outcome_id == projection.outcome_id
                                && evaluation.iteration == iteration.iteration
                        })
                    && evaluation.completed_at.is_some()
                {
                    evaluation.completed_at = index
                        .time_for(&session_id, source_commit_cursor)
                        .map(awaken_session_application_contract::epoch_millis_to_rfc3339);
                }
            }
        }

        let inbound =
            index
                .inbound_orders()
                .fold(HashMap::new(), |mut orders, (operation_id, order)| {
                    retain_earliest(
                        &mut orders,
                        durable_inbound_event_id(&session_id, operation_id),
                        order,
                    );
                    orders
                });
        // A transcript fold's id authenticates its wire type: an assistant
        // response only as the type it was minted for, a Message fold only as
        // a type the projection still carries.
        let present_types = record
            .events
            .iter()
            .map(|event| event.kind.type_str())
            .collect::<std::collections::HashSet<_>>();
        let mut orders = HashMap::<String, CanonicalOrder>::new();
        let mut coordination_facts = Vec::new();
        let mut parent_terminal_facts = Vec::new();
        for event in &record.events {
            if is_transient_event_id(&event.id) {
                continue;
            }
            if let Some(order) = inbound.get(&event.id) {
                orders.insert(event.id.clone(), *order);
                continue;
            }
            let Some(key) = record.checkpoint.progress.fact_keys.get(&event.id) else {
                if let Some(order) = keyed_event_order(record, index, event, &present_types) {
                    orders.insert(event.id.clone(), order);
                }
                continue;
            };
            match &key.provenance {
                FactProvenance::LinkOperation { .. }
                | FactProvenance::CoordinationCall { .. }
                | FactProvenance::AgentReport { .. } => {
                    coordination_facts.push((event.id.clone(), key.clone()));
                }
                FactProvenance::ParentTerminal => {
                    parent_terminal_facts.push((event.id.clone(), key.clone()));
                }
                _ => {
                    if let Some(order) = keyed_event_order(record, index, event, &present_types) {
                        orders.insert(event.id.clone(), order);
                    }
                }
            }
        }
        for (id, key) in &coordination_facts {
            if let Some(order) = coordination.coordination_orders.get(key) {
                orders.insert(id.clone(), *order);
            }
        }
        let coordination_frontier = coordination
            .coordination_orders
            .values()
            .map(|order| order.source_commit_cursor)
            .max()
            .unwrap_or_default();

        // Initial activation can fail before Runtime owns a cleanup cursor. In
        // that case terminality follows the complete immutable prefix already
        // ordered above. The derived maximum is disposable ordering, never a
        // second persisted cursor or Session state.
        let frontier = orders
            .values()
            .map(|order| order.source_commit_cursor)
            .chain([index.evidence_frontier(), coordination_frontier])
            .max()
            .unwrap_or_default();
        if let Some(terminal_source) = index.parent_terminal_source(frontier) {
            for (id, key) in &parent_terminal_facts {
                if let Some(order) = index.parent_terminal_order(key, terminal_source) {
                    orders.insert(id.clone(), order);
                }
            }
        }

        record.event_times = orders
            .iter()
            .filter_map(|(id, order)| order.processed_at_unix_ms.map(|at| (id.clone(), at)))
            .collect();

        let visible_durable_ids = record
            .events
            .iter()
            .filter(|event| !is_transient_event_id(&event.id))
            .map(|event| event.id.clone())
            .collect::<std::collections::HashSet<_>>();
        let mut still_pending = Vec::new();
        for (event, predecessor) in std::mem::take(&mut record.overlay.pending_events) {
            if visible_durable_ids.contains(&predecessor) {
                record.events.push(event);
            } else {
                still_pending.push((event, predecessor));
            }
        }
        record.overlay.pending_events = still_pending;

        // Canonicalization is a disposable projection transaction: validate
        // every immutable anchor against a snapshot of the issued prefix, then
        // replace the cache once. A late/malformed fact must fail closed without
        // erasing receipts and cursors that callers already observed.
        let original_events = record.events.clone();
        let mut durable = Vec::new();
        for mut event in original_events.iter().cloned() {
            if is_transient_event_id(&event.id) {
                continue;
            } else {
                let order = orders.get(&event.id).copied().ok_or_else(|| {
                    StateError::Run(RunError::internal(format!(
                        "committed Managed event {} ({}) has no immutable projection anchor",
                        event.id,
                        event.type_str(),
                    )))
                })?;
                event.processed_at = event.processed_at.or_else(|| {
                    order
                        .processed_at_unix_ms
                        .map(awaken_session_application_contract::epoch_millis_to_rfc3339)
                });
                durable.push((order, event));
            }
        }
        durable.sort_by(|(left_order, left), (right_order, right)| {
            left_order
                .cmp(right_order)
                .then_with(|| left.id.cmp(&right.id))
        });
        let durable_ids = durable
            .iter()
            .map(|(_, event)| event.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        // A fact whose event a reduction replaced no longer names an id.
        record
            .checkpoint
            .progress
            .fact_keys
            .retain(|id, _| durable_ids.contains(id.as_str()));
        let mut overlays = group_live_overlays(
            original_events.iter(),
            &durable_ids,
            &record.overlay.anchors,
        );
        let mut canonical = overlays.remove(&None).unwrap_or_default();
        for (_, event) in durable {
            let id = event.id.clone();
            canonical.push(event);
            if let Some(mut anchored) = overlays.remove(&Some(id)) {
                canonical.append(&mut anchored);
            }
        }
        debug_assert!(
            overlays.is_empty(),
            "every transient overlay has a durable anchor"
        );
        for child in &mut record.child_threads {
            let visible = canonical
                .iter()
                .filter(|event| {
                    Self::project_event_for_thread(
                        &record.session.id,
                        &child.id,
                        (*event).clone(),
                        record
                            .event_thread_owners
                            .get(&event.id)
                            .map(String::as_str),
                        false,
                    )
                    .is_some()
                })
                .collect::<Vec<_>>();
            let times = visible
                .iter()
                .filter_map(|event| event.processed_at.as_ref())
                .collect::<Vec<_>>();
            let lifecycle_time = |created: bool| {
                canonical.iter().find_map(|event| {
                    let matches = match &event.kind {
                        OutboundKind::SessionThreadCreated {
                            session_thread_id, ..
                        } => created && session_thread_id == &child.id,
                        OutboundKind::SessionThreadStatusTerminated {
                            session_thread_id, ..
                        } => !created && session_thread_id == &child.id,
                        _ => false,
                    };
                    matches
                        .then(|| record.event_times.get(&event.id).copied())
                        .flatten()
                })
            };
            record.child_times.insert(
                child.id.clone(),
                ThreadTimeEvidence {
                    created_at_unix_ms: lifecycle_time(true),
                    // A process-local preview is not a committed update. The
                    // last durable visible event owns the v2 observation;
                    // if that event has no time, an older time cannot fill it.
                    updated_at_unix_ms: visible
                        .iter()
                        .rev()
                        .find(|event| !is_transient_event_id(&event.id))
                        .and_then(|event| record.event_times.get(&event.id).copied()),
                    archived_at_unix_ms: (child.status == SessionThreadStatus::Terminated)
                        .then(|| lifecycle_time(false))
                        .flatten(),
                },
            );
            // Creation/terminal time comes from the exact lifecycle event;
            // a prior broadcast interrupt or later cleanup cannot replace it.
            if let Some(at) = canonical.iter().find_map(|event| match &event.kind {
                OutboundKind::SessionThreadCreated {
                    session_thread_id, ..
                } if session_thread_id == &child.id => event.processed_at.clone(),
                _ => None,
            }) {
                child.created_at = at;
            }
            if let Some(last) = times.last() {
                child.updated_at = (*last).clone();
            }
            if child.status == SessionThreadStatus::Terminated
                && let Some(at) = canonical.iter().find_map(|event| match &event.kind {
                    OutboundKind::SessionThreadStatusTerminated {
                        session_thread_id, ..
                    } if session_thread_id == &child.id => event.processed_at.clone(),
                    _ => None,
                })
            {
                child.archived_at = Some(at);
            }
        }
        record.events = canonical;
        Ok(())
    }
}

#[cfg(test)]
mod overlay_anchor_tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn event(id: impl Into<String>) -> Event {
        Event {
            id: id.into(),
            kind: OutboundKind::AgentThinking {},
            processed_at: None,
        }
    }
    #[test]
    fn one_pass_overlay_assignment_preserves_explicit_and_fallback_predecessors() {
        // C1 explicit hint names a retained durable event; C2 hint is missing
        // or no longer retained; C3 a prior retained durable event exists.
        // R1 C1 => explicit hint wins, including a later retained event.
        // R2 !C1+C2+C3 => nearest prior durable; R3 !C1+!C3 => head group.
        // Empty history and a run of 1000 leading overlays consume exactly one
        // input traversal; order within each anchor group remains issued order.
        let events = [
            event("evt_0"),
            event("durable-a"),
            event("evt_1"),
            event("evt_2"),
            event("durable-b"),
            event("evt_3"),
        ];
        let durable = HashSet::from(["durable-a", "durable-b"]);
        let hints = HashMap::from([
            ("evt_1".into(), "durable-b".into()),
            ("evt_2".into(), "gone".into()),
            ("evt_3".into(), "durable-a".into()),
        ]);
        let result = group_live_overlays(events.iter(), &durable, &hints);
        let ids = |key: Option<&str>| {
            result
                .get(&key.map(str::to_owned))
                .unwrap()
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(None), ["evt_0"]);
        assert_eq!(ids(Some("durable-a")), ["evt_2", "evt_3"]);
        assert_eq!(ids(Some("durable-b")), ["evt_1"]);
        for count in [0, 1, 1000] {
            let events = (0..count)
                .map(|i| event(format!("evt_{i}")))
                .collect::<Vec<_>>();
            let inspected = std::cell::Cell::new(0);
            let result = group_live_overlays(
                events
                    .iter()
                    .inspect(|_| inspected.set(inspected.get() + 1)),
                &HashSet::new(),
                &HashMap::new(),
            );
            assert_eq!(inspected.get(), count);
            assert_eq!(result.get(&None).map(Vec::len).unwrap_or_default(), count);
            if count > 0 {
                assert_eq!(result[&None], events);
            }
        }
    }
}
