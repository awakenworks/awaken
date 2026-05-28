//! In-memory canonical event store.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_server_contract::contract::event_store::{
    AppendOptions, AppendResult, CanonicalEvent, CanonicalEventDraft, CanonicalEventId,
    EventCursor, EventLookup, EventPage, EventReader, EventScope, EventStoreError, EventSubscriber,
    EventWriter, SubscribeHandle, SubscribeStart,
};
use futures::StreamExt;
use tokio::sync::{RwLock, broadcast};

const EVENT_BROADCAST_CAPACITY: usize = 1024;

#[derive(Debug, Default, Clone)]
pub(crate) struct InMemoryEventState {
    next_event_seq: u64,
    next_scope_seq: BTreeMap<EventScope, u64>,
    events: BTreeMap<CanonicalEventId, CanonicalEvent>,
    scope_indexes: BTreeMap<EventScope, Vec<CanonicalEventId>>,
    cursor_positions: BTreeMap<(EventScope, EventCursor), usize>,
    idempotency: BTreeMap<(String, String), IdempotencyRecord>,
}

#[derive(Debug, Clone)]
struct IdempotencyRecord {
    digest: Vec<u8>,
    event_id: CanonicalEventId,
}

#[derive(Debug)]
struct InMemoryEventStoreInner {
    state: RwLock<InMemoryEventState>,
    tx: broadcast::Sender<CanonicalEvent>,
}

/// In-memory implementation of canonical event-store traits.
#[derive(Debug, Clone)]
pub struct InMemoryEventStore {
    inner: Arc<InMemoryEventStoreInner>,
}

impl InMemoryEventStore {
    /// Create a new empty in-memory event store.
    #[must_use]
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        Self {
            inner: Arc::new(InMemoryEventStoreInner {
                state: RwLock::new(InMemoryEventState::default()),
                tx,
            }),
        }
    }
}

impl Default for InMemoryEventStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryEventStore {
    /// Snapshot the internal state for transactional rollback (ADR-0036).
    ///
    /// Crate-private hook used by `MemoryCommitCoordinator` to record a
    /// rollback point before performing a batch of writes. The broadcast
    /// channel is intentionally excluded: subscribers that already received
    /// events from a rolled-back commit see them via the live tail (this is
    /// the documented in-memory rollback caveat).
    pub(crate) async fn snapshot_state(&self) -> InMemoryEventState {
        self.inner.state.read().await.clone()
    }

    /// Restore previously-snapshotted state.
    pub(crate) async fn restore_state(&self, snapshot: InMemoryEventState) {
        *self.inner.state.write().await = snapshot;
    }
}

#[async_trait]
impl EventWriter for InMemoryEventStore {
    async fn append(
        &self,
        draft: CanonicalEventDraft,
        options: AppendOptions,
    ) -> Result<AppendResult, EventStoreError> {
        draft.validate()?;
        options.validate()?;
        validate_expected_cursor_scopes(&draft, &options)?;

        let digest = draft.idempotency_digest()?;
        let idempotency_identity = options.idempotency_identity()?;

        let event = {
            let mut state = self.inner.state.write().await;

            if let Some(identity) = idempotency_identity.as_ref()
                && let Some(record) = state.idempotency.get(identity)
            {
                if record.digest != digest {
                    return Err(EventStoreError::IdempotencyConflict(format!(
                        "idempotency identity reused with different input: writer_id={}, idempotency_key={}",
                        identity.0, identity.1
                    )));
                }
                let existing = state.events.get(&record.event_id).cloned().ok_or_else(|| {
                    EventStoreError::Integrity(format!(
                        "idempotent event missing: {}",
                        record.event_id.as_str()
                    ))
                })?;
                return Ok(AppendResult { event: existing });
            }

            validate_expected_prior_cursors(&state, &options)?;

            state.next_event_seq += 1;
            let event_id = CanonicalEventId::new(format!("evt_mem_{}", state.next_event_seq))?;
            let mut cursors_by_scope = BTreeMap::new();
            for scope in &draft.scopes {
                let next_scope_seq = {
                    let seq = state.next_scope_seq.entry(scope.clone()).or_insert(0);
                    *seq += 1;
                    *seq
                };
                cursors_by_scope.insert(
                    scope.clone(),
                    EventCursor::new(format!("cur_mem_{next_scope_seq}"))?,
                );
            }

            let event = CanonicalEvent::from_append(
                event_id.clone(),
                cursors_by_scope,
                crate::current_millis(),
                draft,
            )?;

            for scope in &event.scopes {
                let cursor = event.cursors_by_scope.get(scope).cloned().ok_or_else(|| {
                    EventStoreError::Integrity("persisted event missing scope cursor".to_string())
                })?;
                let index = state.scope_indexes.entry(scope.clone()).or_default();
                let position = index.len();
                index.push(event_id.clone());
                state
                    .cursor_positions
                    .insert((scope.clone(), cursor), position);
            }

            if let Some(identity) = idempotency_identity {
                state.idempotency.insert(
                    identity,
                    IdempotencyRecord {
                        digest,
                        event_id: event_id.clone(),
                    },
                );
            }
            state.events.insert(event_id, event.clone());
            // ADR-0034 D6: broadcast must fire while the write lock is
            // still held so any concurrent `subscribe(FromNow)` sees the
            // event either in its high-water cursor (subscribed after
            // send) or via the live receiver (subscribed before send) —
            // never in both, which would duplicate the delivery.
            let _ = self.inner.tx.send(event.clone());
            event
        };

        Ok(AppendResult { event })
    }
}

