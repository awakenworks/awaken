// @formal evidence session.runtime_projection_delivery_preserves_exact_payload_digest_and_monotonic_completion
//! Subscription, committed-tail broadcast, and Session-event list projection.

use super::*;

#[cfg(feature = "test-support")]
const LIVE_CHANNEL_CAPACITY: usize = 64;
#[cfg(not(feature = "test-support"))]
const LIVE_CHANNEL_CAPACITY: usize = 1024;

impl ManagedState {
    pub(crate) const STREAM_DELIVERY_PAGE_SIZE: usize = 128;
    /// The Session's committed SSE broadcast sender, created on first use.
    /// Capacity is generous so a fast Run's commit burst does not lag a slow
    /// subscriber into `Lagged`; an overflow wakes the same durable projection
    /// catch-up rather than dropping committed facts.
    fn live_sender(&self, session_id: &str) -> broadcast::Sender<Event> {
        let mut live = self.live.lock().unwrap();
        live.entry(session_id.to_string())
            .or_insert_with(|| broadcast::channel(LIVE_CHANNEL_CAPACITY).0)
            .clone()
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
    ) -> Result<(Vec<Event>, broadcast::Receiver<Event>), StateError> {
        self.stream_projection_subscribe(session_id)
            .map(|(snapshot, receiver, _)| (snapshot, receiver))
    }

    pub(crate) fn stream_projection_subscribe(
        &self,
        session_id: &str,
    ) -> Result<(Vec<Event>, broadcast::Receiver<Event>, u64), StateError> {
        let rx = self.live_sender(session_id).subscribe();
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
        Ok((record.events.clone(), rx, record.cache_revision))
    }

    pub(crate) fn stream_delivery_page(
        &self,
        session_id: &str,
        after: (u64, usize),
    ) -> Result<CommittedDeliveryPage, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
        Ok(record.delivery_page(after, Self::STREAM_DELIVERY_PAGE_SIZE))
    }

    /// One emission edge for a fully validated committed delta. Publication
    /// and terminal deletion both use the same delivery admission beforehand.
    pub(in crate::state) fn broadcast_committed_delta(&self, session_id: &str, events: &[Event]) {
        if let Some(tx) = self.live.lock().unwrap().get(session_id) {
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
        let sessions = self.sessions.lock().unwrap();
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
