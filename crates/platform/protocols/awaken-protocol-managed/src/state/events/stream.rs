// @formal evidence session.runtime_projection_delivery_preserves_exact_payload_digest_and_monotonic_completion
//! Subscription, committed-tail broadcast, and Session-event list projection.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

#[cfg(feature = "test-support")]
const LIVE_CHANNEL_CAPACITY: usize = 64;
#[cfg(not(feature = "test-support"))]
const LIVE_CHANNEL_CAPACITY: usize = 1024;

/// Owns one committed-event receiver. It retains no Session state or sender.
/// Last-observer removal is serialized with subscription and publication.
pub struct SessionEventSubscription {
    channels: std::sync::Weak<
        awaken_sync::LeafLock<std::collections::HashMap<String, broadcast::Sender<Event>>>,
    >,
    session_id: String,
    receiver: Option<broadcast::Receiver<Event>>,
}

impl SessionEventSubscription {
    pub async fn recv(&mut self) -> Result<Event, broadcast::error::RecvError> {
        self.receiver
            .as_mut()
            .expect("live subscription")
            .recv()
            .await
    }

    pub fn try_recv(&mut self) -> Result<Event, broadcast::error::TryRecvError> {
        self.receiver
            .as_mut()
            .expect("live subscription")
            .try_recv()
    }
}

impl Drop for SessionEventSubscription {
    fn drop(&mut self) {
        if let Some(channels) = self.channels.upgrade() {
            let mut channels = channels.lock();
            drop(self.receiver.take());
            if channels
                .get(&self.session_id)
                .is_some_and(|sender| sender.receiver_count() == 0)
            {
                channels.remove(&self.session_id);
            }
        }
    }
}

/// One immutable projection version and its matching delivery index.
pub(crate) struct StreamReplay {
    pub(crate) events: Arc<Vec<Event>>,
    index: Arc<super::super::session_record::event_page_index::EventPageIndex>,
}

impl StreamReplay {
    /// Initial replay retains only its immutable payload. Catch-up and terminal
    /// reconciliation acquire their own coherent current index when needed.
    pub(crate) fn into_events(self) -> Arc<Vec<Event>> {
        self.events
    }

    pub(crate) fn changed_after(&self, id: &str, cursor: (u64, usize)) -> bool {
        self.index.changed_after(id, cursor)
    }

    pub(crate) fn lifecycle_events(&self) -> impl Iterator<Item = &Event> {
        self.index
            .lifecycle_positions()
            .iter()
            .rev()
            .map(|&position| &self.events[position])
    }
}

pub(crate) fn preview_was_closed(
    state: &ManagedState,
    session_id: &str,
    event_id: &str,
) -> Result<bool, StateError> {
    let sessions = state.sessions.lock()?;
    let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
    Ok(record.event_page_index.closes_preview(event_id))
}

impl ManagedState {
    pub(crate) const STREAM_DELIVERY_PAGE_SIZE: usize = 128;
    /// The Session's committed SSE broadcast sender, created on first use.
    /// Capacity is generous so a fast Run's commit burst does not lag a slow
    /// subscriber into `Lagged`; an overflow wakes the same durable projection
    /// catch-up rather than dropping committed facts.
    fn live_receiver(&self, session_id: &str) -> SessionEventSubscription {
        let mut live = self.live.lock();
        let receiver = live
            .entry(session_id.to_string())
            .or_insert_with(|| broadcast::channel(LIVE_CHANNEL_CAPACITY).0)
            .subscribe();
        SessionEventSubscription {
            channels: Arc::downgrade(&self.live),
            session_id: session_id.to_owned(),
            receiver: Some(receiver),
        }
    }

    /// Open a live SSE subscription for `session_id`: the current committed-event
    /// snapshot (backfill) plus a receiver for committed events published after
    /// this call.
    /// Subscribing *before* cloning the snapshot means no committed event can slip
    /// through the gap — an event that lands mid-call is on the receiver, and the
    /// caller dedupes it against the snapshot by id.
    pub fn stream_subscribe(
        &self,
        session_id: &str,
    ) -> Result<(Vec<Event>, SessionEventSubscription), StateError> {
        self.stream_projection_subscribe(session_id)
            .map(|(snapshot, receiver, _)| ((*snapshot.events).clone(), receiver))
    }

    pub(crate) fn stream_projection_subscribe(
        &self,
        session_id: &str,
    ) -> Result<(StreamReplay, SessionEventSubscription, u64), StateError> {
        let rx = self.live_receiver(session_id);
        let sessions = self.sessions.lock()?;
        let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
        Ok((
            StreamReplay {
                events: record.events.snapshot(),
                index: record.event_page_index.clone(),
            },
            rx,
            record.cache_revision,
        ))
    }

    pub(crate) fn stream_delivery_page(
        &self,
        session_id: &str,
        after: (u64, usize),
    ) -> Result<CommittedDeliveryPage, StateError> {
        let sessions = self.sessions.lock()?;
        let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
        Ok(record.delivery_page(after, Self::STREAM_DELIVERY_PAGE_SIZE))
    }

    /// One emission edge for a fully validated committed delta. Publication
    /// and terminal deletion both use the same delivery admission beforehand.
    pub(in crate::state) fn broadcast_committed_delta(&self, session_id: &str, events: &[Event]) {
        if let Some(tx) = self.live.lock().get(session_id) {
            for event in events {
                let _ = tx.send(event.clone());
            }
        }
    }
    /// `GET /v1/sessions/{id}/events` — the session's events in the requested
    /// chronological direction, paged by cursor via the kernel's shared
    /// [`paginate_by_id`]. `cursor` is the id of the last event on the previous
    /// page; an absent/empty cursor starts at that direction's beginning; an
    /// unknown cursor is a caller error (400).
    pub fn list_events(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: Option<usize>,
        descending: bool,
    ) -> Result<ListEventsResponse, StateError> {
        self.list_events_filtered(session_id, cursor, limit, descending, |_| true)
    }

    /// Filter the canonical projection before ordering and cursor pagination.
    /// Applying a filter after paging would silently skip matching rows and
    /// manufacture incorrect continuation cursors.
    pub(crate) fn list_events_filtered(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: Option<usize>,
        descending: bool,
        include: impl Fn(&Event) -> bool,
    ) -> Result<ListEventsResponse, StateError> {
        self.list_events_filtered_with_time(
            session_id,
            cursor,
            limit,
            descending,
            false,
            false,
            include,
            |_| true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn list_events_filtered_with_time(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: Option<usize>,
        descending: bool,
        time_v2: bool,
        requires_time: bool,
        include: impl Fn(&Event) -> bool,
        could_match_without_time: impl Fn(&Event) -> bool,
    ) -> Result<ListEventsResponse, StateError> {
        let sessions = self.sessions.lock()?;
        let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
        Self::project_event_page_with_time(
            record,
            session_id,
            session_id,
            cursor,
            limit,
            descending,
            time_v2,
            requires_time,
            include,
            could_match_without_time,
        )
    }
}
