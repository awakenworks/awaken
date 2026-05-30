//! Server/store-owned canonical event store traits and store-output types.
//!
//! The input/identity vocabulary (drafts, options, ids, cursors, scopes, kinds,
//! visibility) stays in `awaken-runtime-contract` and is re-exported below by
//! name. The store-output types (the persisted `CanonicalEvent`, `EventPage`,
//! `AppendResult`, subscription handles and `FidelityClass`) and the capability
//! traits are server/store concerns and live here.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// Canonical event input & identity vocabulary shared with the runtime write
// boundary; defined in runtime-contract.
pub use awaken_runtime_contract::contract::event_store::{
    AppendOptions, CanonicalEventDraft, CanonicalEventId, CanonicalEventKind, EventCursor,
    EventScope, EventScopeFamily, EventScopeIds, EventStoreError, EventVisibility,
};

/// Durability class used by compacted and full-fidelity event capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FidelityClass {
    ObservedRuntimeEvent,
    CommittedRuntimeEvent,
    DomainEvent,
    ControlEvent,
}

/// EventStore append output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalEvent {
    pub event_id: CanonicalEventId,
    pub scopes: Vec<EventScope>,
    pub cursors_by_scope: BTreeMap<EventScope, EventCursor>,
    pub event_kind: CanonicalEventKind,
    #[serde(default)]
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    pub origin: String,
    #[serde(default)]
    pub visibility: EventVisibility,
    pub schema_version: u32,
    pub created_at: u64,
}

impl CanonicalEvent {
    /// Build a persisted canonical event from an accepted draft and store output.
    pub fn from_append(
        event_id: CanonicalEventId,
        cursors_by_scope: BTreeMap<EventScope, EventCursor>,
        created_at: u64,
        draft: CanonicalEventDraft,
    ) -> Result<Self, EventStoreError> {
        draft.validate()?;
        validate_cursor_coverage(&draft.scopes, &cursors_by_scope)?;
        let ids = draft.scope_ids()?;
        Ok(Self {
            event_id,
            scopes: draft.scopes,
            cursors_by_scope,
            event_kind: draft.event_kind,
            payload: draft.payload,
            thread_id: ids.thread_id,
            run_id: ids.run_id,
            causation_id: draft.causation_id,
            correlation_id: draft.correlation_id,
            origin: draft.origin,
            visibility: draft.visibility,
            schema_version: draft.schema_version,
            created_at,
        })
    }
}

/// Result returned by an append call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppendResult {
    pub event: CanonicalEvent,
}

impl AppendResult {
    #[must_use]
    pub fn event_id(&self) -> &CanonicalEventId {
        &self.event.event_id
    }

    #[must_use]
    pub fn cursors_by_scope(&self) -> &BTreeMap<EventScope, EventCursor> {
        &self.event.cursors_by_scope
    }
}

/// Paged canonical event list response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventPage {
    pub events: Vec<CanonicalEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<EventCursor>,
    pub has_more: bool,
}

/// Start position for event subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscribeStart {
    FromStart,
    FromCursor(EventCursor),
    FromNow,
}

/// Live canonical event stream.
pub type CanonicalEventStream = BoxStream<'static, Result<CanonicalEvent, EventStoreError>>;

/// Result returned when a live subscription is opened.
pub struct SubscribeHandle {
    pub start_cursor: Option<EventCursor>,
    pub stream: CanonicalEventStream,
}

fn validate_cursor_coverage(
    scopes: &[EventScope],
    cursors: &BTreeMap<EventScope, EventCursor>,
) -> Result<(), EventStoreError> {
    let scope_set = scopes.iter().collect::<BTreeSet<_>>();
    let cursor_scope_set = cursors.keys().collect::<BTreeSet<_>>();
    if scope_set != cursor_scope_set {
        return Err(EventStoreError::Validation(
            "cursors_by_scope must exactly match event scopes".to_string(),
        ));
    }
    Ok(())
}

/// Append canonical events.
#[async_trait]
pub trait EventWriter: Send + Sync {
    async fn append(
        &self,
        draft: CanonicalEventDraft,
        options: AppendOptions,
    ) -> Result<AppendResult, EventStoreError>;
}

/// Read canonical event history.
#[async_trait]
pub trait EventReader: Send + Sync {
    async fn list(
        &self,
        scope: EventScope,
        from: Option<EventCursor>,
        limit: usize,
    ) -> Result<EventPage, EventStoreError>;

    async fn count(&self, scope: EventScope) -> Result<u64, EventStoreError>;
}

/// Lookup canonical events by stable event id.
#[async_trait]
pub trait EventLookup: Send + Sync {
    async fn load_event(
        &self,
        event_id: &CanonicalEventId,
    ) -> Result<CanonicalEvent, EventStoreError>;
}

/// Subscribe to canonical event history and live tail.
#[async_trait]
pub trait EventSubscriber: Send + Sync {
    async fn subscribe(
        &self,
        scope: EventScope,
        start: SubscribeStart,
    ) -> Result<SubscribeHandle, EventStoreError>;
}

/// Full canonical event store capability.
pub trait EventStore: EventWriter + EventReader + EventLookup + EventSubscriber {}

impl<T> EventStore for T where T: EventWriter + EventReader + EventLookup + EventSubscriber {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn kind() -> CanonicalEventKind {
        CanonicalEventKind::new("RunStarted").unwrap()
    }

    fn cursor(value: &str) -> EventCursor {
        EventCursor::new(value).unwrap()
    }

    fn event_id(value: &str) -> CanonicalEventId {
        CanonicalEventId::new(value).unwrap()
    }

    #[test]
    fn persisted_event_requires_cursor_for_each_scope() {
        let draft = CanonicalEventDraft::new(
            vec![EventScope::thread("t1"), EventScope::run("r1")],
            kind(),
            serde_json::Value::Null,
            "server",
        )
        .unwrap();
        let mut cursors = BTreeMap::new();
        cursors.insert(EventScope::thread("t1"), cursor("cur_t_1"));

        let err = CanonicalEvent::from_append(event_id("evt_1"), cursors, 1, draft).unwrap_err();
        assert!(
            matches!(err, EventStoreError::Validation(message) if message.contains("cursors_by_scope"))
        );
    }

    #[test]
    fn persisted_event_carries_store_assigned_fields_and_denormalized_ids() {
        let draft = CanonicalEventDraft::new(
            vec![EventScope::thread("t1"), EventScope::run("r1")],
            kind(),
            serde_json::json!({"ok": true}),
            "server",
        )
        .unwrap();
        let mut cursors = BTreeMap::new();
        cursors.insert(EventScope::thread("t1"), cursor("cur_t_1"));
        cursors.insert(EventScope::run("r1"), cursor("cur_r_1"));

        let event = CanonicalEvent::from_append(event_id("evt_1"), cursors, 42, draft).unwrap();
        assert_eq!(event.event_id.as_str(), "evt_1");
        assert_eq!(event.thread_id.as_deref(), Some("t1"));
        assert_eq!(event.run_id.as_deref(), Some("r1"));
        assert_eq!(event.created_at, 42);
        assert_eq!(event.payload, serde_json::json!({"ok": true}));
    }
}
