//! Live inbox: the input-direction mirror of the stream sink (G10/G13's
//! best-effort side). An in-process, editable queue of messages addressed to a
//! running attempt, consumed only at safe loop boundaries.
//!
//! A run's inputs are otherwise fixed at activation and reopened only by
//! resume after a park. The live inbox is the third input surface — "inject
//! while running" — for senders that outlive a single call site:
//!
//! - the engine drains it at natural-end/step boundaries and folds the
//!   messages into the transcript (messages become authoritative only through
//!   that commit, never by sitting in the queue);
//! - a sub-run blocks on [`LiveInbox::wait_for_change`] to stay alive until
//!   its background tasks report back;
//! - [`LiveCommand::Wake`](crate::control::LiveCommand) delivery lands as
//!   [`LiveInbox::wake`] — a versioned nudge, not a queue entry;
//! - the host's queued-message surface lists, removes, replaces, and reorders
//!   entries that the engine has not yet consumed.
//!
//! Everything here is process-local and best-effort: entries still queued when
//! the attempt closes are returned to the closer to route or drop, and the
//! durable pending-input path stays the at-least-once channel for parked runs.
//!
//! `LiveInbox` stays in the contract as a field of `RuntimeRunContext`
//! (`runtime_context::RuntimeRunContext`) — the parameter object of the
//! `RunExecutor` port. Its `Mutex`/`Notify` are process-local coordination, not
//! external infrastructure, so it is a contract citizen by virtue of that port,
//! not machinery that leaked in. (It cannot move to the engine without either
//! cycling the contract through its own port signature or introducing a
//! `dyn`-inbox port, which the design deliberately avoids.)

use std::collections::VecDeque;
use std::pin::pin;
use std::sync::{Arc, Mutex, MutexGuard};

use awaken_agent_contract::agent::message::Message;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Identity of one queued message, unique and monotonic within its inbox.
/// Serializable so the host's queued-message surface can hand it to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LiveInboxMessageId(pub u64);

/// Where a live-inbox entry came from — the *only* new fact the neutral runtime
/// needs to distinguish an out-of-band injection from the run's own carried-over
/// input. The system-vs-task distinction is already the message's
/// [`Role`](awaken_agent_contract::agent::message::Role); this names *provenance*,
/// not role.
///
/// The runtime only carries this tag through the fold. A consumer (e.g. an
/// operator-steering governance layer) reads it to decide policy — "an operator
/// steer may not override governed fields" — which the neutral runtime takes no
/// position on. Aligns with awaken-next's neutral `RunOrigin` vocabulary: the
/// substrate names the origin, the product names the actor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrigin {
    /// The run's own input — activation carry-over, a sub-run's background-task
    /// callback, or leftovers seeded into the next attempt. The default.
    #[default]
    Run,
    /// Injected from outside the run while it was in flight (the host's
    /// queued-message surface). What a product maps operator steering onto.
    External,
}

/// One queued message: content, the identity that makes it editable, and where it
/// came from. Identity ends at the drain — a consumed message can no longer be
/// targeted.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveInboxMessage {
    pub id: LiveInboxMessageId,
    pub origin: MessageOrigin,
    pub message: Message,
}

/// Outcome of [`LiveInbox::offer`]. `Closed` is a routing decision, not an
/// error: the sender falls back to the durable pending-input path or drops.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    Accepted(LiveInboxMessageId),
    Closed,
}

/// Why an edit to the queue was refused.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EditError {
    /// No queued message has this id — it was never offered, was removed,
    /// or the engine already consumed it.
    #[error("no queued message with that id")]
    UnknownMessage,
    /// The proposed order is not a permutation of the current queue; the
    /// caller's view is stale. Re-list and retry.
    #[error("proposed order does not match the current queue")]
    StaleOrder,
    /// The attempt is over; there is nothing left to edit.
    #[error("live inbox is closed")]
    Closed,
}

/// Outcome of [`LiveInbox::wait_for_change`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// The inbox changed since the observed version; the new version is
    /// returned so the next wait can resume from it.
    Changed(u64),
    Cancelled,
}

#[derive(Default)]
struct State {
    entries: VecDeque<LiveInboxMessage>,
    next_id: u64,
    version: u64,
    closed: bool,
}

