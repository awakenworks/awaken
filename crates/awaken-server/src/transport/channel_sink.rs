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
}
