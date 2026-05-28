//! Event-sink helpers for streaming child agent output through the parent.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use awaken_runtime_contract::contract::event::AgentEvent;
use awaken_runtime_contract::contract::event_sink::EventSink;

/// How [`StreamingPassthroughSink`] handles child [`AgentEvent::Error`] events.
///
/// The default is [`Self::WrapAsToolCallDelta`] so a child stream error stays
/// scoped to the parent tool call instead of looking like a fatal parent-run
/// error to consumers that treat `AgentEvent::Error` as terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildErrorForwarding {
    /// Drop child error events from the parent passthrough stream.
    Drop,
    /// Emit a human-readable child error as `ToolCallStreamDelta`.
    WrapAsToolCallDelta,
    /// Forward the child `AgentEvent::Error` unchanged.
    ///
    /// Use only when the parent event consumer explicitly understands that
    /// these errors come from the child stream and are not necessarily fatal
    /// to the parent run.
    ForwardRawParentError,
}

/// Sink that intercepts a child agent's [`AgentEvent::TextDelta`] events and
/// re-emits them on a parent sink as [`AgentEvent::ToolCallStreamDelta`]
/// keyed by the parent tool call id.
///
/// Non-text events are dropped by default except child [`AgentEvent::Error`],
/// which is emitted as a `ToolCallStreamDelta` message but not appended to
/// the accumulated text buffer. This keeps diagnostics visible in the parent
/// tool stream without mixing them into child-generated content. Use
/// [`Self::new_with_error_forwarding`] with
/// [`ChildErrorForwarding::ForwardRawParentError`] only when your event
/// consumer explicitly wants raw child errors on the parent sink.
///
/// Use this when the parent tool wants the child's tokens to look like its
/// own streaming output (e.g. generative UI agents whose text becomes a
/// component the parent renders).
pub struct StreamingPassthroughSink {
    call_id: String,
    tool_name: String,
    parent_sink: Arc<dyn EventSink>,
    buffer: Arc<Mutex<String>>,
    error_forwarding: ChildErrorForwarding,
}

impl StreamingPassthroughSink {
    /// Construct the sink and return a shared handle to its accumulated text.
    ///
    /// Child errors are wrapped as `ToolCallStreamDelta` by default so parent
    /// event consumers do not mistake them for fatal parent-run errors.
    pub fn new(
        call_id: String,
        tool_name: String,
        parent_sink: Arc<dyn EventSink>,
    ) -> (Self, Arc<Mutex<String>>) {
        Self::new_with_error_forwarding(
            call_id,
            tool_name,
            parent_sink,
            ChildErrorForwarding::WrapAsToolCallDelta,
        )
    }

    /// Construct the sink with an explicit child error forwarding policy.
    pub fn new_with_error_forwarding(
        call_id: String,
        tool_name: String,
        parent_sink: Arc<dyn EventSink>,
        error_forwarding: ChildErrorForwarding,
    ) -> (Self, Arc<Mutex<String>>) {
        let buffer = Arc::new(Mutex::new(String::new()));
        let sink = Self {
            call_id,
            tool_name,
            parent_sink,
            buffer: buffer.clone(),
            error_forwarding,
        };
        (sink, buffer)
    }

    fn child_error_delta(message: &str, code: Option<&str>) -> String {
        match code {
            Some(code) => format!("[child error {code}: {message}]"),
            None => format!("[child error: {message}]"),
        }
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
            AgentEvent::Error { message, code } => match self.error_forwarding {
                ChildErrorForwarding::Drop => {}
                ChildErrorForwarding::WrapAsToolCallDelta => {
                    let delta = Self::child_error_delta(message, code.as_deref());
                    self.parent_sink
                        .emit(AgentEvent::ToolCallStreamDelta {
                            id: self.call_id.clone(),
                            name: self.tool_name.clone(),
                            delta,
                        })
                        .await;
                }
                ChildErrorForwarding::ForwardRawParentError => {
                    self.parent_sink.emit(event).await;
                }
            },
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::contract::event_sink::VecEventSink;

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
    async fn wraps_error_events_as_tool_stream_by_default() {
        let parent = Arc::new(VecEventSink::new());
        let (sink, buffer) =
            StreamingPassthroughSink::new("call-1".into(), "render_ui".into(), parent.clone());

        sink.emit(AgentEvent::Error {
            message: "something broke".into(),
            code: Some("LLM_ERROR".into()),
        })
        .await;

        let events = parent.events();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ToolCallStreamDelta { id, name, delta } => {
                assert_eq!(id, "call-1");
                assert_eq!(name, "render_ui");
                assert_eq!(delta, "[child error LLM_ERROR: something broke]");
            }
            other => panic!("expected ToolCallStreamDelta, got: {other:?}"),
        }

        assert!(
            buffer.lock().await.is_empty(),
            "wrapped child error diagnostics should not pollute accumulated child text"
        );
    }

    #[tokio::test]
    async fn can_forward_raw_error_events_when_explicitly_requested() {
        let parent = Arc::new(VecEventSink::new());
        let (sink, _buffer) = StreamingPassthroughSink::new_with_error_forwarding(
            "call-1".into(),
            "render_ui".into(),
            parent.clone(),
            ChildErrorForwarding::ForwardRawParentError,
        );

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

    #[tokio::test]
    async fn can_drop_error_events_when_explicitly_requested() {
        let parent = Arc::new(VecEventSink::new());
        let (sink, _buffer) = StreamingPassthroughSink::new_with_error_forwarding(
            "call-1".into(),
            "render_ui".into(),
            parent.clone(),
            ChildErrorForwarding::Drop,
        );

        sink.emit(AgentEvent::Error {
            message: "something broke".into(),
            code: None,
        })
        .await;

        assert!(parent.events().is_empty(), "child errors should be dropped");
    }
}
