//! Subscription, committed-tail broadcast, and Session-event list projection.

use super::*;

#[cfg(feature = "test-support")]
const LIVE_CHANNEL_CAPACITY: usize = 64;
#[cfg(not(feature = "test-support"))]
const LIVE_CHANNEL_CAPACITY: usize = 1024;

impl ManagedState {
    pub(in crate::state) fn next_event_id(&self) -> String {
        format!("evt_{}", self.event_seq.fetch_add(1, Ordering::SeqCst))
    }

    /// The Session's committed SSE broadcast sender, created on first use.
    /// Capacity is generous so a fast Run's commit burst does not lag a slow
    /// subscriber into `Lagged` (which the stream tolerates by skipping).
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
        let rx = self.live_sender(session_id).subscribe();
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
        Ok((record.events.clone(), rx))
    }

    /// Publish each committed `Event` appended to `session_id` since `from` on the
    /// live broadcast, so an open SSE connection receives it without a reconnect.
    /// Best-effort: no subscriber (or a lagging one) is not an error.
    pub(in crate::state) fn broadcast_committed_from(
        &self,
        session_id: &str,
        record: &SessionRecord,
        from: usize,
    ) {
        if from >= record.events.len() {
            return;
        }
        if let Some(tx) = self.live.lock().unwrap().get(session_id) {
            for event in &record.events[from..] {
                let _ = tx.send(event.clone());
            }
        }
    }

    /// Broadcast only identities that were absent before a canonical rebuild.
    /// Reordering the disposable vector cannot make an older event look newly
    /// committed, and a newly completed interval is delivered in its canonical
    /// order even when rebuilding it inserted facts before a transient overlay.
    pub(in crate::state) fn broadcast_new_event_ids(
        &self,
        session_id: &str,
        record: &SessionRecord,
        previous_ids: &std::collections::HashSet<String>,
    ) {
        if let Some(tx) = self.live.lock().unwrap().get(session_id) {
            for event in record
                .events
                .iter()
                .filter(|event| !previous_ids.contains(&event.id))
            {
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
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
        let primary_answerable_event_ids = record.primary_answerable_event_ids();
        let primary_events = record.events.iter().cloned().filter_map(|event| {
            let owner = record.event_thread_owners.get(&event.id);
            let primary_references_event = primary_answerable_event_ids.contains(event.id.as_str());
            Self::project_event_for_thread(
                session_id,
                session_id,
                event,
                owner.map(String::as_str),
                primary_references_event,
            )
        });
        let ordered = if descending {
            primary_events.rev().collect::<Vec<_>>()
        } else {
            primary_events.collect::<Vec<_>>()
        };
        let page = paginate_by_id(&ordered, cursor, limit, |e| e.id.as_str())
            .map_err(|_| RunError::bad_request("unknown pagination cursor"))?;
        Ok(ListEventsResponse {
            data: page.items.to_vec(),
            next_page: page.next_page,
        })
    }
}
