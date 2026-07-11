//! Per-attempt live-inbox lifecycle: open with carry-over, close with
//! leftover capture, and the in-flight lookup wire handlers use.
//!
//! Only the native direct turn path opens an inbox (see `turn_exec`): the
//! ACP executor never drains one, and the durable path runs in the worker's
//! process. The slot lives on `SessionCtx` with the same locking discipline
//! as the cancel token — brief std locks, never held across an await.

use awaken_runtime_contract::live_inbox::{LiveInbox, LiveInboxMessage};

use crate::host::{SessionCtx, SharedHost};

/// The per-thread live-inbox slot. `open` is `Some` only while a native
/// direct turn is in flight; `leftovers` carries messages queued but not
/// consumed when an attempt closed, seeded into the next attempt's inbox so
/// a queued message survives the turn boundary instead of dying with it. The
/// full [`LiveInboxMessage`] is kept (not just its content) so an entry's
/// [`MessageOrigin`](awaken_runtime_contract::live_inbox::MessageOrigin) — e.g.
/// an out-of-band External injection — is not relaundered to Run across the
/// attempt boundary.
#[derive(Default)]
pub(crate) struct LiveInboxSlot {
    open: Option<LiveInbox>,
    leftovers: Vec<LiveInboxMessage>,
}

impl SessionCtx {
    /// Open a fresh live inbox for the attempt about to run, seeded with the
    /// previous attempt's unconsumed leftovers. Registered on this ctx so a
    /// concurrent wire request can list/edit the in-flight queue.
    pub(crate) fn open_live_inbox(&self) -> LiveInbox {
        let inbox = LiveInbox::new();
        let mut slot = self.live_inbox.lock().expect("live-inbox slot poisoned");
        for entry in slot.leftovers.drain(..) {
            // A freshly created inbox is never closed; the offer cannot fail.
            // Re-offer with the entry's original origin so provenance carries over.
            let _ = inbox.offer_as(entry.origin, entry.message);
        }
        slot.open = Some(inbox.clone());
        inbox
    }

    /// Close the attempt's inbox and keep whatever the run did not consume
    /// for the next attempt on this thread.
    pub(crate) fn close_live_inbox(&self) {
        let mut slot = self.live_inbox.lock().expect("live-inbox slot poisoned");
        if let Some(inbox) = slot.open.take() {
            // Keep the whole entry (origin included) for the next attempt.
            slot.leftovers.extend(inbox.close());
        }
    }

    /// The in-flight attempt's live inbox; `None` when no native turn is running.
    pub(crate) fn live_inbox(&self) -> Option<LiveInbox> {
        self.live_inbox
            .lock()
            .expect("live-inbox slot poisoned")
            .open
            .clone()
    }
}

impl SharedHost {
    /// The in-flight live inbox for `thread`, if a native turn is currently
    /// running. A pure lookup — never materializes a session — so wire
    /// handlers can probe without side effects.
    pub async fn live_inbox(&self, thread: &str) -> Option<LiveInbox> {
        let sessions = self.sessions.lock().await;
        sessions.get(thread)?.live_inbox()
    }
}
