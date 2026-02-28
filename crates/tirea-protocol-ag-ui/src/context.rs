use crate::events::{Event, ReasoningEncryptedValueSubtype};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use tirea_contract::{AgentEvent, TerminationReason};
use tracing::warn;
use uuid::Uuid;

// AG-UI Context
// ============================================================================

/// Context for AG-UI event conversion.
///
/// Maintains state needed for converting internal AgentEvents to AG-UI events.
/// State is initialized from the first `RunStart` event in the stream.
#[derive(Debug, Clone)]
pub struct AgUiEventContext {
    /// Prefix derived from run_id (first 8 chars), set on RunStart.
    run_id_prefix: String,
    /// Current message identifier.
    pub message_id: String,
    /// Step counter for generating step names.
    step_counter: u32,
    /// Whether text message stream has started.
    pub(super) text_started: bool,
    /// Whether reasoning stream has started.
    reasoning_started: bool,
    /// Whether text has ever been ended (used to detect restarts).
    text_ever_ended: bool,
    /// Current step name.
    current_step: Option<String>,
    /// Whether a terminal event (RunFinish/Error) has been emitted.
    /// After this, all subsequent events are suppressed.
    stopped: bool,
    /// Last emitted state snapshot, used to compute RFC 6902 deltas.
    last_state: Option<Value>,
    /// Tool call IDs that have received at least one ToolCallDelta (args chunk).
    /// Used to avoid double-emitting TOOL_CALL_ARGS: when ToolCallReady arrives
    /// for a tool call that never received deltas (e.g. frontend tool invocations),
    /// we emit the full arguments as a single TOOL_CALL_ARGS event.
    tool_ids_with_deltas: HashSet<String>,
}

impl AgUiEventContext {
    /// Create a new AG-UI context.
    ///
    /// The context is fully initialized when the first `RunStart` event
    /// arrives, which sets the `message_id` from the run ID.
    pub fn new() -> Self {
        Self {
            run_id_prefix: String::new(),
            message_id: String::new(),
            step_counter: 0,
            text_started: false,
            reasoning_started: false,
            text_ever_ended: false,
            current_step: None,
            stopped: false,
            last_state: None,
            tool_ids_with_deltas: HashSet::new(),
        }
    }

    /// Generate the next step name.
    pub fn next_step_name(&mut self) -> String {
        self.step_counter += 1;
        let name = format!("step_{}", self.step_counter);
        self.current_step = Some(name.clone());
        name
    }

    /// Get the current step name.
    pub fn current_step_name(&self) -> String {
        self.current_step
            .clone()
            .unwrap_or_else(|| format!("step_{}", self.step_counter))
    }

    /// Mark text stream as started.
    ///
    /// If text was previously ended, a new `message_id` is generated so that
    /// each TEXT_MESSAGE_START / TEXT_MESSAGE_END cycle uses a unique ID.
    /// This prevents CopilotKit / AG-UI runtimes from confusing reopened
    /// message IDs with already-ended ones (which causes "text-end for
    /// missing text part" errors on the frontend).
    pub fn start_text(&mut self) -> bool {
        let was_started = self.text_started;
        self.text_started = true;
        if !was_started {
            // Generate a fresh message_id when restarting text after a prior end.
            if self.text_ever_ended {
                self.new_message_id();
            }
            true
        } else {
            false
        }
    }

    /// Mark text stream as ended and return whether it was active.
    pub fn end_text(&mut self) -> bool {
        let was_started = self.text_started;
        self.text_started = false;
        if was_started {
            self.text_ever_ended = true;
        }
        was_started
    }

    /// Whether a text stream is currently open.
    pub fn is_text_open(&self) -> bool {
        self.text_started
    }

    /// Mark reasoning stream as started.
    pub fn start_reasoning(&mut self) -> bool {
        let was_started = self.reasoning_started;
        self.reasoning_started = true;
        !was_started
    }

    /// Mark reasoning stream as ended and return whether it was active.
    pub fn end_reasoning(&mut self) -> bool {
        let was_started = self.reasoning_started;
        self.reasoning_started = false;
        was_started
    }

    /// Reset text lifecycle state for a new step with a pre-generated message ID.
    ///
    /// This ensures that the streaming message ID matches the stored `Message.id`
    /// for the assistant message produced by this step.
    pub fn reset_for_step(&mut self, message_id: String) {
        self.message_id = message_id;
        self.text_started = false;
        self.reasoning_started = false;
        self.text_ever_ended = false;
    }

    /// Generate a new message ID.
    pub fn new_message_id(&mut self) -> String {
        self.message_id = format!("msg_{}_{}", self.run_id_prefix, Uuid::new_v4().simple());
        self.message_id.clone()
    }

    fn close_reasoning_stream(&mut self) -> Vec<Event> {
        if self.end_reasoning() {
            vec![
                Event::reasoning_message_end(&self.message_id),
                Event::reasoning_end(&self.message_id),
            ]
        } else {
            Vec::new()
        }
    }

    fn close_open_streams(&mut self) -> Vec<Event> {
        let mut events = self.close_reasoning_stream();
        if self.end_text() {
            events.push(Event::text_message_end(&self.message_id));
        }
        events
    }

