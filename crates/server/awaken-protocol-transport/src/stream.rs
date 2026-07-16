//! A reusable [`StreamSink`] adapter that forwards the engine's best-effort live
//! progress onto an mpsc channel a streaming adapter drains.
//!
//! This is protocol-neutral: it carries the neutral `AgentEvent` vocabulary, so
//! every wire adapter (AI SDK, AG-UI) drains the same channel and transcodes with
//! its own vocabulary. It is a second adapter of the `StreamSink` port alongside
//! the in-memory one — the engine is untouched, it just emits to whatever sink the
//! edge injects.

use async_trait::async_trait;
use awaken_agent_contract::event::AgentEvent;
use awaken_agent_contract::stream::event::Event;
use awaken_agent_contract::stream::sink::{Error as SinkError, Sink as StreamSink};
use tokio::sync::mpsc::UnboundedSender;

/// Forwards each live event's neutral `AgentEvent` onto an mpsc channel. Best-effort
/// by contract: once the receiver is dropped, `send` reports `Closed` and the engine
/// swallows it (the committed step stays authoritative, G10/G13).
pub struct ChannelStreamSink {
    tx: UnboundedSender<AgentEvent>,
}

impl ChannelStreamSink {
    pub fn new(tx: UnboundedSender<AgentEvent>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl StreamSink for ChannelStreamSink {
    async fn send(&self, event: Event) -> Result<(), SinkError> {
        self.tx.send(event.kind).map_err(|_| SinkError::Closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::Id as RunId;
    use tokio::sync::mpsc;

    use awaken_agent_contract::event::{Delta, Fact};

    #[tokio::test]
    async fn forwards_event_kind() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = ChannelStreamSink::new(tx);
        sink.send(Event {
            run_id: RunId("r1".into()),
            kind: AgentEvent::Delta(Delta::TextDelta { delta: "hi".into() }),
        })
        .await
        .unwrap();
        assert_eq!(
            rx.recv().await,
            Some(AgentEvent::Delta(Delta::TextDelta { delta: "hi".into() }))
        );
    }

    #[tokio::test]
    async fn closed_after_receiver_dropped() {
        let (tx, rx) = mpsc::unbounded_channel();
        let sink = ChannelStreamSink::new(tx);
        drop(rx);
        let err = sink
            .send(Event {
                run_id: RunId("r1".into()),
                kind: AgentEvent::Fact(Fact::RunFinished { exhausted: false }),
            })
            .await;
        assert!(matches!(err, Err(SinkError::Closed)));
    }
}
