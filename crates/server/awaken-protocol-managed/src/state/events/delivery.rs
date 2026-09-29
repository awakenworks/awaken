// @formal entrypoint session.runtime_projection_delivery_preserves_exact_payload_digest_and_monotonic_completion
// @formal evidence session.runtime_projection_logical_deletion_requires_positive_exact_scoped_evidence
//! One monotonic delivery rule for published history, broadcasts and SSE.
//! Fingerprints are disposable equality observations, never event authority.

use std::collections::HashMap;

use super::*;

pub(crate) type CommittedDeliveryPage = Vec<((u64, usize), Event)>;

/// One immutable logical-deletion projection for the producer, retained root
/// and scoped tombstone readers. Session ids cannot be recreated after deletion.
/// Cleanup can update root timestamps, so those are not a deletion observation.
pub(crate) fn session_deletion_event(owner_scope: &str, session_id: &str) -> Event {
    Event {
        id: format!(
            "evt_deleted_{}",
            awaken_agent_contract::stable_fingerprint(&(
                "managed-session-delete-v1",
                owner_scope,
                session_id,
            ))
        ),
        kind: OutboundKind::SessionDeleted {},
        processed_at: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventDeliveryConflict {
    Payload,
    ProcessedAt,
    Withdrawn,
    Reordered,
    InsertedBeforeFrontier,
    DuplicateId,
}

/// This exact comparison kernel is called by production after deriving the
/// immutable payload and processed observation from the owning Event DTO.
fn delivery_decision<P: Eq + ?Sized, T: Eq + ?Sized>(
    previous: Option<(&P, Option<&T>)>,
    payload: &P,
    processed_at: Option<&T>,
) -> Result<bool, EventDeliveryConflict> {
    let Some((previous_payload, previous_processed_at)) = previous else {
        return Ok(true);
    };
    if previous_payload != payload {
        return Err(EventDeliveryConflict::Payload);
    }
    match (previous_processed_at, processed_at) {
        (Some(previous), Some(current)) if previous != current => {
            Err(EventDeliveryConflict::ProcessedAt)
        }
        (None, Some(_)) => Ok(true),
        _ => Ok(false),
    }
}

#[derive(Clone)]
struct DeliveryStamp {
    payload: String,
    processed_at: Option<String>,
}

/// Connection/publication-local delivery progress. Store only the canonical
/// SHA-256 equality digest and optional completion time, not another payload
/// history. A stale frame cannot erase an already observed completion.
#[derive(Default)]
pub(crate) struct CommittedEventDelivery(HashMap<String, DeliveryStamp>);

impl CommittedEventDelivery {
    pub(crate) fn accept(&mut self, event: &Event) -> Result<bool, StateError> {
        let payload =
            awaken_agent_contract::canonical_json_sha256("managed-event-delivery-v1", &event.kind)
                .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?;
        let processed_at = event
            .processed_at
            .as_deref()
            .filter(|time| !time.is_empty());
        let previous = self
            .0
            .get(&event.id)
            .map(|stamp| (&stamp.payload, stamp.processed_at.as_deref()));
        let publish = delivery_decision(previous, &payload, processed_at).map_err(|cause| {
            StateError::event_history_conflict(&event.id, &format!("{cause:?}"))
        })?;
        if publish {
            self.0.insert(
                event.id.clone(),
                DeliveryStamp {
                    payload,
                    processed_at: processed_at.map(str::to_owned),
                },
            );
        }
        Ok(publish)
    }

    pub(crate) fn from_events(events: &[Event]) -> Result<Self, StateError> {
        let mut delivery = Self::default();
        for event in events {
            delivery.accept(event)?;
        }
        Ok(delivery)
    }
}

/// Validate the complete delta before publishing a candidate or any frame.
/// No partial broadcast may precede detection of a conflicting later Event.
pub(in crate::state) fn committed_delivery_delta(
    previous: &[Event],
    incoming: &mut [Event],
) -> Result<Vec<Event>, StateError> {
    let mut delivery = CommittedEventDelivery::from_events(previous)?;
    incoming
        .iter_mut()
        .filter_map(|event| match delivery.accept(event) {
            Ok(true) => Some(Ok(event.clone())),
            Ok(false) => {
                // Suppressing an old empty frame must also preserve GET's
                // already-issued completion under the same immutable payload.
                if event.processed_at.as_deref().is_none_or(str::is_empty) {
                    event.processed_at = delivery
                        .0
                        .get(&event.id)
                        .and_then(|stamp| stamp.processed_at.clone());
                }
                None
            }
            Err(error) => Some(Err(error)),
        })
        .collect()
}

/// A committed refresh extends the exact listable history held by this
/// replica. The deletion command uses `committed_delivery_delta` directly
/// because its one terminal event is delivered as the record is removed.
/// Here the existing delivery kernel still owns payload and completion
/// equality; this guard adds the position and membership rule before CAS.
pub(in crate::state) fn committed_projection_delivery_delta(
    session_id: &str,
    previous: &[Event],
    incoming: &mut [Event],
) -> Result<Vec<Event>, StateError> {
    // `evt_N` overlays are live previews rather than committed facts. The
    // canonicalizer can reattach them to a newly committed predecessor; only
    // durable ids belong to the cross-refresh pagination frontier.
    let previous_durable = previous
        .iter()
        .filter(|event| !is_transient_event_id(&event.id))
        .collect::<Vec<_>>();
    let incoming_durable = incoming
        .iter()
        .filter(|event| !is_transient_event_id(&event.id))
        .collect::<Vec<_>>();
    let previous_positions = previous_durable
        .iter()
        .enumerate()
        .map(|(position, event)| (event.id.as_str(), position))
        .collect::<HashMap<_, _>>();
    let mut incoming_positions = HashMap::new();
    for (position, event) in incoming_durable.iter().enumerate() {
        if incoming_positions
            .insert(event.id.as_str(), position)
            .is_some()
        {
            return Err(log_history_conflict(
                session_id,
                event,
                EventDeliveryConflict::DuplicateId,
                Some(event),
            ));
        }
    }
    for (position, old) in previous_durable.iter().enumerate() {
        let Some(current) = incoming_durable.get(position) else {
            return Err(log_history_conflict(
                session_id,
                old,
                EventDeliveryConflict::Withdrawn,
                None,
            ));
        };
        if current.id != old.id {
            let cause = if !incoming_positions.contains_key(old.id.as_str()) {
                EventDeliveryConflict::Withdrawn
            } else if previous_positions.contains_key(current.id.as_str()) {
                EventDeliveryConflict::Reordered
            } else {
                EventDeliveryConflict::InsertedBeforeFrontier
            };
            return Err(log_history_conflict(session_id, old, cause, Some(current)));
        }
    }
    committed_delivery_delta(previous, incoming).map_err(|error| {
        tracing::error!(session_id, error = %error, "Managed committed event history conflict");
        error
    })
}

fn log_history_conflict(
    session_id: &str,
    old: &Event,
    cause: EventDeliveryConflict,
    candidate: Option<&Event>,
) -> StateError {
    let candidate_id = candidate.map(|event| event.id.as_str());
    let candidate_type = candidate.map(Event::type_str);
    tracing::error!(
        session_id,
        old_id = %old.id,
        old_type = old.type_str(),
        ?candidate_id,
        ?candidate_type,
        ?cause,
        "Managed committed event history conflict"
    );
    let detail = match candidate {
        Some(event) => format!(
            "{cause:?} after {} ({}) before {} ({})",
            old.id,
            old.type_str(),
            event.id,
            event.type_str()
        ),
        None => format!("{cause:?} after {} ({})", old.id, old.type_str()),
    };
    StateError::event_history_conflict(&old.id, &detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: &str, time: Option<&str>, thinking: bool) -> Event {
        Event {
            id: id.into(),
            kind: if thinking {
                OutboundKind::AgentThinking {}
            } else {
                OutboundKind::SessionDeleted {}
            },
            processed_at: time.map(str::to_owned),
        }
    }

    #[test]
    fn delivery_covers_new_enriched_duplicate_regressed_and_conflicting_events() {
        // Cause/effect table: D1 unseen id=>publish; D2 exact id/payload with
        // null->time=>publish; D3 exact repeat=>stutter; D4 time->null/empty
        // =>stutter; D5 payload or nonempty time changes=>reject unchanged.
        // All outcomes run through production Event serialization and accept.
        let mut delivery = CommittedEventDelivery::default();
        assert!(delivery.accept(&event("one", None, true)).unwrap(), "D1");
        assert!(
            delivery.accept(&event("one", Some("t1"), true)).unwrap(),
            "D2"
        );
        for time in [Some("t1"), None, Some("")] {
            assert!(
                !delivery.accept(&event("one", time, true)).unwrap(),
                "D3/D4"
            );
        }
        for changed in [
            event("one", Some("t2"), true),
            event("one", Some("t1"), false),
        ] {
            assert!(delivery.accept(&changed).is_err(), "D5");
            assert!(
                !delivery.accept(&event("one", Some("t1"), true)).unwrap(),
                "D5 frame"
            );
        }
        assert!(
            delivery.accept(&event("two", Some("t1"), false)).unwrap(),
            "D1 distinct id"
        );
    }

    #[test]
    fn candidate_delta_includes_completion_and_rejects_partial_conflict() {
        // D6 the id is already issued but completion is new, plus a new id
        // => both delivered in canonical incoming order. D7 a later conflict
        // => no complete delta exists; caller cannot publish an earlier prefix.
        let old = [event("one", None, true)];
        let mut next = [event("one", Some("t1"), true), event("two", None, false)];
        assert_eq!(
            committed_delivery_delta(&old, &mut next).unwrap(),
            next,
            "D6"
        );
        let mut conflict = [event("new", None, true), event("one", None, false)];
        assert!(committed_delivery_delta(&old, &mut conflict).is_err(), "D7");
        // D8 a delayed empty observation retains the canonical GET marker.
        let known = [event("one", Some("t1"), true)];
        let mut delayed = [event("one", None, true)];
        assert!(
            committed_delivery_delta(&known, &mut delayed)
                .unwrap()
                .is_empty(),
            "D8"
        );
        assert_eq!(delayed, known, "D8 unchanged completion");
    }

    #[test]
    fn committed_refresh_preserves_the_entire_published_frontier() {
        // Causes: H1 an old listable [a,b] exists; H2 the candidate appends,
        // completes a prior None timestamp, withdraws a, reorders a/b, inserts
        // a new fact before b, duplicates an id, changes payload, or changes a
        // completed timestamp; H3 a transient `evt_N` preview reattaches
        // around the same durable event. Effects: E1 append and None->Some
        // completion produce a complete delta; E2 every other durable change
        // returns one typed conflict before install/broadcast; E3 H3 leaves
        // the committed frontier unchanged.
        // Rules: R1=H1+append=>E1; R2=H1+completion=>E1;
        // R3=H1+withdraw/reorder/insert/duplicate/payload/time change=>E2;
        // R4=H1+H3=>E3.
        let old = [event("a", None, true), event("b", None, false)];
        let mut appended = [old[0].clone(), old[1].clone(), event("c", None, true)];
        assert_eq!(
            committed_projection_delivery_delta("session", &old, &mut appended).unwrap(),
            vec![event("c", None, true)],
            "R1"
        );
        let mut completed = [event("a", Some("t1"), true), old[1].clone()];
        assert_eq!(
            committed_projection_delivery_delta("session", &old, &mut completed).unwrap(),
            vec![event("a", Some("t1"), true)],
            "R2"
        );
        let mut invalid = [
            vec![old[0].clone()],
            vec![old[1].clone(), old[0].clone()],
            vec![old[0].clone(), event("new", None, true), old[1].clone()],
            vec![old[0].clone(), old[1].clone(), old[1].clone()],
            vec![event("a", None, false), old[1].clone()],
        ];
        for (candidate, cause) in invalid.iter_mut().zip([
            "Withdrawn",
            "Reordered",
            "InsertedBeforeFrontier",
            "DuplicateId",
            "Payload",
        ]) {
            let error = committed_projection_delivery_delta("session", &old, candidate)
                .expect_err("R3 candidate must fail before publication");
            assert!(error.to_string().contains(cause), "R3 {cause}: {error}");
            assert!(
                matches!(error, StateError::Run(ref run)
                if run.code == "session_event_history_conflict"),
                "R3 typed {cause}"
            );
        }
        let completed_old = [event("a", Some("t1"), true)];
        let mut changed_time = [event("a", Some("t2"), true)];
        let error =
            committed_projection_delivery_delta("session", &completed_old, &mut changed_time)
                .expect_err("R3 completed timestamp mutation");
        assert!(error.to_string().contains("ProcessedAt"), "R3 {error}");
        let old_with_preview = [event("evt_1", None, true), event("a", None, true)];
        let mut reattached = [event("a", None, true), event("evt_1", None, true)];
        assert!(
            committed_projection_delivery_delta("session", &old_with_preview, &mut reattached)
                .unwrap()
                .is_empty(),
            "R4 transient preview does not define committed order"
        );
    }
}

#[cfg(kani)]
/// @obligation session.runtime_projection_delivery_preserves_exact_payload_digest_and_monotonic_completion
/// @kani profile=solver-kissat-v1
#[kani::proof]
fn committed_delivery_all_equalities_preserve_only_monotonic_completion() {
    // The actual generic production comparison, over arbitrary payload/time
    // representatives. This proves equality/Option transition semantics;
    // Event codec, SHA-256 collision resistance and transport are explicit
    // adapter/TCB boundaries, covered by the concrete D1-D7 tests above.
    let old_payload: u64 = kani::any();
    let payload: u64 = kani::any();
    let old_time: Option<u64> = kani::any();
    let time: Option<u64> = kani::any();
    let previous = kani::any::<bool>().then_some((&old_payload, old_time.as_ref()));
    let result = delivery_decision(previous, &payload, time.as_ref());
    if let Some((prior_payload, prior_time)) = previous {
        if prior_payload != &payload {
            assert_eq!(result, Err(EventDeliveryConflict::Payload));
        } else if prior_time.zip(time.as_ref()).is_some_and(|(a, b)| a != b) {
            assert_eq!(result, Err(EventDeliveryConflict::ProcessedAt));
        } else {
            assert_eq!(result, Ok(prior_time.is_none() && time.is_some()));
        }
    } else {
        assert_eq!(result, Ok(true));
    }
}