    /// Convert an AgentEvent to AG-UI protocol compatible events.
    ///
    /// Handles full stream lifecycle: text start/end pairs, step counters,
    /// terminal event suppression (after Error), and Pending event filtering.
    pub fn on_agent_event(&mut self, ev: &AgentEvent) -> Vec<Event> {
        // After a terminal event (RunFinish/Error), suppress everything.
        if self.stopped {
            return Vec::new();
        }

        // Lifecycle bookkeeping before conversion.
        match ev {
            AgentEvent::RunFinish { .. } | AgentEvent::Error { .. } => {
                self.stopped = true;
            }
            AgentEvent::ToolCallResumed { .. } => {
                return vec![];
            }
            _ => {}
        }

        match ev {
            AgentEvent::RunStart {
                thread_id,
                run_id,
                parent_run_id,
            } => {
                self.run_id_prefix = run_id.chars().take(8).collect();
                self.message_id = format!("msg_{}", self.run_id_prefix);
                vec![Event::run_started(thread_id, run_id, parent_run_id.clone())]
            }
            AgentEvent::RunFinish {
                thread_id,
                run_id,
                result,
                termination,
            } => {
                let mut events = self.close_open_streams();
                match termination {
                    TerminationReason::Cancelled => {
                        events.push(Event::run_error(
                            "Run cancelled",
                            Some("CANCELLED".to_string()),
                        ));
                    }
                    TerminationReason::Error(ref msg) => {
                        events.push(Event::run_error(msg, Some("ERROR".to_string())));
                    }
                    _ => {
                        events.push(Event::run_finished(thread_id, run_id, result.clone()));
                    }
                }
                events
            }

            AgentEvent::TextDelta { delta } => {
                let mut events = vec![];
                if self.start_text() {
                    events.push(Event::text_message_start(&self.message_id));
                }
                events.push(Event::text_message_content(&self.message_id, delta));
                events
            }
            AgentEvent::ReasoningDelta { delta } => {
                let mut events = vec![];
                if self.start_reasoning() {
                    events.push(Event::reasoning_start(&self.message_id));
                    events.push(Event::reasoning_message_start(&self.message_id));
                }
                events.push(Event::reasoning_message_content(&self.message_id, delta));
                events
            }
            AgentEvent::ReasoningEncryptedValue { encrypted_value } => {
                vec![Event::reasoning_encrypted_value(
                    ReasoningEncryptedValueSubtype::Message,
                    self.message_id.clone(),
                    encrypted_value.clone(),
                )]
            }

            AgentEvent::ToolCallStart { id, name } => {
                let mut events = self.close_open_streams();
                events.push(Event::tool_call_start(
                    id,
                    name,
                    Some(self.message_id.clone()),
                ));
                events
            }
            AgentEvent::ToolCallDelta { id, args_delta } => {
                self.tool_ids_with_deltas.insert(id.clone());
                vec![Event::tool_call_args(id, args_delta)]
            }
            AgentEvent::ToolCallReady { id, arguments, .. } => {
                let mut events = Vec::new();
                // For frontend tool invocations (and any tool call that skipped
                // the streaming ToolCallDelta path), emit the full arguments as
                // a single TOOL_CALL_ARGS event before TOOL_CALL_END.
                if !self.tool_ids_with_deltas.contains(id.as_str()) {
                    let args_str = arguments.to_string();
                    if args_str != "{}" && args_str != "null" {
                        events.push(Event::tool_call_args(id.clone(), args_str));
                    }
                }
                events.push(Event::tool_call_end(id));
                events
            }
            AgentEvent::ToolCallDone {
                id,
                result,
                message_id,
                ..
            } => {
                let content = match serde_json::to_string(&result.to_json()) {
                    Ok(content) => content,
                    Err(err) => {
                        warn!(error = %err, tool_call_id = %id, "failed to serialize tool result for AG-UI");
                        r#"{"error":"failed to serialize tool result"}"#.to_string()
                    }
                };
                let msg_id = if message_id.is_empty() {
                    format!("result_{id}")
                } else {
                    message_id.clone()
                };
                vec![Event::tool_call_result(msg_id, id, content)]
            }

            AgentEvent::StepStart { message_id } => {
                if !message_id.is_empty() {
                    self.reset_for_step(message_id.clone());
                }
                vec![Event::step_started(self.next_step_name())]
            }
            AgentEvent::StepEnd => {
                let mut events = self.close_reasoning_stream();
                events.push(Event::step_finished(self.current_step_name()));
                events
            }

            AgentEvent::StateSnapshot { snapshot } => {
                let mut events = Vec::new();
                // Emit RFC 6902 delta if we have a previous state to diff against.
                if let Some(ref old) = self.last_state {
                    let patch = json_patch::diff(old, snapshot);
                    if !patch.0.is_empty() {
                        let delta = patch
                            .0
                            .iter()
                            .map(|op| serde_json::to_value(op).expect("RFC 6902 op serializes"))
                            .collect();
                        events.push(Event::state_delta(delta));
                    }
                }
                self.last_state = Some(snapshot.clone());
                events.push(Event::state_snapshot(snapshot.clone()));
                events
            }
            AgentEvent::StateDelta { delta } => {
                vec![Event::state_delta(delta.clone())]
            }
            AgentEvent::MessagesSnapshot { messages } => {
                vec![Event::messages_snapshot(messages.clone())]
            }

            AgentEvent::ActivitySnapshot {
                message_id,
                activity_type,
                content,
                replace,
            } => {
                vec![Event::activity_snapshot(
                    message_id.clone(),
                    activity_type.clone(),
                    value_to_map(content),
                    *replace,
                )]
            }
            AgentEvent::ActivityDelta {
                message_id,
                activity_type,
                patch,
            } => {
                vec![Event::activity_delta(
                    message_id.clone(),
                    activity_type.clone(),
                    patch.clone(),
                )]
            }

            AgentEvent::Error { message, code } => {
                let mut events = self.close_reasoning_stream();
                events.push(Event::run_error(message, code.clone()));
                events
            }
            AgentEvent::InferenceComplete {
                model,
                usage,
                duration_ms,
            } => {
                let mut content = serde_json::Map::new();
                content.insert(
                    "model".to_string(),
                    serde_json::Value::String(model.clone()),
                );
                content.insert(
                    "duration_ms".to_string(),
                    serde_json::Value::Number((*duration_ms).into()),
                );
                if let Some(u) = usage {
                    if let Ok(v) = serde_json::to_value(u) {
                        content.insert("usage".to_string(), v);
                    }
                }
                vec![Event::activity_snapshot(
                    self.message_id.clone(),
                    "inference_complete".to_string(),
                    content.into_iter().collect(),
                    Some(false),
                )]
            }
            AgentEvent::ToolCallResumed { .. } => unreachable!(),
        }
    }
}

