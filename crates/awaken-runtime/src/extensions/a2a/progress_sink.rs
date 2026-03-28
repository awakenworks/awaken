//! Filtering event sink that forwards only tool-call progress from direct child tool calls.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::progress::{
    TOOL_CALL_PROGRESS_ACTIVITY_TYPE, ToolCallProgressState,
};

/// An EventSink that intercepts child agent events and selectively forwards
/// tool-call progress to the parent's sink.
///
/// Tracks `ToolCallStart` events to build a set of known direct child call IDs.
/// When a `tool-call-progress` ActivitySnapshot arrives, checks if its `call_id`
/// is in the known set (i.e., a direct child, not a grandchild).
/// Only forwards matching progress events to the parent sink.
pub struct ProgressForwardingSink {
    parent_sink: Arc<dyn EventSink>,
    seen_child_calls: Mutex<HashSet<String>>,
}

impl ProgressForwardingSink {
    pub fn new(parent_sink: Arc<dyn EventSink>) -> Self {
        Self {
            parent_sink,
            seen_child_calls: Mutex::new(HashSet::new()),
        }
    }
}

#[async_trait]
impl EventSink for ProgressForwardingSink {
    async fn emit(&self, event: AgentEvent) {
        match &event {
            // Track direct child tool call IDs
            AgentEvent::ToolCallStart { id, .. } => {
                self.seen_child_calls.lock().unwrap().insert(id.clone());
            }
            // Forward progress events from known direct child calls
            AgentEvent::ActivitySnapshot {
                activity_type,
                content,
                ..
            } if activity_type == TOOL_CALL_PROGRESS_ACTIVITY_TYPE => {
                if let Ok(state) = serde_json::from_value::<ToolCallProgressState>(content.clone())
                {
                    if self
                        .seen_child_calls
                        .lock()
                        .unwrap()
                        .contains(&state.call_id)
                    {
                        self.parent_sink.emit(event).await;
                    }
                }
            }
            // All other events are silently dropped
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::progress::ProgressStatus;
    use serde_json::json;
    use std::sync::Mutex as StdMutex;

    /// Test sink that records all emitted events.
    struct RecordingSink {
        events: StdMutex<Vec<AgentEvent>>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                events: StdMutex::new(Vec::new()),
            }
        }

        fn events(&self) -> Vec<AgentEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl EventSink for RecordingSink {
        async fn emit(&self, event: AgentEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn make_progress_snapshot(call_id: &str, tool_name: &str) -> AgentEvent {
        let state = ToolCallProgressState {
            schema: "tool-call-progress.v1".into(),
            node_id: format!("tool_call:{call_id}"),
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            status: ProgressStatus::Running,
            progress: Some(0.5),
            loaded: None,
            total: None,
            message: Some("working...".into()),
            parent_node_id: None,
            parent_call_id: None,
            run_id: None,
            parent_run_id: None,
            thread_id: None,
        };
        AgentEvent::ActivitySnapshot {
            message_id: "msg-1".into(),
            activity_type: TOOL_CALL_PROGRESS_ACTIVITY_TYPE.into(),
            content: serde_json::to_value(state).unwrap(),
            replace: None,
        }
    }

    #[tokio::test]
    async fn forwards_progress_from_known_child_call() {
        let recorder = Arc::new(RecordingSink::new());
        let sink = ProgressForwardingSink::new(recorder.clone());

        // Register a child tool call
        sink.emit(AgentEvent::ToolCallStart {
            id: "call-1".into(),
            name: "search".into(),
        })
        .await;

        // Emit progress for that call
        sink.emit(make_progress_snapshot("call-1", "search")).await;

        let events = recorder.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AgentEvent::ActivitySnapshot { activity_type, .. }
            if activity_type == TOOL_CALL_PROGRESS_ACTIVITY_TYPE
        ));
    }

    #[tokio::test]
    async fn drops_progress_from_unknown_call() {
        let recorder = Arc::new(RecordingSink::new());
        let sink = ProgressForwardingSink::new(recorder.clone());

        // Emit progress without registering the call (grandchild scenario)
        sink.emit(make_progress_snapshot("unknown-call", "deep_tool"))
            .await;

        assert!(recorder.events().is_empty());
    }

    #[tokio::test]
    async fn drops_non_progress_events() {
        let recorder = Arc::new(RecordingSink::new());
        let sink = ProgressForwardingSink::new(recorder.clone());

        // These should all be dropped
        sink.emit(AgentEvent::TextDelta {
            delta: "hello".into(),
        })
        .await;
        sink.emit(AgentEvent::StepStart {
            message_id: "m1".into(),
        })
        .await;
        sink.emit(AgentEvent::StepEnd).await;

        assert!(recorder.events().is_empty());
    }

    #[tokio::test]
    async fn tracks_multiple_child_calls() {
        let recorder = Arc::new(RecordingSink::new());
        let sink = ProgressForwardingSink::new(recorder.clone());

        sink.emit(AgentEvent::ToolCallStart {
            id: "call-a".into(),
            name: "tool_a".into(),
        })
        .await;
        sink.emit(AgentEvent::ToolCallStart {
            id: "call-b".into(),
            name: "tool_b".into(),
        })
        .await;

        sink.emit(make_progress_snapshot("call-a", "tool_a")).await;
        sink.emit(make_progress_snapshot("call-b", "tool_b")).await;
        sink.emit(make_progress_snapshot("call-c", "tool_c")).await; // unknown

        assert_eq!(recorder.events().len(), 2);
    }

    #[tokio::test]
    async fn ignores_non_progress_activity_snapshots() {
        let recorder = Arc::new(RecordingSink::new());
        let sink = ProgressForwardingSink::new(recorder.clone());

        sink.emit(AgentEvent::ToolCallStart {
            id: "call-1".into(),
            name: "tool".into(),
        })
        .await;

        // Activity snapshot with a different type should be dropped
        sink.emit(AgentEvent::ActivitySnapshot {
            message_id: "msg-1".into(),
            activity_type: "file-activity".into(),
            content: json!({"some": "data"}),
            replace: None,
        })
        .await;

        assert!(recorder.events().is_empty());
    }
}