#[async_trait]
impl EventReader for InMemoryEventStore {
    async fn list(
        &self,
        scope: EventScope,
        from: Option<EventCursor>,
        limit: usize,
    ) -> Result<EventPage, EventStoreError> {
        let state = self.inner.state.read().await;
        let events = list_locked(&state, &scope, from.as_ref(), limit)?;
        Ok(events)
    }

    async fn count(&self, scope: EventScope) -> Result<u64, EventStoreError> {
        let state = self.inner.state.read().await;
        Ok(state
            .scope_indexes
            .get(&scope)
            .map_or(0, |events| events.len() as u64))
    }
}

#[async_trait]
impl EventLookup for InMemoryEventStore {
    async fn load_event(
        &self,
        event_id: &CanonicalEventId,
    ) -> Result<CanonicalEvent, EventStoreError> {
        let state = self.inner.state.read().await;
        state.events.get(event_id).cloned().ok_or_else(|| {
            EventStoreError::Integrity(format!("missing event: {}", event_id.as_str()))
        })
    }
}

#[async_trait]
impl EventSubscriber for InMemoryEventStore {
    async fn subscribe(
        &self,
        scope: EventScope,
        start: SubscribeStart,
    ) -> Result<SubscribeHandle, EventStoreError> {
        let state = self.inner.state.read().await;
        let rx = self.inner.tx.subscribe();
        let (start_cursor, history) = match start {
            SubscribeStart::FromStart => {
                let page = list_locked(&state, &scope, None, usize::MAX)?;
                (None, page.events)
            }
            SubscribeStart::FromCursor(cursor) => {
                let page = list_locked(&state, &scope, Some(&cursor), usize::MAX)?;
                (Some(cursor), page.events)
            }
            SubscribeStart::FromNow => (last_cursor_locked(&state, &scope), Vec::new()),
        };
        drop(state);

        let history_stream = futures::stream::iter(history.into_iter().map(Ok));
        let live_scope = scope.clone();
        let live_stream = futures::stream::unfold(rx, move |mut rx| {
            let live_scope = live_scope.clone();
            async move {
                loop {
                    match rx.recv().await {
                        Ok(event) if event.scopes.iter().any(|scope| scope == &live_scope) => {
                            return Some((Ok(event), rx));
                        }
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            return Some((
                                Err(EventStoreError::Integrity(format!(
                                    "event subscriber lagged by {skipped} messages"
                                ))),
                                rx,
                            ));
                        }
                        Err(broadcast::error::RecvError::Closed) => return None,
                    }
                }
            }
        });

        Ok(SubscribeHandle {
            start_cursor,
            stream: history_stream.chain(live_stream).boxed(),
        })
    }
}

fn validate_expected_cursor_scopes(
    draft: &CanonicalEventDraft,
    options: &AppendOptions,
) -> Result<(), EventStoreError> {
    for expected_scope in options.expected_prior_cursors.keys() {
        if !draft.scopes.iter().any(|scope| scope == expected_scope) {
            return Err(EventStoreError::Validation(format!(
                "expected cursor scope is not in append scope set: {expected_scope:?}"
            )));
        }
    }
    Ok(())
}

fn validate_expected_prior_cursors(
    state: &InMemoryEventState,
    options: &AppendOptions,
) -> Result<(), EventStoreError> {
    for (scope, expected_cursor) in &options.expected_prior_cursors {
        let actual = last_cursor_locked(state, scope);
        if actual.as_ref() != Some(expected_cursor) {
            return Err(EventStoreError::ExpectedCursorConflict(format!(
                "expected prior cursor mismatch for scope {scope:?}"
            )));
        }
    }
    Ok(())
}

fn list_locked(
    state: &InMemoryEventState,
    scope: &EventScope,
    from: Option<&EventCursor>,
    limit: usize,
) -> Result<EventPage, EventStoreError> {
    let ids = state.scope_indexes.get(scope).cloned().unwrap_or_default();
    let start = match from {
        Some(cursor) => state
            .cursor_positions
            .get(&(scope.clone(), cursor.clone()))
            .map(|position| position.saturating_add(1))
            .ok_or_else(|| EventStoreError::CursorExpired(cursor.as_str().to_string()))?,
        None => 0,
    };
    let end = start.saturating_add(limit).min(ids.len());
    let events = ids[start..end]
        .iter()
        .map(|event_id| {
            state.events.get(event_id).cloned().ok_or_else(|| {
                EventStoreError::Integrity(format!(
                    "event index points at missing event: {event_id:?}"
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let has_more = end < ids.len();
    let next_cursor = if has_more {
        events
            .last()
            .and_then(|event| event.cursors_by_scope.get(scope))
            .cloned()
    } else {
        None
    };
    Ok(EventPage {
        events,
        next_cursor,
        has_more,
    })
}

fn last_cursor_locked(state: &InMemoryEventState, scope: &EventScope) -> Option<EventCursor> {
    state
        .scope_indexes
        .get(scope)
        .and_then(|ids| ids.last())
        .and_then(|event_id| state.events.get(event_id))
        .and_then(|event| event.cursors_by_scope.get(scope))
        .cloned()
}