pub(super) fn value_to_map(value: &Value) -> HashMap<String, Value> {
    match value.as_object() {
        Some(map) => map
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        None => {
            let mut map = HashMap::new();
            map.insert("value".to_string(), value.clone());
            map
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tirea_contract::TokenUsage;

    /// Create a context pre-initialized via a RunStart event.
    fn make_ctx() -> AgUiEventContext {
        let mut ctx = AgUiEventContext::new();
        ctx.on_agent_event(&AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "run_12345678".into(),
            parent_run_id: None,
        });
        ctx
    }

    #[test]
    fn inference_complete_emits_activity_snapshot() {
        let mut ctx = make_ctx();
        let ev = AgentEvent::InferenceComplete {
            model: "gpt-4o".into(),
            usage: Some(TokenUsage {
                prompt_tokens: Some(100),
                completion_tokens: Some(50),
                ..Default::default()
            }),
            duration_ms: 1234,
        };
        let events = ctx.on_agent_event(&ev);
        assert_eq!(events.len(), 1);
        let json = serde_json::to_value(&events[0]).unwrap();
        assert_eq!(json["type"], "ACTIVITY_SNAPSHOT");
        let content = &json["content"];
        assert_eq!(content["model"], "gpt-4o");
        assert_eq!(content["duration_ms"], 1234);
        assert!(content["usage"].is_object());
    }

    #[test]
    fn inference_complete_without_usage() {
        let mut ctx = make_ctx();
        let ev = AgentEvent::InferenceComplete {
            model: "gpt-4o-mini".into(),
            usage: None,
            duration_ms: 500,
        };
        let events = ctx.on_agent_event(&ev);
        assert_eq!(events.len(), 1);
        let json = serde_json::to_value(&events[0]).unwrap();
        assert_eq!(json["type"], "ACTIVITY_SNAPSHOT");
        let content = &json["content"];
        assert_eq!(content["model"], "gpt-4o-mini");
        assert!(content.get("usage").is_none());
    }

    #[test]
    fn reasoning_delta_emits_reasoning_events() {
        let mut ctx = make_ctx();
        let events = ctx.on_agent_event(&AgentEvent::ReasoningDelta {
            delta: "step-by-step".into(),
        });
        assert_eq!(events.len(), 3);

        let values: Vec<serde_json::Value> = events
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        assert_eq!(values[0]["type"], "REASONING_START");
        assert_eq!(values[1]["type"], "REASONING_MESSAGE_START");
        assert_eq!(values[2]["type"], "REASONING_MESSAGE_CONTENT");
        assert_eq!(values[2]["delta"], "step-by-step");
    }

    #[test]
    fn reasoning_encrypted_value_maps_to_message_entity() {
        let mut ctx = make_ctx();
        let events = ctx.on_agent_event(&AgentEvent::ReasoningEncryptedValue {
            encrypted_value: "opaque-token".into(),
        });
        assert_eq!(events.len(), 1);
        let value = serde_json::to_value(&events[0]).unwrap();
        assert_eq!(value["type"], "REASONING_ENCRYPTED_VALUE");
        assert_eq!(value["subtype"], "message");
        assert_eq!(value["encryptedValue"], "opaque-token");
    }
}

// ============================================================================
