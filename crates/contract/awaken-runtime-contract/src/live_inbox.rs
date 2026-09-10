//! Live inbox: the input-direction mirror of the stream sink (G10/G13's
//! best-effort side). An in-process, editable queue of messages addressed to a
//! running attempt, consumed only at safe loop boundaries.
//!
//! A run's inputs are otherwise fixed at activation and reopened only by
//! resume after an await. The live inbox is the third input surface — "inject
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
//! durable pending-input path stays the at-least-once channel for awaiting runs.
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
    /// The run's own attempt-local input — for example, a sub-run's
    /// background-task callback. The default. Entries never cross an attempt
    /// boundary through this process-local queue.
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum InboxLifecycle {
    #[default]
    Open,
    Closed,
}

impl InboxLifecycle {
    /// Sole admission and lifecycle transition for process-local inbox edits.
    /// Once closed, no mutation can reopen or change the inbox.
    const fn admits_mutation(self) -> bool {
        matches!(self, Self::Open)
    }

    const fn close(self) -> Self {
        Self::Closed
    }
}

#[derive(Default)]
struct State {
    entries: VecDeque<LiveInboxMessage>,
    next_id: u64,
    version: u64,
    lifecycle: InboxLifecycle,
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

    fn next_message_id(current: u64) -> Option<LiveInboxMessageId> {
        current.checked_add(1).map(LiveInboxMessageId)
    }

    /// Reorder only after validating the complete request.  In particular, an
    /// invalid suffix must not leave behind the valid-but-reordered prefix that
    /// was inspected before it.  The helper is generic so the exact production
    /// transition can also be model-checked with small symbolic identities.
    fn reorder_exactly<T, K: Copy + Eq>(
        entries: &mut VecDeque<T>,
        order: &[K],
        key: impl Fn(&T) -> K,
    ) -> bool {
        if entries.len() != order.len()
            || entries.iter().any(|entry| {
                let id = key(entry);
                order.iter().filter(|candidate| **candidate == id).count() != 1
            })
            || order
                .iter()
                .any(|id| entries.iter().filter(|entry| key(entry) == *id).count() != 1)
        {
            return false;
        }

        let mut remaining = std::mem::take(entries);
        for id in order {
            let index = remaining
                .iter()
                .position(|entry| key(entry) == *id)
                .expect("validated reorder identity");
            entries.push_back(remaining.remove(index).expect("validated reorder index"));
        }
        true
    }

    /// Replace one payload selected by its stable key. This is the sole queue
    /// mutation used by [`Self::replace`], so selection, peer preservation and
    /// the miss-as-stutter rule cannot drift apart from the checked kernel.
    fn replace_payload_exactly<T, K: Copy + Eq, V>(
        entries: &mut VecDeque<T>,
        id: K,
        key: impl Fn(&T) -> K,
        payload: impl Fn(&mut T) -> &mut V,
        replacement: V,
    ) -> bool {
        let Some(entry) = entries.iter_mut().find(|entry| key(entry) == id) else {
            return false;
        };
        *payload(entry) = replacement;
        true
    }

    /// Move the complete FIFO queue across a boundary exactly once. Boundary
    /// drain and close share this owner instead of maintaining parallel
    /// drain-to-`Vec` implementations.
    fn take_all_in_order<T>(entries: &mut VecDeque<T>) -> Vec<T> {
        entries.drain(..).collect()
    }

    /// Queue a message for the run as the run's own input
    /// ([`MessageOrigin::Run`]). Best-effort: `Closed` means this inbox can no
    /// longer issue an identity (the attempt closed or the finite identity
    /// space was exhausted), and the sender must decide (durable fallback or
    /// drop).
    pub fn offer(&self, message: Message) -> Offer {
        self.offer_as(MessageOrigin::Run, message)
    }

