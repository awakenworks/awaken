//! Lightweight channel for delivering events to an agent's owner thread.
//!
//! `InboxSender` wraps an unbounded mpsc channel. Background tasks push
//! structured messages into the owning agent's inbox via [`InboxSender::send`].
//!
//! When the receiver has been dropped (agent run ended), `send()` invokes
//! an optional `on_closed` callback so infrastructure (e.g. mailbox) can
//! react — for example by enqueuing a wake dispatch for continuation.

use std::sync::Arc;

use awaken_runtime_contract::contract::message::{Message, Visibility};
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
        match self.tx.unbounded_send(msg) {
            Ok(()) => {
                let depth = self.tx.len();
                if depth > 0 && depth.is_multiple_of(Self::DEPTH_WARNING_THRESHOLD) {
                    tracing::warn!(depth, "inbox channel depth is high");
                }
                true
            }
            Err(e) => {
                if let Some(ref cb) = self.on_closed {
                    cb.closed(&e.into_inner());
                }
                false
            }
        }
    }

    const DEPTH_WARNING_THRESHOLD: usize = 256;

    /// Try to send a message without invoking the closed-channel fallback.
    ///
    /// Runtime control paths use this when the caller owns fallback policy
    /// itself, for example live user-input steering that should queue a
    /// durable dispatch if the active receiver is gone.
    pub fn try_send(&self, msg: serde_json::Value) -> bool {
        self.tx.unbounded_send(msg).is_ok()
    }

    /// Returns the number of messages currently buffered in the channel.
    pub fn len(&self) -> usize {
        self.tx.len()
    }

    /// Returns `true` when the channel buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.tx.is_empty()
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

    /// Async receive: wait for a message or cancellation.
    ///
    /// Returns `Some(msg)` when a message arrives, `None` if cancelled
    /// or all senders dropped.
    pub async fn recv_or_cancel(
        &mut self,
        cancel: Option<&crate::cancellation::CancellationToken>,
    ) -> Option<serde_json::Value> {
        use futures::StreamExt;
        tokio::select! {
            msg = self.rx.next() => msg,
            _ = async {
                match cancel {
                    Some(t) => t.cancelled().await,
                    None => std::future::pending().await,
                }
            } => None,
        }
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

/// Convert a structured inbox event into an internal user message.
pub fn inbox_event_message(json: &serde_json::Value) -> Message {
    let kind = json.get("kind").and_then(|k| k.as_str()).unwrap_or("event");
    let task_id = json
        .get("task_id")
        .and_then(|t| t.as_str())
        .unwrap_or("unknown");
    let text = format!(
        "<background-task-event kind=\"{kind}\" task_id=\"{task_id}\">\n{}\n</background-task-event>",
        json
    );
    let mut msg = Message::user(text);
    msg.visibility = Visibility::Internal;
    msg
}

/// Convert direct messages into a single inbox payload.
pub fn inbox_messages_payload(messages: Vec<Message>) -> serde_json::Value {
    serde_json::json!({
        "kind": "messages",
        "messages": messages,
    })
}

pub fn pending_boundary_wake_payload() -> serde_json::Value {
    serde_json::json!({
        "kind": "pending_boundary_wake",
    })
}

pub fn is_pending_boundary_wake_payload(json: &serde_json::Value) -> bool {
    json.get("kind").and_then(|kind| kind.as_str()) == Some("pending_boundary_wake")
}

/// Convert any inbox payload into messages for the owner agent.
///
/// Unknown payloads are treated as background-task events to preserve the
/// historical inbox behavior.
pub fn inbox_payload_messages(json: &serde_json::Value) -> Vec<Message> {
    if json.get("kind").and_then(|kind| kind.as_str()) == Some("messages")
        && let Some(values) = json
            .get("messages")
            .and_then(|messages| messages.as_array())
    {
        let messages = values
            .iter()
            .filter_map(|value| serde_json::from_value::<Message>(value.clone()).ok())
            .collect::<Vec<_>>();
        if !messages.is_empty() {
            return messages;
        }
    }
    vec![inbox_event_message(json)]
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
    fn try_send_does_not_invoke_closed_fallback() {
        struct Counter(AtomicUsize);
        impl OnInboxClosed for Counter {
            fn closed(&self, _msg: &serde_json::Value) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let (tx, rx) = inbox_channel_with_fallback(counter.clone());
        drop(rx);

        assert!(!tx.try_send(serde_json::json!("lost")));
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);
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

    #[test]
    fn inbox_event_message_uses_internal_user_semantics() {
        let msg = inbox_event_message(&serde_json::json!({
            "kind": "completed",
            "task_id": "bg_1",
            "result": {"ok": true}
        }));
        assert_eq!(
            msg.role,
            awaken_runtime_contract::contract::message::Role::User
        );
        assert_eq!(msg.visibility, Visibility::Internal);
        assert!(msg.text().contains("background-task-event"));
        assert!(msg.text().contains("bg_1"));
    }

    #[test]
    fn inbox_messages_payload_roundtrips_direct_messages() {
        let payload = inbox_messages_payload(vec![Message::user("live steering")]);
        let messages = inbox_payload_messages(&payload);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].role,
            awaken_runtime_contract::contract::message::Role::User
        );
        assert_eq!(messages[0].visibility, Visibility::All);
        assert_eq!(messages[0].text(), "live steering");
    }

    #[test]
    fn pending_boundary_wake_payload_is_detectable_and_not_a_message() {
        let payload = pending_boundary_wake_payload();
        assert!(is_pending_boundary_wake_payload(&payload));
        assert!(
            inbox_payload_messages(&payload)[0]
                .text()
                .contains("pending_boundary_wake")
        );
    }

    #[test]
    fn inbox_payload_messages_keeps_background_event_fallback() {
        let messages = inbox_payload_messages(&serde_json::json!({
            "kind": "completed",
            "task_id": "bg_2",
        }));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].visibility, Visibility::Internal);
        assert!(messages[0].text().contains("background-task-event"));
    }
}
