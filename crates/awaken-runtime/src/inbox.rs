//! Lightweight channel for delivering events to an agent's owner thread.
//!
//! `InboxSender` wraps an unbounded mpsc channel. Background tasks push
//! structured messages into the owning agent's inbox via [`InboxSender::send`].
//!
//! When the receiver has been dropped (agent run ended), `send()` invokes
//! an optional `on_closed` callback so infrastructure (e.g. mailbox) can
//! react — for example by enqueuing a wake job for continuation.

use std::sync::Arc;

use futures::channel::mpsc;

/// Callback invoked when [`InboxSender::send`] detects the receiver is gone.
///
/// Implementations should be cheap and idempotent — the callback may fire
/// multiple times if several tasks complete after the receiver is dropped.
pub trait OnInboxClosed: Send + Sync + 'static {
    fn closed(&self, message: &serde_json::Value);
}

/// Sending half of an agent inbox channel.
///
/// Cloneable and `Send + Sync` — background tasks receive a clone and can
/// fire-and-forget messages into the owner agent's inbox.
#[derive(Clone)]
pub struct InboxSender {
    tx: mpsc::UnboundedSender<serde_json::Value>,
    on_closed: Option<Arc<dyn OnInboxClosed>>,
}

impl std::fmt::Debug for InboxSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboxSender")
            .field("is_closed", &self.tx.is_closed())
            .finish()
    }
}

/// Receiving half of the inbox channel (held by the owner agent's loop).
#[derive(Debug)]
pub struct InboxReceiver {
    rx: mpsc::UnboundedReceiver<serde_json::Value>,
}

impl InboxSender {
    /// Send a message to the owner agent.
    ///
    /// Returns `true` if delivered to the channel. Returns `false` if
    /// the receiver was dropped — in that case `on_closed` (if set) is
    /// also invoked so the infrastructure layer can take action.
    pub fn send(&self, msg: serde_json::Value) -> bool {
        if self.tx.unbounded_send(msg.clone()).is_ok() {
            return true;
        }
        // Receiver gone — invoke fallback
        if let Some(ref cb) = self.on_closed {
            cb.closed(&msg);
        }
        false
    }

    /// Returns `true` when the receiving half has been dropped.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

impl InboxReceiver {
    /// Try to receive the next pending message without blocking.
    ///
    /// Returns `None` when the channel is empty (or all senders dropped).
    pub fn try_recv(&mut self) -> Option<serde_json::Value> {
        self.rx.try_recv().ok()
    }

    /// Drain all currently buffered messages into a `Vec`.
    pub fn drain(&mut self) -> Vec<serde_json::Value> {
        let mut msgs = Vec::new();
        while let Some(msg) = self.try_recv() {
            msgs.push(msg);
        }
        msgs
    }
}

/// Create a new `(InboxSender, InboxReceiver)` pair.
pub fn inbox_channel() -> (InboxSender, InboxReceiver) {
    let (tx, rx) = mpsc::unbounded();
    (
        InboxSender {
            tx,
            on_closed: None,
        },
        InboxReceiver { rx },
    )
}

/// Create a new `(InboxSender, InboxReceiver)` pair with an `on_closed`
/// callback. The callback fires when `send()` detects the receiver is gone.
pub fn inbox_channel_with_fallback(
    on_closed: Arc<dyn OnInboxClosed>,
) -> (InboxSender, InboxReceiver) {
    let (tx, rx) = mpsc::unbounded();
    (
        InboxSender {
            tx,
            on_closed: Some(on_closed),
        },
        InboxReceiver { rx },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn send_and_drain() {
        let (tx, mut rx) = inbox_channel();
        assert!(tx.send(serde_json::json!({"type": "progress", "pct": 50})));
        assert!(tx.send(serde_json::json!("done")));

        let msgs = rx.drain();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["type"], "progress");
        assert_eq!(msgs[1], "done");

        assert!(rx.try_recv().is_none());
    }

    #[test]
    fn sender_clone_is_independent() {
        let (tx1, mut rx) = inbox_channel();
        let tx2 = tx1.clone();
        assert!(tx1.send(serde_json::json!(1)));
        assert!(tx2.send(serde_json::json!(2)));

        let msgs = rx.drain();
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn is_closed_after_receiver_drop() {
        let (tx, rx) = inbox_channel();
        assert!(!tx.is_closed());
        drop(rx);
        assert!(tx.is_closed());
        assert!(!tx.send(serde_json::json!("lost")));
    }

    #[test]
    fn try_recv_returns_none_on_empty() {
        let (_tx, mut rx) = inbox_channel();
        assert!(rx.try_recv().is_none());
    }

    #[test]
    fn on_closed_fires_when_receiver_dropped() {
        struct Counter(AtomicUsize);
        impl OnInboxClosed for Counter {
            fn closed(&self, _msg: &serde_json::Value) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let (tx, rx) = inbox_channel_with_fallback(counter.clone());

        // Send succeeds while receiver is alive
        assert!(tx.send(serde_json::json!("ok")));
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);

        // Drop receiver
        drop(rx);

        // Send fails, on_closed fires
        assert!(!tx.send(serde_json::json!("lost")));
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);

        // Fires again on subsequent sends
        assert!(!tx.send(serde_json::json!("lost2")));
        assert_eq!(counter.0.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn no_on_closed_without_fallback() {
        let (tx, rx) = inbox_channel();
        drop(rx);
        // Should not panic — no callback set
        assert!(!tx.send(serde_json::json!("lost")));
    }
}
