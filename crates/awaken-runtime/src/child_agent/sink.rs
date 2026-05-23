//! Event-sink helpers for streaming child agent output through the parent.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;

/// Sink that intercepts a child agent's [`AgentEvent::TextDelta`] events and
/// re-emits them on a parent sink as [`AgentEvent::ToolCallStreamDelta`]
/// keyed by the parent tool call id.
///
/// Error events are forwarded as-is. All other events are dropped — the child
/// agent has its own event stream for full observability; this sink exists
/// purely to surface incremental text inside the parent tool's stream.
///
/// Use this when the parent tool wants the child's tokens to look like its
/// own streaming output (e.g. generative UI agents whose text becomes a
/// component the parent renders).
pub struct StreamingPassthroughSink {
    call_id: String,
    tool_name: String,
    parent_sink: Arc<dyn EventSink>,
    buffer: Arc<Mutex<String>>,
}

impl StreamingPassthroughSink {
    /// Construct the sink and return a shared handle to its accumulated text.
    pub fn new(
        call_id: String,
        tool_name: String,
        parent_sink: Arc<dyn EventSink>,
    ) -> (Self, Arc<Mutex<String>>) {
        let buffer = Arc::new(Mutex::new(String::new()));
        let sink = Self {
            call_id,
            tool_name,
            parent_sink,
            buffer: buffer.clone(),
        };
        (sink, buffer)
    }
}

#[async_trait]
impl EventSink for StreamingPassthroughSink {
    async fn emit(&self, event: AgentEvent) {
        match &event {
            AgentEvent::TextDelta { delta } => {
                self.buffer.lock().await.push_str(delta);
                self.parent_sink
                    .emit(AgentEvent::ToolCallStreamDelta {
                        id: self.call_id.clone(),
                        name: self.tool_name.clone(),
                        delta: delta.clone(),
                    })
                    .await;
            }
            AgentEvent::Error { .. } => {
                self.parent_sink.emit(event).await;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::event_sink::VecEventSink;

    #[tokio::test]
    async fn forwards_text_delta_as_tool_stream() {
        let parent = Arc::new(VecEventSink::new());
        let (sink, buffer) =
            StreamingPassthroughSink::new("call-1".into(), "render_ui".into(), parent.clone());

        sink.emit(AgentEvent::TextDelta {
            delta: "Hello".into(),
        })
        .await;
        sink.emit(AgentEvent::TextDelta {
            delta: " world".into(),
        })
        .await;

        let events = parent.events();
        assert_eq!(events.len(), 2);

        match &events[0] {
            AgentEvent::ToolCallStreamDelta { id, name, delta } => {
                assert_eq!(id, "call-1");
                assert_eq!(name, "render_ui");
                assert_eq!(delta, "Hello");
            }
            other => panic!("expected ToolCallStreamDelta, got: {other:?}"),
        }

        match &events[1] {
            AgentEvent::ToolCallStreamDelta { delta, .. } => {
                assert_eq!(delta, " world");
            }
            other => panic!("expected ToolCallStreamDelta, got: {other:?}"),
        }

        let accumulated = buffer.lock().await.clone();
        assert_eq!(accumulated, "Hello world");
    }

    #[tokio::test]
    async fn forwards_error_events() {
        let parent = Arc::new(VecEventSink::new());
        let (sink, _buffer) =
            StreamingPassthroughSink::new("call-1".into(), "render_ui".into(), parent.clone());

        sink.emit(AgentEvent::Error {
            message: "something broke".into(),
            code: Some("LLM_ERROR".into()),
        })
        .await;

        let events = parent.events();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::Error { message, code } => {
                assert_eq!(message, "something broke");
                assert_eq!(code.as_deref(), Some("LLM_ERROR"));
            }
            other => panic!("expected Error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn drops_other_events() {
        let parent = Arc::new(VecEventSink::new());
        let (sink, _buffer) =
            StreamingPassthroughSink::new("call-1".into(), "render_ui".into(), parent.clone());

        sink.emit(AgentEvent::StepStart {
            message_id: "m1".into(),
        })
        .await;
        sink.emit(AgentEvent::StepEnd).await;

        let events = parent.events();
        assert!(events.is_empty(), "non-text/error events should be dropped");
    }
}
