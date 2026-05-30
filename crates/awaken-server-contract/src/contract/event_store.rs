//! Server/store-owned canonical event store traits.
//!
//! The event data vocabulary (`CanonicalEventDraft`, `CanonicalEvent`,
//! `AppendOptions`, `EventStoreError`, scope/visibility/fidelity enums, page
//! and subscription types) stays in `awaken-runtime-contract` and is pulled in
//! via the glob below. The store capability traits — the durable read/write
//! surface — are a server/store concern and live here.

pub use awaken_runtime_contract::contract::event_store::*;

use async_trait::async_trait;

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
