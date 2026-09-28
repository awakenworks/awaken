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
            StateError::Run(RunError::internal(format!(
                "committed Managed event {} has conflicting {cause:?}",
                event.id,
            )))
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