    /// Queue a message tagged with its [`MessageOrigin`]. The host's
    /// queued-message surface uses [`MessageOrigin::External`] for an out-of-band
    /// injection so a consumer can tell it apart from the run's own input.
    pub fn offer_as(&self, origin: MessageOrigin, message: Message) -> Offer {
        let mut state = self.state();
        if !state.lifecycle.admits_mutation() {
            return Offer::Closed;
        }
        let Some(id) = Self::next_message_id(state.next_id) else {
            // The identity space is finite in the implementation. Refuse the
            // offer without changing the queue instead of wrapping and
            // resurrecting an already-retired identity.
            return Offer::Closed;
        };
        state.next_id = id.0;
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
        if !state.lifecycle.admits_mutation() {
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
        if !state.lifecycle.admits_mutation() {
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
        if !state.lifecycle.admits_mutation() {
            return Err(EditError::Closed);
        }
        if !Self::replace_payload_exactly(
            &mut state.entries,
            id,
            |entry| entry.id,
            |entry| &mut entry.message,
            message,
        ) {
            return Err(EditError::UnknownMessage);
        }
        Self::bump_and_notify(&mut state, &self.shared.notify);
        Ok(())
    }

    /// Reorder the queue to exactly `order`, which must be a permutation of
    /// the current ids. Anything else is a stale view of the queue
    /// (a concurrent offer, remove, or drain won the race): the caller
    /// re-lists and retries; the queue is left untouched.
    pub fn reorder(&self, order: &[LiveInboxMessageId]) -> Result<(), EditError> {
        let mut state = self.state();
        if !state.lifecycle.admits_mutation() {
            return Err(EditError::Closed);
        }
        if !Self::reorder_exactly(&mut state.entries, order, |entry| entry.id) {
            return Err(EditError::StaleOrder);
        }
        Self::bump_and_notify(&mut state, &self.shared.notify);
        Ok(())
    }

    /// Take everything queued, in order. The point of no return: consumed
    /// messages leave the editable window, and the engine folds them into
    /// the transcript, where the commit — not the queue — makes them true.
    pub fn drain_at_boundary(&self) -> Vec<LiveInboxMessage> {
        Self::take_all_in_order(&mut self.state().entries)
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
        if !state.lifecycle.admits_mutation() {
            return Vec::new();
        }
        state.lifecycle = state.lifecycle.close();
        let leftovers = Self::take_all_in_order(&mut state.entries);
        Self::bump_and_notify(&mut state, &self.shared.notify);
        leftovers
    }

    pub fn is_closed(&self) -> bool {
        self.state().lifecycle == InboxLifecycle::Closed
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
    fn retired_ids_are_never_reissued_after_remove_or_drain() {
        // Causes: C1 an issued id is retired by remove; C2 an issued id is
        // retired by boundary drain; C3 the identity frontier is exhausted.
        // Effects: E1/E2 every later accepted id is strictly beyond both
        // retired ids; E3 exhaustion rejects without wrapping or changing the
        // queue/frontier. Close is covered by the lifecycle table because it
        // absorbs every later offer rather than issuing another id.
        //
        // | Rule | retirement/exhaustion | later offer | effect |
        // |---|---|---|---|
        // | I1 | remove(id) | accepted | new id > retired id |
        // | I2 | drain(id) | accepted | new id > retired id |
        // | I3 | frontier = u64::MAX | rejected | exact state stutter |
        let inbox = LiveInbox::new();
        let first = offered(&inbox, "one");
        let second = offered(&inbox, "two");
        inbox.remove(first).expect("I1 retires first id");
        let drained = inbox.drain_at_boundary();
        assert_eq!(drained[0].id, second, "I2 retires second id");

        let third = offered(&inbox, "three");
        let fourth = offered(&inbox, "four");
        assert!(third.0 > first.0 && third.0 > second.0, "I1+I2/E1+E2");
        assert!(fourth.0 > third.0, "I1+I2/E1+E2 monotonic frontier");
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
    fn replace_decision_table_changes_only_the_target_payload() {
        // Causes: C1 the open queue contains the requested id; C2 it does not.
        // Effects: E1 a hit replaces only that entry's Message, preserving its
        // id, origin, position, and both complete peers, then advances version
        // once; E2 a miss returns UnknownMessage and the complete queue/version
        // stutter. Closed admission is owned by the adjacent lifecycle table.
        //
        // | Rule | Open | Target | Result | Queue/version effect |
        // |---|---|---|---|---|
        // | R1 | yes | absent | UnknownMessage | exact stutter |
        // | R2 | yes | present | Ok | target payload only; version + 1 |
        let inbox = LiveInbox::new();
        let _a = offered(&inbox, "a");
        let b = match inbox.offer_as(MessageOrigin::External, msg("b")) {
            Offer::Accepted(id) => id,
            Offer::Closed => panic!("inbox unexpectedly closed"),
        };
        let _c = offered(&inbox, "c");

        let before = inbox.list();
        let version = inbox.version();
        assert_eq!(
            inbox.replace(LiveInboxMessageId(999), msg("missing")),
            Err(EditError::UnknownMessage),
            "R1"
        );
        assert_eq!(inbox.list(), before, "R1 queue stutter");
        assert_eq!(inbox.version(), version, "R1 version stutter");

        inbox.replace(b, msg("b-edited")).expect("b is queued");
        let listed = inbox.list();
        assert_eq!(texts(&listed), ["a", "b-edited", "c"]);
        assert_eq!(listed[0], before[0], "R2 leading peer");
        assert_eq!(listed[2], before[2], "R2 trailing peer");
        assert_eq!(listed[1].id, before[1].id, "R2 identity");
        assert_eq!(listed[1].origin, before[1].origin, "R2 provenance");
        assert_eq!(inbox.version(), version + 1, "R2 version");
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
        let c = offered(&inbox, "c");

        // Wrong length (stale after a concurrent offer the caller missed).
        assert_eq!(inbox.reorder(&[a, b]), Err(EditError::StaleOrder));
        // A reordered valid prefix followed by an unknown id used to expose a
        // check-while-mutating bug: the call returned StaleOrder but left
        // [c, a, b] behind. Rejection must be an atomic stutter instead.
        assert_eq!(
            inbox.reorder(&[c, LiveInboxMessageId(999), b]),
            Err(EditError::StaleOrder)
        );
        // Duplicate id.
        assert_eq!(inbox.reorder(&[c, c, a]), Err(EditError::StaleOrder));
        // Every rejection left the original order intact.
        assert_eq!(texts(&inbox.list()), ["a", "b", "c"]);
    }

    #[test]
    fn exhausted_identity_space_refuses_offer_without_reusing_an_id() {
        let inbox = LiveInbox::new();
        inbox.state().next_id = u64::MAX;

        assert_eq!(inbox.offer(msg("cannot-wrap")), Offer::Closed);
        assert!(inbox.list().is_empty());
        assert_eq!(inbox.state().next_id, u64::MAX);
    }

    #[test]
    fn close_returns_leftovers_and_shuts_every_mutation_path() {
        // Causes: C1 first close with two queued messages; C2 each post-close
        // operation (Offer, Wake, Remove, Replace, Reorder, Close). Effects:
        // L1 first Close returns the complete FIFO queue and enters Closed;
        // L2 every later operation rejects or returns empty; L3 queue, identity
        // frontier, and version remain unchanged after closure.
        //
        // | Rule | Lifecycle | Operation | Result/state effect |
        // |---|---|---|---|
        // | L1 | Open | Close | FIFO leftovers, Closed, version + 1 |
        // | L2 | Closed | Offer | Closed, exact state stutter |
        // | L3 | Closed | Wake | exact state stutter |
        // | L4 | Closed | Remove/Replace/Reorder | Closed error, state stutter |
        // | L5 | Closed | Close | empty leftovers, state stutter |
        let inbox = LiveInbox::new();
        let a = offered(&inbox, "a");
        let _b = offered(&inbox, "b");

        let leftovers = inbox.close();
        assert_eq!(texts(&leftovers), ["a", "b"]);
        assert!(inbox.is_closed());
        let closed_version = inbox.version();
        let closed_next_id = inbox.state().next_id;

        assert_eq!(inbox.offer(msg("late")), Offer::Closed);
        assert_eq!(inbox.remove(a), Err(EditError::Closed));
        assert_eq!(inbox.replace(a, msg("a2")), Err(EditError::Closed));
        assert_eq!(inbox.reorder(&[]), Err(EditError::Closed));
        assert!(inbox.drain_at_boundary().is_empty());
        assert!(inbox.list().is_empty());
        // Waking or re-closing a finished attempt is a normal race, not a fault.
        inbox.wake();
        assert!(inbox.close().is_empty());
        assert_eq!(inbox.version(), closed_version, "L2-L5 version stutter");
        assert_eq!(inbox.state().next_id, closed_next_id, "L2-L5 id stutter");
        assert!(inbox.list().is_empty(), "L2-L5 queue stutter");
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
    async fn offer_racing_close_has_one_exact_owner_for_the_message() {
        // Cause/effect decision table: offer and close start concurrently.
        // R1 offer linearizes first -> Accepted and close returns that one
        // leftover; R2 close linearizes first -> offer reports Closed and close
        // returns no leftover. Constraint: the message is owned by exactly one
        // outcome and can be neither duplicated nor silently accepted after
        // the attempt closed.
        for _ in 0..64 {
            let inbox = LiveInbox::new();
            let barrier = Arc::new(tokio::sync::Barrier::new(3));
            let offer = {
                let inbox = inbox.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    inbox.offer(msg("raced"))
                })
            };
            let close = {
                let inbox = inbox.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    inbox.close()
                })
            };
            barrier.wait().await;
            let offered = offer.await.expect("offer task");
            let leftovers = close.await.expect("close task");
            match offered {
                Offer::Accepted(id) => {
                    assert_eq!(leftovers.len(), 1, "R1");
                    assert_eq!(leftovers[0].id, id, "R1 exact owner");
                }
                Offer::Closed => assert!(leftovers.is_empty(), "R2"),
            }
            assert!(inbox.is_closed());
            assert!(inbox.list().is_empty());
        }
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

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn live_inbox_closed_lifecycle_is_absorbing_for_every_mutation() {
        let lifecycle = if kani::any::<bool>() {
            InboxLifecycle::Open
        } else {
            InboxLifecycle::Closed
        };
        let next = lifecycle.close();

        assert_eq!(
            lifecycle.admits_mutation(),
            matches!(lifecycle, InboxLifecycle::Open)
        );
        if matches!(lifecycle, InboxLifecycle::Closed) {
            assert_eq!(next, InboxLifecycle::Closed);
            assert!(!next.admits_mutation());
        }
        assert_eq!(next, InboxLifecycle::Closed);
        assert_eq!(next.close(), InboxLifecycle::Closed);
    }

    #[kani::proof]
    #[kani::unwind(8)]
    fn rejected_live_inbox_reorder_is_an_atomic_stutter() {
        let mut entries = VecDeque::from([kani::any::<u8>(), kani::any::<u8>(), kani::any::<u8>()]);
        let requested = [kani::any::<u8>(), kani::any::<u8>(), kani::any::<u8>()];
        let before = entries.clone();

        let accepted = LiveInbox::reorder_exactly(&mut entries, &requested, |value| *value);
        if accepted {
            assert_eq!(entries.iter().copied().collect::<Vec<_>>(), requested);
        } else {
            assert_eq!(entries, before);
        }
    }

    #[kani::proof]
    fn live_inbox_identity_advances_strictly_or_exhausts() {
        let current = kani::any::<u64>();
        match LiveInbox::next_message_id(current) {
            Some(next) => {
                assert!(next.0 > current);
                assert_eq!(next.0, current + 1);
            }
            None => assert_eq!(current, u64::MAX),
        }
    }

    #[kani::proof]
    fn retired_live_inbox_identity_is_never_reissued() {
        // Causes: R1 retired <= current frontier and current < MAX; R2 retired
        // <= current = MAX; R3 retired > current (not a reachable State
        // invariant). Effects: R1 issues strictly above retired; R2 refuses;
        // R3 makes no claim. These three rules partition the complete u64 pair
        // domain without an assumption or reduced symbolic range.
        let retired = kani::any::<u64>();
        let current = kani::any::<u64>();

        // Every retired identity was issued at or before the current private
        // frontier. Express the complete u64 relation as an implication rather
        // than constraining the symbolic domain with an assumption.
        if retired <= current {
            match LiveInbox::next_message_id(current) {
                Some(next) => {
                    assert!(next.0 > retired);
                    assert_ne!(next.0, retired);
                }
                None => assert_eq!(current, u64::MAX),
            }
        }
    }

    #[kani::proof]
    fn live_inbox_replace_preserves_identity_and_order() {
        // Causes: P1 target is key 1/2/3; P2 target is every other u8. Effects:
        // P1 changes only that position's payload while all keys, provenance,
        // peers and length remain; P2 rejects and exactly stutters. The tuple
        // fields are the production helper's key, provenance and payload axes;
        // the three hit positions plus complete miss partition cover its rules.
        let mut entries = VecDeque::from([
            (1_u8, kani::any::<u8>(), kani::any::<u8>()),
            (2_u8, kani::any::<u8>(), kani::any::<u8>()),
            (3_u8, kani::any::<u8>(), kani::any::<u8>()),
        ]);
        let target = kani::any::<u8>();
        let replacement = kani::any::<u8>();
        let before = entries.clone();

        let replaced = LiveInbox::replace_payload_exactly(
            &mut entries,
            target,
            |entry| entry.0,
            |entry| &mut entry.2,
            replacement,
        );
        let expected_hit = (1..=3).contains(&target);
        assert_eq!(replaced, expected_hit);
        assert_eq!(entries.len(), before.len());
        for index in 0..3 {
            assert_eq!(entries[index].0, before[index].0);
            assert_eq!(entries[index].1, before[index].1);
            if entries[index].0 == target {
                assert_eq!(entries[index].2, replacement);
            } else {
                assert_eq!(entries[index].2, before[index].2);
            }
        }
        if !expected_hit {
            assert_eq!(entries, before);
        }
    }

    #[kani::proof]
    fn live_inbox_boundary_drain_retires_exactly_the_queue() {
        // Cause: D1 the bounded non-empty queue contains three arbitrary
        // values. Effects: D1 returns those values once in exact FIFO order and
        // leaves no editable entry. Empty-queue behavior is the same transition
        // with zero iterations and is cross-checked by native tests.
        let mut entries = VecDeque::from([kani::any::<u8>(), kani::any::<u8>(), kani::any::<u8>()]);
        let before = entries.iter().copied().collect::<Vec<_>>();
        let drained = LiveInbox::take_all_in_order(&mut entries);

        assert_eq!(drained, before);
        assert!(entries.is_empty());
    }
}
