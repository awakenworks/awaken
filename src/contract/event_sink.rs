//! Streaming event sink for agent loop events.
//!
//! [`EventSink`] receives [`AgentEvent`]s as they occur during a run.
//! The default [`VecEventSink`] collects events in memory for backward
//! compatibility; production implementations can push to channels,
//! WebSocket, SSE, or any other transport.

use async_trait::async_trait;

use super::event::AgentEvent;

/// Receives agent events as they occur during a run.
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Called for each event during the agent loop.
    async fn emit(&self, event: AgentEvent);

    /// Called when the run completes. Optional cleanup.
    async fn close(&self) {}
}

/// Collects events in a `Vec` (default, for backward compatibility).
#[derive(Default)]
pub struct VecEventSink {
    events: std::sync::Mutex<Vec<AgentEvent>>,
}

impl VecEventSink {
    /// Create a new empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Take all collected events, leaving the buffer empty.
    pub fn take(&self) -> Vec<AgentEvent> {
        std::mem::take(&mut *self.events.lock().expect("event sink poisoned"))
    }

    /// Clone all collected events without clearing the buffer.
    pub fn events(&self) -> Vec<AgentEvent> {
        self.events.lock().expect("event sink poisoned").clone()
    }
}

#[async_trait]
impl EventSink for VecEventSink {
    async fn emit(&self, event: AgentEvent) {
        self.events.lock().expect("event sink poisoned").push(event);
    }
}

/// Discards all events (useful in tests where events are not needed).
pub struct NullEventSink;

#[async_trait]
impl EventSink for NullEventSink {
    async fn emit(&self, _event: AgentEvent) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::lifecycle::TerminationReason;
    use serde_json::json;

    fn sample_events() -> Vec<AgentEvent> {
        vec![
            AgentEvent::RunStart {
                thread_id: "t1".into(),
                run_id: "r1".into(),
                parent_run_id: None,
            },
            AgentEvent::TextDelta {
                delta: "hello".into(),
            },
            AgentEvent::RunFinish {
                thread_id: "t1".into(),
                run_id: "r1".into(),
                result: Some(json!({"response": "done"})),
                termination: TerminationReason::NaturalEnd,
            },
        ]
    }

    #[tokio::test]
    async fn vec_sink_collects_events_in_order() {
        let sink = VecEventSink::new();
        for event in sample_events() {
            sink.emit(event).await;
        }
        let collected = sink.take();
        assert_eq!(collected.len(), 3);
        assert!(matches!(&collected[0], AgentEvent::RunStart { .. }));
        assert!(matches!(&collected[1], AgentEvent::TextDelta { delta } if delta == "hello"));
        assert!(matches!(&collected[2], AgentEvent::RunFinish { .. }));
    }

    #[tokio::test]
    async fn vec_sink_take_clears_buffer() {
        let sink = VecEventSink::new();
        sink.emit(AgentEvent::StepEnd).await;
        assert_eq!(sink.take().len(), 1);
        assert!(sink.take().is_empty());
    }

    #[tokio::test]
    async fn vec_sink_events_returns_clone_without_clearing() {
        let sink = VecEventSink::new();
        sink.emit(AgentEvent::StepEnd).await;
        assert_eq!(sink.events().len(), 1);
        assert_eq!(sink.events().len(), 1);
    }

    #[tokio::test]
    async fn null_sink_does_not_panic() {
        let sink = NullEventSink;
        for event in sample_events() {
            sink.emit(event).await;
        }
        sink.close().await;
    }
}
