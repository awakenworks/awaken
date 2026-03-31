//! Channel-based event sink that bridges AgentRuntime to SSE relay.

use async_trait::async_trait;
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;
use tokio::sync::mpsc;

/// An EventSink that forwards events to an mpsc channel.
pub struct ChannelEventSink {
    tx: mpsc::UnboundedSender<AgentEvent>,
}

impl ChannelEventSink {
    pub fn new(tx: mpsc::UnboundedSender<AgentEvent>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl EventSink for ChannelEventSink {
    async fn emit(&self, event: AgentEvent) {
        let _ = self.tx.send(event);
    }

    async fn close(&self) {
        // Dropping sender will close the channel
    }
}

/// An EventSink whose underlying channel can be swapped at runtime.
///
/// This enables SSE reconnection: when a suspended run resumes via a new HTTP
/// request, the handler creates a fresh `(event_tx, event_rx)` pair and calls
/// `reconnect(new_tx)`. Subsequent events flow to the new receiver without
/// interrupting the backend run.
pub struct ReconnectableEventSink {
    inner: std::sync::Arc<tokio::sync::Mutex<mpsc::UnboundedSender<AgentEvent>>>,
}

impl ReconnectableEventSink {
    pub fn new(tx: mpsc::UnboundedSender<AgentEvent>) -> Self {
        Self {
            inner: std::sync::Arc::new(tokio::sync::Mutex::new(tx)),
        }
    }

    /// Replace the underlying sender. Future `emit()` calls go to `new_tx`.
    pub async fn reconnect(&self, new_tx: mpsc::UnboundedSender<AgentEvent>) {
        *self.inner.lock().await = new_tx;
    }
}

#[async_trait]
impl EventSink for ReconnectableEventSink {
    async fn emit(&self, event: AgentEvent) {
        let _ = self.inner.lock().await.send(event);
    }

    async fn close(&self) {}
}

/// An EventSink backed by a bounded mpsc channel.
///
/// Suitable for NATS transport where back-pressure is desired.
pub struct BoundedChannelEventSink {
    tx: mpsc::Sender<AgentEvent>,
}

impl BoundedChannelEventSink {
    pub fn new(tx: mpsc::Sender<AgentEvent>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl EventSink for BoundedChannelEventSink {
    async fn emit(&self, event: AgentEvent) {
        let _ = self.tx.send(event).await;
    }

    async fn close(&self) {
        // Dropping sender will close the channel
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::event::AgentEvent;

    #[tokio::test]
    async fn channel_sink_forwards_events() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = ChannelEventSink::new(tx);

        sink.emit(AgentEvent::TextDelta {
            delta: "hello".into(),
        })
        .await;
        sink.emit(AgentEvent::StepEnd).await;

        let event1 = rx.recv().await.unwrap();
        assert!(matches!(event1, AgentEvent::TextDelta { delta } if delta == "hello"));

        let event2 = rx.recv().await.unwrap();
        assert!(matches!(event2, AgentEvent::StepEnd));
    }

    #[tokio::test]
    async fn channel_sink_drops_silently_on_closed_receiver() {
        let (tx, rx) = mpsc::unbounded_channel();
        let sink = ChannelEventSink::new(tx);
        drop(rx);

        // Should not panic
        sink.emit(AgentEvent::TextDelta {
            delta: "ignored".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn channel_sink_close_is_noop() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let sink = ChannelEventSink::new(tx);
        sink.close().await; // Should not panic
    }

    #[tokio::test]
    async fn reconnectable_sink_forwards_events() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = ReconnectableEventSink::new(tx);

        sink.emit(AgentEvent::TextDelta {
            delta: "hello".into(),
        })
        .await;

        let event = rx.recv().await.unwrap();
        assert!(matches!(event, AgentEvent::TextDelta { delta } if delta == "hello"));
    }

    #[tokio::test]
    async fn reconnectable_sink_switches_channel() {
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        let sink = ReconnectableEventSink::new(tx1);

        // Event 1 goes to rx1
        sink.emit(AgentEvent::TextDelta {
            delta: "before".into(),
        })
        .await;

        // Swap to tx2
        sink.reconnect(tx2).await;

        // Event 2 goes to rx2
        sink.emit(AgentEvent::TextDelta {
            delta: "after".into(),
        })
        .await;

        let e1 = rx1.recv().await.unwrap();
        assert!(matches!(e1, AgentEvent::TextDelta { delta } if delta == "before"));

        let e2 = rx2.recv().await.unwrap();
        assert!(matches!(e2, AgentEvent::TextDelta { delta } if delta == "after"));

        // rx1 should have no more events
        assert!(rx1.try_recv().is_err());
    }

    #[tokio::test]
    async fn reconnectable_sink_survives_dropped_receiver() {
        let (tx, rx) = mpsc::unbounded_channel();
        let sink = ReconnectableEventSink::new(tx);
        drop(rx);

        // Should not panic — silent drop
        sink.emit(AgentEvent::StepEnd).await;

        // Reconnect to a fresh channel
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        sink.reconnect(tx2).await;

        sink.emit(AgentEvent::StepEnd).await;
        assert!(rx2.recv().await.is_some());
    }

    #[tokio::test]
    async fn reconnectable_sink_multiple_reconnects() {
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let sink = ReconnectableEventSink::new(tx1);

        sink.emit(AgentEvent::StepEnd).await;
        assert!(rx1.recv().await.is_some());

        // Reconnect twice — simulates Turn 2 then Turn 3
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        sink.reconnect(tx2).await;
        sink.emit(AgentEvent::StepEnd).await;
        assert!(rx2.recv().await.is_some());
        assert!(rx1.try_recv().is_err()); // rx1 gets nothing

        let (tx3, mut rx3) = mpsc::unbounded_channel();
        sink.reconnect(tx3).await;
        sink.emit(AgentEvent::StepEnd).await;
        assert!(rx3.recv().await.is_some());
        assert!(rx2.try_recv().is_err()); // rx2 gets nothing
    }

    #[tokio::test]
    async fn reconnectable_sink_shared_via_arc() {
        use std::sync::Arc;
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let sink = Arc::new(ReconnectableEventSink::new(tx1));

        // Emit from one clone
        let s1 = Arc::clone(&sink);
        s1.emit(AgentEvent::StepEnd).await;
        assert!(rx1.recv().await.is_some());

        // Reconnect from another clone
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        sink.reconnect(tx2).await;

        // Emit from the first clone — goes to new channel
        s1.emit(AgentEvent::StepEnd).await;
        assert!(rx2.recv().await.is_some());
    }
}