/// Shared handle to one attempt's live inbox. Cheap to clone; producers,
/// editors, and the single consumer (the run's engine) all hold the same
/// queue. Only the engine drains; only the attempt owner closes.
#[derive(Clone, Default)]
pub struct LiveInbox {
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    notify: Notify,
}

impl LiveInbox {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .expect("live-inbox state lock poisoned")
    }

    fn bump_and_notify(state: &mut State, notify: &Notify) {
        state.version += 1;
        notify.notify_waiters();
    }

    /// Queue a message for the run as the run's own input
    /// ([`MessageOrigin::Run`]). Best-effort: `Closed` means the attempt is gone
    /// and the sender must decide (durable fallback or drop).
    pub fn offer(&self, message: Message) -> Offer {
        self.offer_as(MessageOrigin::Run, message)
    }

    /// Queue a message tagged with its [`MessageOrigin`]. The host's
    /// queued-message surface uses [`MessageOrigin::External`] for an out-of-band
    /// injection so a consumer can tell it apart from the run's own input.
    pub fn offer_as(&self, origin: MessageOrigin, message: Message) -> Offer {
        let mut state = self.state();
        if state.closed {
            return Offer::Closed;
        }
        state.next_id += 1;
        let id = LiveInboxMessageId(state.next_id);
        state.entries.push_back(LiveInboxMessage {
            id,
            origin,
            message,
        });
        Self::bump_and_notify(&mut state, &self.shared.notify);
        Offer::Accepted(id)
    }

    /// Nudge the consumer to re-check state without queueing content — the
    /// landing point for `LiveCommand::Wake`. A wake racing the end of the
    /// attempt is normal, so waking a closed inbox is a no-op.
    pub fn wake(&self) {
        let mut state = self.state();
        if state.closed {
            return;
        }
        Self::bump_and_notify(&mut state, &self.shared.notify);
    }

    /// Snapshot of the queue in consumption order, for the host's
    /// queued-message surface. Ids in the snapshot stay targetable until the
    /// engine drains them.
    pub fn list(&self) -> Vec<LiveInboxMessage> {
        self.state().entries.iter().cloned().collect()
    }

    /// Delete one queued message, returning its content.
    pub fn remove(&self, id: LiveInboxMessageId) -> Result<Message, EditError> {
        let mut state = self.state();
        if state.closed {
            return Err(EditError::Closed);
        }
        let index = state
            .entries
            .iter()
            .position(|entry| entry.id == id)
            .ok_or(EditError::UnknownMessage)?;
        let removed = state.entries.remove(index).expect("index just found");
        Self::bump_and_notify(&mut state, &self.shared.notify);
        Ok(removed.message)
    }

    /// Replace one queued message's content, keeping its id and position.
    pub fn replace(&self, id: LiveInboxMessageId, message: Message) -> Result<(), EditError> {
        let mut state = self.state();
        if state.closed {
            return Err(EditError::Closed);
        }
        let entry = state
            .entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .ok_or(EditError::UnknownMessage)?;
        entry.message = message;
        Self::bump_and_notify(&mut state, &self.shared.notify);
        Ok(())
    }

    /// Reorder the queue to exactly `order`, which must be a permutation of
    /// the current ids. Anything else is a stale view of the queue
    /// (a concurrent offer, remove, or drain won the race): the caller
    /// re-lists and retries; the queue is left untouched.
    pub fn reorder(&self, order: &[LiveInboxMessageId]) -> Result<(), EditError> {
        let mut state = self.state();
        if state.closed {
            return Err(EditError::Closed);
        }
        if order.len() != state.entries.len() {
            return Err(EditError::StaleOrder);
        }
        let mut reordered = VecDeque::with_capacity(order.len());
        let mut remaining: Vec<Option<LiveInboxMessage>> =
            state.entries.drain(..).map(Some).collect();
        for id in order {
            match remaining
                .iter_mut()
                .find(|slot| slot.as_ref().is_some_and(|entry| entry.id == *id))
            {
                Some(slot) => reordered.push_back(slot.take().expect("slot just matched")),
                None => {
                    // Unknown or duplicated id: restore and refuse.
                    state.entries = remaining.into_iter().flatten().collect();
                    let mut restored = std::mem::take(&mut reordered);
                    while let Some(entry) = restored.pop_back() {
                        state.entries.push_front(entry);
                    }
                    return Err(EditError::StaleOrder);
                }
            }
        }
        state.entries = reordered;
        Self::bump_and_notify(&mut state, &self.shared.notify);
        Ok(())
    }

    /// Take everything queued, in order. The point of no return: consumed
    /// messages leave the editable window, and the engine folds them into
    /// the transcript, where the commit — not the queue — makes them true.
    pub fn drain_at_boundary(&self) -> Vec<LiveInboxMessage> {
        self.state().entries.drain(..).collect()
    }

    /// Current change version. Bumped by every offer/wake/edit and by close,
    /// never by drain: the version tracks input-side changes the consumer
    /// waits on, not its own consumption.
    pub fn version(&self) -> u64 {
        self.state().version
    }

    /// Wait until the inbox changes past `seen` or the token cancels.
    /// Cancellation wins over a simultaneous change, matching the engine's
    /// cancel-first discipline at boundaries.
    pub async fn wait_for_change(&self, seen: u64, cancel: &CancellationToken) -> WaitOutcome {
        loop {
            if cancel.is_cancelled() {
                return WaitOutcome::Cancelled;
            }
            let mut notified = pin!(self.shared.notify.notified());
            // Register interest before the version check so a bump between
            // the check and the await is never missed.
            notified.as_mut().enable();
            {
                let state = self.state();
                if state.version != seen {
                    return WaitOutcome::Changed(state.version);
                }
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return WaitOutcome::Cancelled,
                _ = notified => {}
            }
        }
    }

    /// End the attempt's live input. Whatever is still queued comes back to
    /// the closer, who decides its fate — re-queue durably or drop. After
    /// close, offers report `Closed` and edits fail.
    #[must_use = "undelivered messages must be routed or knowingly dropped"]
    pub fn close(&self) -> Vec<LiveInboxMessage> {
        let mut state = self.state();
        if state.closed {
            return Vec::new();
        }
        state.closed = true;
        let leftovers = state.entries.drain(..).collect();
        Self::bump_and_notify(&mut state, &self.shared.notify);
        leftovers
    }

    pub fn is_closed(&self) -> bool {
        self.state().closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id, Role};

    fn msg(text: &str) -> Message {
        Message::text(Id(format!("m-{text}")), Role::User, text)
    }

    fn offered(inbox: &LiveInbox, text: &str) -> LiveInboxMessageId {
        match inbox.offer(msg(text)) {
            Offer::Accepted(id) => id,
            Offer::Closed => panic!("inbox unexpectedly closed"),
        }
    }

    fn texts(entries: &[LiveInboxMessage]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| entry.message.text_content())
            .collect()
    }

    #[test]
    fn offer_defaults_to_run_origin() {
        let inbox = LiveInbox::new();
        offered(&inbox, "a");
        assert_eq!(inbox.list()[0].origin, MessageOrigin::Run);
        assert_eq!(MessageOrigin::default(), MessageOrigin::Run);
    }

    #[test]
    fn offer_as_tags_the_origin_and_it_survives_list_and_drain() {
        let inbox = LiveInbox::new();
        assert!(matches!(
            inbox.offer_as(MessageOrigin::External, msg("steer")),
            Offer::Accepted(_)
        ));
        let _ = inbox.offer(msg("task"));
        // Listed in order with their origins intact.
        let listed = inbox.list();
        assert_eq!(listed[0].origin, MessageOrigin::External);
        assert_eq!(listed[1].origin, MessageOrigin::Run);
        // The tag rides through the drain the engine folds from.
        let drained = inbox.drain_at_boundary();
        assert_eq!(drained[0].origin, MessageOrigin::External);
        assert_eq!(drained[1].origin, MessageOrigin::Run);
    }

    #[test]
    fn replace_keeps_the_original_origin() {
        let inbox = LiveInbox::new();
        let id = match inbox.offer_as(MessageOrigin::External, msg("v1")) {
            Offer::Accepted(id) => id,
            Offer::Closed => panic!("closed"),
        };
        inbox.replace(id, msg("v2")).unwrap();
        let entry = &inbox.list()[0];
        assert_eq!(entry.message.text_content(), "v2");
        // Editing content must not silently relaunder the provenance to Run.
        assert_eq!(entry.origin, MessageOrigin::External);
    }

    #[test]
    fn message_origin_serde_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&MessageOrigin::External).unwrap(),
            "\"external\""
        );
    }

    #[test]
    fn offer_then_drain_preserves_fifo_order_with_monotonic_ids() {
        let inbox = LiveInbox::new();
        let a = offered(&inbox, "a");
        let b = offered(&inbox, "b");
        let c = offered(&inbox, "c");
        assert!(a.0 < b.0 && b.0 < c.0);

        let drained = inbox.drain_at_boundary();
        assert_eq!(texts(&drained), ["a", "b", "c"]);
        assert_eq!(drained.iter().map(|e| e.id).collect::<Vec<_>>(), [a, b, c]);
        assert!(inbox.drain_at_boundary().is_empty());
    }

    #[test]
    fn ids_are_never_reused_across_drains() {
        let inbox = LiveInbox::new();
        let first = offered(&inbox, "one");
        let _ = inbox.drain_at_boundary();
        let second = offered(&inbox, "two");
        assert!(second.0 > first.0);
    }

    #[test]
    fn remove_deletes_only_the_target_and_returns_its_content() {
        let inbox = LiveInbox::new();
        let _a = offered(&inbox, "a");
        let b = offered(&inbox, "b");
        let _c = offered(&inbox, "c");

        let removed = inbox.remove(b).expect("b is queued");
        assert_eq!(removed.text_content(), "b");
        assert_eq!(texts(&inbox.list()), ["a", "c"]);
    }

    #[test]
    fn edits_cannot_target_unknown_or_already_consumed_messages() {
        let inbox = LiveInbox::new();
        let a = offered(&inbox, "a");
        let _ = inbox.drain_at_boundary();

        assert_eq!(inbox.remove(a), Err(EditError::UnknownMessage));
        assert_eq!(inbox.replace(a, msg("a2")), Err(EditError::UnknownMessage));
        assert_eq!(
            inbox.remove(LiveInboxMessageId(999)),
            Err(EditError::UnknownMessage)
        );
    }

    #[test]
    fn replace_keeps_id_and_position() {
        let inbox = LiveInbox::new();
        let _a = offered(&inbox, "a");
        let b = offered(&inbox, "b");
        let _c = offered(&inbox, "c");

        inbox.replace(b, msg("b-edited")).expect("b is queued");
        let listed = inbox.list();
        assert_eq!(texts(&listed), ["a", "b-edited", "c"]);
        assert_eq!(listed[1].id, b);
    }

    #[test]
    fn reorder_applies_a_full_permutation() {
        let inbox = LiveInbox::new();
        let a = offered(&inbox, "a");
        let b = offered(&inbox, "b");
        let c = offered(&inbox, "c");

        inbox.reorder(&[c, a, b]).expect("valid permutation");
        assert_eq!(texts(&inbox.list()), ["c", "a", "b"]);
        assert_eq!(texts(&inbox.drain_at_boundary()), ["c", "a", "b"]);
    }

    #[test]
    fn reorder_rejects_stale_views_and_leaves_the_queue_untouched() {
        let inbox = LiveInbox::new();
        let a = offered(&inbox, "a");
        let b = offered(&inbox, "b");

        // Wrong length (stale after a concurrent offer the caller missed).
        assert_eq!(inbox.reorder(&[a]), Err(EditError::StaleOrder));
        // Unknown id.
        assert_eq!(
            inbox.reorder(&[a, LiveInboxMessageId(999)]),
            Err(EditError::StaleOrder)
        );
        // Duplicate id.
        assert_eq!(inbox.reorder(&[a, a]), Err(EditError::StaleOrder));
        // Every rejection left the original order intact.
        assert_eq!(texts(&inbox.list()), ["a", "b"]);
        let _ = b;
    }

    #[test]
    fn close_returns_leftovers_and_shuts_every_mutation_path() {
        let inbox = LiveInbox::new();
        let a = offered(&inbox, "a");
        let _b = offered(&inbox, "b");

        let leftovers = inbox.close();
        assert_eq!(texts(&leftovers), ["a", "b"]);
        assert!(inbox.is_closed());

        assert_eq!(inbox.offer(msg("late")), Offer::Closed);
        assert_eq!(inbox.remove(a), Err(EditError::Closed));
        assert_eq!(inbox.replace(a, msg("a2")), Err(EditError::Closed));
        assert_eq!(inbox.reorder(&[]), Err(EditError::Closed));
        assert!(inbox.drain_at_boundary().is_empty());
        assert!(inbox.list().is_empty());
        // Waking or re-closing a finished attempt is a normal race, not a fault.
        inbox.wake();
        assert!(inbox.close().is_empty());
    }

    #[tokio::test]
    async fn wake_wakes_a_waiter_without_queueing_content() {
        let inbox = LiveInbox::new();
        let seen = inbox.version();
        let waiter = {
            let inbox = inbox.clone();
            let cancel = CancellationToken::new();
            tokio::spawn(async move { inbox.wait_for_change(seen, &cancel).await })
        };
        // Let the waiter register before waking.
        tokio::task::yield_now().await;
        inbox.wake();
        let outcome = waiter.await.expect("waiter not cancelled");
        assert_eq!(outcome, WaitOutcome::Changed(inbox.version()));
        assert!(inbox.list().is_empty());
    }

    #[tokio::test]
    async fn change_between_snapshot_and_wait_is_not_missed() {
        let inbox = LiveInbox::new();
        let seen = inbox.version();
        // The bump lands before wait_for_change is even called — the version
        // check, not the notification, must catch it.
        let _ = offered(&inbox, "raced");
        let cancel = CancellationToken::new();
        let outcome = inbox.wait_for_change(seen, &cancel).await;
        assert!(matches!(outcome, WaitOutcome::Changed(_)));
    }

    #[tokio::test]
    async fn wait_for_change_honours_cancellation_first() {
        let inbox = LiveInbox::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        // Even with a pending change, cancellation wins.
        let _ = offered(&inbox, "pending");
        let outcome = inbox.wait_for_change(inbox.version(), &cancel).await;
        assert_eq!(outcome, WaitOutcome::Cancelled);

        let quiet = LiveInbox::new();
        let token = CancellationToken::new();
        let waiter = {
            let quiet = quiet.clone();
            let token = token.clone();
            tokio::spawn(async move { quiet.wait_for_change(quiet.version(), &token).await })
        };
        tokio::task::yield_now().await;
        token.cancel();
        assert_eq!(
            waiter.await.expect("waiter finished"),
            WaitOutcome::Cancelled
        );
    }

    #[tokio::test]
    async fn concurrent_offers_are_all_delivered_exactly_once() {
        let inbox = LiveInbox::new();
        let mut senders = Vec::new();
        for task in 0..8 {
            let inbox = inbox.clone();
            senders.push(tokio::spawn(async move {
                for n in 0..25 {
                    match inbox.offer(msg(&format!("t{task}-{n}"))) {
                        Offer::Accepted(_) => {}
                        Offer::Closed => panic!("inbox closed mid-test"),
                    }
                }
            }));
        }
        for sender in senders {
            sender.await.expect("sender task panicked");
        }

        let drained = inbox.drain_at_boundary();
        assert_eq!(drained.len(), 8 * 25);
        let mut ids: Vec<u64> = drained.iter().map(|e| e.id.0).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 8 * 25, "every id delivered exactly once");
    }

    #[tokio::test]
    async fn waiter_sees_close_as_a_change() {
        let inbox = LiveInbox::new();
        let seen = inbox.version();
        let waiter = {
            let inbox = inbox.clone();
            let cancel = CancellationToken::new();
            tokio::spawn(async move { inbox.wait_for_change(seen, &cancel).await })
        };
        tokio::task::yield_now().await;
        let leftovers = inbox.close();
        assert!(leftovers.is_empty());
        assert!(matches!(
            waiter.await.expect("waiter finished"),
            WaitOutcome::Changed(_)
        ));
    }
}
