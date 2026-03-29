//! AI SDK v6 encoder: maps AgentEvent to UIStreamEvent.

use std::collections::{HashMap, HashSet};

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::lifecycle::TerminationReason;
use awaken_contract::contract::tool::ToolStatus;
use awaken_contract::contract::transport::Transcoder;
use serde_json::{Value, json};

use super::types::UIStreamEvent;

const DATA_EVENT_STATE_SNAPSHOT: &str = "state-snapshot";
const DATA_EVENT_STATE_DELTA: &str = "state-delta";
const DATA_EVENT_MESSAGES_SNAPSHOT: &str = "messages-snapshot";
const DATA_EVENT_ACTIVITY_SNAPSHOT: &str = "activity-snapshot";
const DATA_EVENT_ACTIVITY_DELTA: &str = "activity-delta";
const DATA_EVENT_INFERENCE_COMPLETE: &str = "inference-complete";
const DATA_EVENT_REASONING_ENCRYPTED: &str = "reasoning-encrypted";
const RUN_INFO_EVENT_NAME: &str = "run-info";

/// Stateful encoder for AI SDK v6 UI Message Stream protocol.
///
/// Tracks text block lifecycle (open/close) across tool calls.
#[derive(Debug)]
pub struct AiSdkEncoder {
    message_id: String,
    text_open: bool,
    text_counter: u32,
    finished: bool,
    message_id_set: bool,
    open_reasoning_ids: HashSet<String>,
    /// Accumulated streaming output per tool call id.
    stream_accumulators: HashMap<String, String>,
}

impl AiSdkEncoder {
    pub fn new() -> Self {
        Self {
            message_id: String::new(),
            text_open: false,
            text_counter: 0,
            finished: false,
            message_id_set: false,
            open_reasoning_ids: HashSet::new(),
            stream_accumulators: HashMap::new(),
        }
    }

    fn text_id(&self) -> String {
        format!("txt_{}", self.text_counter)
    }

    fn open_text(&mut self) -> UIStreamEvent {
        self.text_open = true;
        UIStreamEvent::text_start(self.text_id())
    }

    fn close_text(&mut self) -> UIStreamEvent {
        let event = UIStreamEvent::text_end(self.text_id());
        self.text_open = false;
        self.text_counter += 1;
        event
    }

    fn close_all_reasoning(&mut self) -> Vec<UIStreamEvent> {
        let mut ids: Vec<String> = self.open_reasoning_ids.drain().collect();
        ids.sort();
        ids.into_iter().map(UIStreamEvent::reasoning_end).collect()
    }

    fn start_reasoning_if_needed(&mut self, id: &str, events: &mut Vec<UIStreamEvent>) {
        if self.open_reasoning_ids.insert(id.to_string()) {
            events.push(UIStreamEvent::reasoning_start(id));
        }
    }

    pub fn message_id(&self) -> &str {
        &self.message_id
    }

    pub fn on_agent_event(&mut self, ev: &AgentEvent) -> Vec<UIStreamEvent> {
        if self.finished {
            return Vec::new();
        }

        match ev {
            AgentEvent::RunStart {
                run_id, thread_id, ..
            } => {
                self.message_id = run_id.clone();
                let mut events = vec![UIStreamEvent::message_start(&self.message_id)];
                events.push(UIStreamEvent::data(
                    RUN_INFO_EVENT_NAME,
                    json!({
                        "runId": run_id,
                        "threadId": thread_id,
                    }),
                ));
                events
            }

            AgentEvent::TextDelta { delta } => {
                let mut events = Vec::new();
                if !self.text_open {
                    events.push(self.open_text());
                }
                events.push(UIStreamEvent::text_delta(self.text_id(), delta));
                events
            }

            AgentEvent::ReasoningDelta { delta } => {
                let reasoning_id = reasoning_id_for(&self.message_id, None);
                let mut events = Vec::new();
                self.start_reasoning_if_needed(&reasoning_id, &mut events);
                events.push(UIStreamEvent::reasoning_delta(reasoning_id, delta));
                events
            }

            AgentEvent::ReasoningEncryptedValue { encrypted_value } => {
                vec![UIStreamEvent::data_with_options(
                    DATA_EVENT_REASONING_ENCRYPTED,
                    json!({ "encryptedValue": encrypted_value }),
                    Some(reasoning_id_for(&self.message_id, None)),
                    Some(true),
                )]
            }

            AgentEvent::ToolCallStart { id, name } => {
                let mut events = Vec::new();
                if self.text_open {
                    events.push(self.close_text());
                }
                events.push(UIStreamEvent::tool_input_start(id, name));
                events
            }

            AgentEvent::ToolCallDelta { id, args_delta } => {
                vec![UIStreamEvent::tool_input_delta(id, args_delta)]
            }

            AgentEvent::ToolCallReady {
                id,
                name,
                arguments,
            } => {
                vec![UIStreamEvent::tool_input_available(
                    id,
                    name,
                    arguments.clone(),
                )]
            }

            AgentEvent::ToolCallStreamDelta { id, delta, .. } => {
                let acc = self.stream_accumulators.entry(id.clone()).or_default();
                acc.push_str(delta);
                vec![UIStreamEvent::tool_output_preliminary(
                    id,
                    json!(acc.clone()),
                )]
            }

            AgentEvent::ToolCallDone { id, result, .. } => {
                // Clear accumulated stream output for this call
                self.stream_accumulators.remove(id);
                match result.status {
                    ToolStatus::Success => {
                        let output = if result.metadata.is_empty() {
                            result.data.clone()
                        } else {
                            json!({
                                "data": result.data,
                                "metadata": result.metadata,
                            })
                        };
                        vec![UIStreamEvent::tool_output_available(id, output)]
                    }
                    ToolStatus::Pending => {
                        // Suspended tool call — emit approval request
                        vec![UIStreamEvent::tool_approval_request(id, id)]
                    }
                    ToolStatus::Error => {
                        let error_text =
                            result.message.as_deref().unwrap_or("tool execution error");
                        vec![UIStreamEvent::tool_output_error(id, error_text)]
                    }
                }
            }

            AgentEvent::ToolCallResumed { target_id, result } => {
                if result.get("error").and_then(|v| v.as_str()).is_some() {
                    let error_text = result["error"].as_str().unwrap_or("unknown error");
                    vec![UIStreamEvent::tool_output_error(target_id, error_text)]
                } else if result.get("approved") == Some(&json!(false)) {
                    vec![UIStreamEvent::tool_output_denied(target_id)]
                } else {
                    vec![UIStreamEvent::tool_output_available(
                        target_id,
                        result.clone(),
                    )]
                }
            }

            AgentEvent::StepStart { message_id } => {
                if !self.message_id_set && !message_id.is_empty() {
                    self.message_id = message_id.clone();
                    self.message_id_set = true;
                }
                vec![UIStreamEvent::start_step()]
            }

            AgentEvent::StepEnd => {
                let mut events = Vec::new();
                if self.text_open {
                    events.push(self.close_text());
                }
                events.extend(self.close_all_reasoning());
                events.push(UIStreamEvent::finish_step());
                events
            }

            AgentEvent::RunFinish { termination, .. } => {
                self.finished = true;
                let mut events = Vec::new();
                if self.text_open {
                    events.push(self.close_text());
                }
                events.extend(self.close_all_reasoning());
                let reason = match termination {
                    TerminationReason::NaturalEnd | TerminationReason::BehaviorRequested => "stop",
                    TerminationReason::Stopped(_) => "stop",
                    TerminationReason::Cancelled => "stop",
                    TerminationReason::Suspended => "tool-calls",
                    TerminationReason::Error(_) => "error",
                    TerminationReason::Blocked(_) => "content-filter",
                };
                events.push(UIStreamEvent::finish_with_reason(reason));
                events
            }

            AgentEvent::Error { message, .. } => {
                self.finished = true;
                vec![UIStreamEvent::error(message)]
            }

            AgentEvent::StateSnapshot { snapshot } => {
                vec![UIStreamEvent::data(
                    DATA_EVENT_STATE_SNAPSHOT,
                    snapshot.clone(),
                )]
            }

            AgentEvent::StateDelta { delta } => {
                vec![UIStreamEvent::data(DATA_EVENT_STATE_DELTA, json!(delta))]
            }

            AgentEvent::MessagesSnapshot { messages } => {
                vec![UIStreamEvent::data(
                    DATA_EVENT_MESSAGES_SNAPSHOT,
                    json!(messages),
                )]
            }

            AgentEvent::ActivitySnapshot {
                message_id,
                activity_type,
                content,
                replace,
            } => {
                let mut events = Vec::new();

                // Extract dedicated events from activity type
                match activity_type.as_str() {
                    "source-url" => {
                        if let Some(obj) = content.as_object() {
                            let source_id = obj
                                .get("sourceId")
                                .and_then(Value::as_str)
                                .unwrap_or(message_id.as_str());
                            let url = obj.get("url").and_then(Value::as_str).unwrap_or("");
                            let title = obj.get("title").and_then(Value::as_str).map(String::from);
                            events.push(UIStreamEvent::source_url(source_id, url, title));
                        }
                    }
                    "source-document" => {
                        if let Some(obj) = content.as_object() {
                            let source_id = obj
                                .get("sourceId")
                                .and_then(Value::as_str)
                                .unwrap_or(message_id.as_str());
                            let media_type = obj
                                .get("mediaType")
                                .and_then(Value::as_str)
                                .unwrap_or("application/octet-stream");
                            let title = obj.get("title").and_then(Value::as_str).map(String::from);
                            let filename = obj
                                .get("filename")
                                .and_then(Value::as_str)
                                .map(String::from);
                            events.push(UIStreamEvent::source_document(
                                source_id, media_type, title, filename,
                            ));
                        }
                    }
                    "file" => {
                        if let Some(obj) = content.as_object() {
                            let url = obj.get("url").and_then(Value::as_str).unwrap_or("");
                            let media_type = obj
                                .get("mediaType")
                                .and_then(Value::as_str)
                                .unwrap_or("application/octet-stream");
                            events.push(UIStreamEvent::file(url, media_type));
                        }
                    }
                    _ => {}
                }

                // Always emit the data event too
                events.push(UIStreamEvent::data(
                    DATA_EVENT_ACTIVITY_SNAPSHOT,
                    json!({
                        "messageId": message_id,
                        "activityType": activity_type,
                        "content": content,
                        "replace": replace,
                    }),
                ));
                events
            }

            AgentEvent::ActivityDelta {
                message_id,
                activity_type,
                patch,
            } => {
                vec![UIStreamEvent::data(
                    DATA_EVENT_ACTIVITY_DELTA,
                    json!({
                        "messageId": message_id,
                        "activityType": activity_type,
                        "patch": patch,
                    }),
                )]
            }

            AgentEvent::InferenceComplete {
                model,
                usage,
                duration_ms,
            } => {
                vec![UIStreamEvent::data(
                    DATA_EVENT_INFERENCE_COMPLETE,
                    json!({
                        "model": model,
                        "usage": usage,
                        "durationMs": duration_ms,
                    }),
                )]
            }
        }
    }
}

impl Default for AiSdkEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcoder for AiSdkEncoder {
    type Input = AgentEvent;
    type Output = UIStreamEvent;

    fn transcode(&mut self, item: &AgentEvent) -> Vec<UIStreamEvent> {
        self.on_agent_event(item)
    }
}

fn reasoning_id_for(message_id: &str, suffix: Option<&str>) -> String {
    match suffix {
        Some(s) => format!("reasoning_{message_id}_{s}"),
        None => format!("reasoning_{message_id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::event::AgentEvent;
    use awaken_contract::contract::lifecycle::TerminationReason;
    use awaken_contract::contract::suspension::ToolCallOutcome;
    use awaken_contract::contract::tool::ToolResult;
    use serde_json::json;

    #[test]
    fn run_start_emits_message_start_and_run_info() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            parent_run_id: None,
        });
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[0], UIStreamEvent::MessageStart { message_id: Some(id), .. } if id == "r1")
        );
    }

    #[test]
    fn text_delta_opens_text_block() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::TextDelta { delta: "hi".into() });
        assert_eq!(events.len(), 2); // text_start + text_delta
        assert!(matches!(&events[0], UIStreamEvent::TextStart { id, .. } if id == "txt_0"));
        assert!(matches!(&events[1], UIStreamEvent::TextDelta { delta, .. } if delta == "hi"));
    }

    #[test]
    fn second_text_delta_reuses_open_block() {
        let mut enc = AiSdkEncoder::new();
        enc.on_agent_event(&AgentEvent::TextDelta { delta: "a".into() });
        let events = enc.on_agent_event(&AgentEvent::TextDelta { delta: "b".into() });
        assert_eq!(events.len(), 1); // Only delta, no new start
    }

    #[test]
    fn tool_call_start_closes_text_block() {
        let mut enc = AiSdkEncoder::new();
        enc.on_agent_event(&AgentEvent::TextDelta { delta: "hi".into() });
        let events = enc.on_agent_event(&AgentEvent::ToolCallStart {
            id: "c1".into(),
            name: "search".into(),
        });
        assert_eq!(events.len(), 2); // text_end + tool_input_start
        assert!(matches!(&events[0], UIStreamEvent::TextEnd { .. }));
        assert!(matches!(&events[1], UIStreamEvent::ToolInputStart { .. }));
    }

    #[test]
    fn tool_call_delta_maps_to_tool_input_delta() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallDelta {
            id: "c1".into(),
            args_delta: "{\"q\":".into(),
        });
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], UIStreamEvent::ToolInputDelta { .. }));
    }

    #[test]
    fn tool_call_ready_maps_to_tool_input_available() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallReady {
            id: "c1".into(),
            name: "search".into(),
            arguments: json!({"q": "rust"}),
        });
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            UIStreamEvent::ToolInputAvailable { tool_call_id, .. } if tool_call_id == "c1"
        ));
    }

    #[test]
    fn tool_call_done_success_maps_to_output_available() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: "c1".into(),
            message_id: "m1".into(),
            result: ToolResult::success("search", json!({"items": [1]})),
            outcome: ToolCallOutcome::Succeeded,
        });
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            UIStreamEvent::ToolOutputAvailable { tool_call_id, .. } if tool_call_id == "c1"
        ));
    }

    #[test]
    fn tool_call_done_success_without_metadata_passes_data_only() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: "c1".into(),
            message_id: "m1".into(),
            result: ToolResult::success("search", json!({"items": [1, 2]})),
            outcome: ToolCallOutcome::Succeeded,
        });
        assert_eq!(events.len(), 1);
        if let UIStreamEvent::ToolOutputAvailable { output, .. } = &events[0] {
            assert_eq!(*output, json!({"items": [1, 2]}));
        } else {
            panic!("expected ToolOutputAvailable");
        }
    }

    #[test]
    fn tool_call_done_success_with_metadata_includes_both() {
        let mut enc = AiSdkEncoder::new();
        let mut result = ToolResult::success("search", json!({"items": [1]}));
        result
            .metadata
            .insert("mcp.server".into(), json!("my-server"));
        let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: "c1".into(),
            message_id: "m1".into(),
            result,
            outcome: ToolCallOutcome::Succeeded,
        });
        assert_eq!(events.len(), 1);
        if let UIStreamEvent::ToolOutputAvailable { output, .. } = &events[0] {
            assert_eq!(output["data"], json!({"items": [1]}));
            assert_eq!(output["metadata"]["mcp.server"], json!("my-server"));
        } else {
            panic!("expected ToolOutputAvailable");
        }
    }

    #[test]
    fn tool_call_done_error_maps_to_output_error() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: "c1".into(),
            message_id: "m1".into(),
            result: ToolResult::error("search", "not found"),
            outcome: ToolCallOutcome::Failed,
        });
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            UIStreamEvent::ToolOutputError { error_text, .. } if error_text == "not found"
        ));
    }

    #[test]
    fn tool_call_done_pending_maps_to_approval_request() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: "c1".into(),
            message_id: "m1".into(),
            result: ToolResult::suspended("dangerous", "needs approval"),
            outcome: ToolCallOutcome::Suspended,
        });
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            UIStreamEvent::ToolApprovalRequest { tool_call_id, .. } if tool_call_id == "c1"
        ));
    }

    #[test]
    fn run_finish_closes_text_and_emits_finish() {
        let mut enc = AiSdkEncoder::new();
        enc.on_agent_event(&AgentEvent::TextDelta { delta: "hi".into() });
        let events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::NaturalEnd,
        });
        assert!(events.len() >= 2); // text_end + finish
        assert!(matches!(
            events.last().unwrap(),
            UIStreamEvent::Finish { .. }
        ));
    }

    #[test]
    fn terminal_guard_suppresses_events_after_finish() {
        let mut enc = AiSdkEncoder::new();
        enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::NaturalEnd,
        });
        let events = enc.on_agent_event(&AgentEvent::TextDelta {
            delta: "ignored".into(),
        });
        assert!(events.is_empty());
    }

    #[test]
    fn error_event_sets_terminal_guard() {
        let mut enc = AiSdkEncoder::new();
        enc.on_agent_event(&AgentEvent::Error {
            message: "fatal".into(),
            code: Some("E001".into()),
        });
        let events = enc.on_agent_event(&AgentEvent::TextDelta {
            delta: "ignored".into(),
        });
        assert!(events.is_empty());
    }

    #[test]
    fn reasoning_delta_opens_reasoning_block() {
        let mut enc = AiSdkEncoder::new();
        enc.message_id = "msg1".into();
        let events = enc.on_agent_event(&AgentEvent::ReasoningDelta {
            delta: "thinking".into(),
        });
        assert_eq!(events.len(), 2); // reasoning_start + reasoning_delta
    }

    #[test]
    fn state_snapshot_emits_data_event() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
            snapshot: json!({"key": "value"}),
        });
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn inference_complete_emits_data_event() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::InferenceComplete {
            model: "gpt-4o".into(),
            usage: None,
            duration_ms: 1234,
        });
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn step_events_pass_through() {
        let mut enc = AiSdkEncoder::new();
        let start = enc.on_agent_event(&AgentEvent::StepStart {
            message_id: "m1".into(),
        });
        assert_eq!(start.len(), 1);
        assert!(matches!(&start[0], UIStreamEvent::StartStep));

        let end = enc.on_agent_event(&AgentEvent::StepEnd);
        assert_eq!(end.len(), 1);
        assert!(matches!(&end[0], UIStreamEvent::FinishStep));
    }

    #[test]
    fn text_counter_increments_after_close() {
        let mut enc = AiSdkEncoder::new();
        // Open text block 0
        enc.on_agent_event(&AgentEvent::TextDelta { delta: "a".into() });
        // Close it via tool call
        enc.on_agent_event(&AgentEvent::ToolCallStart {
            id: "c1".into(),
            name: "t".into(),
        });
        // Open text block 1
        let events = enc.on_agent_event(&AgentEvent::TextDelta { delta: "b".into() });
        assert!(matches!(
            &events[0],
            UIStreamEvent::TextStart { id, .. } if id == "txt_1"
        ));
    }

    #[test]
    fn run_finish_suspended_uses_tool_calls_reason() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::Suspended,
        });
        match events.last().unwrap() {
            UIStreamEvent::Finish { finish_reason, .. } => {
                assert_eq!(finish_reason.as_deref(), Some("tool-calls"));
            }
            _ => panic!("expected Finish"),
        }
    }

    #[test]
    fn transcoder_trait_delegates_to_on_agent_event() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.transcode(&AgentEvent::TextDelta { delta: "hi".into() });
        assert!(!events.is_empty());
    }

    #[test]
    fn activity_snapshot_source_url_emits_source_url_event() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
            message_id: "m1".into(),
            activity_type: "source-url".into(),
            content: json!({
                "sourceId": "src1",
                "url": "https://example.com",
                "title": "Example"
            }),
            replace: Some(false),
        });
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            UIStreamEvent::SourceUrl { source_id, url, title, .. }
            if source_id == "src1" && url == "https://example.com" && title.as_deref() == Some("Example")
        ));
    }

    #[test]
    fn activity_snapshot_source_document_emits_source_document_event() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
            message_id: "m1".into(),
            activity_type: "source-document".into(),
            content: json!({
                "sourceId": "doc1",
                "mediaType": "application/pdf",
                "title": "Report",
                "filename": "report.pdf"
            }),
            replace: Some(false),
        });
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            UIStreamEvent::SourceDocument { source_id, media_type, .. }
            if source_id == "doc1" && media_type == "application/pdf"
        ));
    }

    #[test]
    fn activity_snapshot_file_emits_file_event() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
            message_id: "m1".into(),
            activity_type: "file".into(),
            content: json!({
                "url": "https://example.com/file.png",
                "mediaType": "image/png"
            }),
            replace: Some(false),
        });
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            UIStreamEvent::File { url, media_type, .. }
            if url == "https://example.com/file.png" && media_type == "image/png"
        ));
    }

    #[test]
    fn activity_snapshot_unknown_type_emits_only_data_event() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
            message_id: "m1".into(),
            activity_type: "custom-type".into(),
            content: json!({"key": "val"}),
            replace: Some(false),
        });
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn step_end_closes_open_text_and_reasoning() {
        let mut enc = AiSdkEncoder::new();
        enc.message_id = "msg1".into();
        // Open text and reasoning
        enc.on_agent_event(&AgentEvent::TextDelta { delta: "hi".into() });
        enc.on_agent_event(&AgentEvent::ReasoningDelta {
            delta: "think".into(),
        });
        let events = enc.on_agent_event(&AgentEvent::StepEnd);
        // text_end + reasoning_end + finish_step
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], UIStreamEvent::TextEnd { .. }));
        assert!(matches!(&events[1], UIStreamEvent::ReasoningEnd { .. }));
        assert!(matches!(&events[2], UIStreamEvent::FinishStep));
    }

    #[test]
    fn step_end_without_open_blocks_emits_only_finish_step() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::StepEnd);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], UIStreamEvent::FinishStep));
    }

    #[test]
    fn tool_call_stream_delta_emits_preliminary_output() {
        let mut enc = AiSdkEncoder::new();
        // First delta
        let events = enc.on_agent_event(&AgentEvent::ToolCallStreamDelta {
            id: "c1".into(),
            name: "json_render".into(),
            delta: "hello".into(),
        });
        assert_eq!(events.len(), 1);
        let json = serde_json::to_string(&events[0]).unwrap();
        assert!(json.contains("tool-output-available"));
        assert!(json.contains("\"preliminary\":true"));
        assert!(json.contains("hello"));
    }

    #[test]
    fn tool_call_stream_delta_accumulates_across_deltas() {
        let mut enc = AiSdkEncoder::new();
        // First delta
        enc.on_agent_event(&AgentEvent::ToolCallStreamDelta {
            id: "c1".into(),
            name: "openui".into(),
            delta: "hello ".into(),
        });
        // Second delta — output should contain accumulated text
        let events = enc.on_agent_event(&AgentEvent::ToolCallStreamDelta {
            id: "c1".into(),
            name: "openui".into(),
            delta: "world".into(),
        });
        assert_eq!(events.len(), 1);
        let json = serde_json::to_string(&events[0]).unwrap();
        assert!(json.contains("hello world"));
        assert!(json.contains("\"preliminary\":true"));
    }

    #[test]
    fn tool_call_done_clears_stream_accumulator() {
        let mut enc = AiSdkEncoder::new();
        // Accumulate some deltas
        enc.on_agent_event(&AgentEvent::ToolCallStreamDelta {
            id: "c1".into(),
            name: "render".into(),
            delta: "partial".into(),
        });
        // ToolCallDone should clear accumulator and emit final output
        let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: "c1".into(),
            message_id: "m1".into(),
            result: ToolResult::success("render", json!({"final": "result"})),
            outcome: ToolCallOutcome::Succeeded,
        });
        assert_eq!(events.len(), 1);
        let json = serde_json::to_string(&events[0]).unwrap();
        // Final output should NOT have preliminary field
        assert!(!json.contains("preliminary"));
        assert!(json.contains("final"));

        // New stream delta for same id should start fresh
        let events = enc.on_agent_event(&AgentEvent::ToolCallStreamDelta {
            id: "c1".into(),
            name: "render".into(),
            delta: "fresh".into(),
        });
        let json = serde_json::to_string(&events[0]).unwrap();
        assert!(json.contains("fresh"));
        assert!(!json.contains("partial"));
    }

    #[test]
    fn multiple_tool_calls_stream_independently() {
        let mut enc = AiSdkEncoder::new();
        // Delta for tool c1
        enc.on_agent_event(&AgentEvent::ToolCallStreamDelta {
            id: "c1".into(),
            name: "render_a".into(),
            delta: "aaa".into(),
        });
        // Delta for tool c2
        enc.on_agent_event(&AgentEvent::ToolCallStreamDelta {
            id: "c2".into(),
            name: "render_b".into(),
            delta: "bbb".into(),
        });
        // Second delta for c1
        let events = enc.on_agent_event(&AgentEvent::ToolCallStreamDelta {
            id: "c1".into(),
            name: "render_a".into(),
            delta: "111".into(),
        });
        let json = serde_json::to_string(&events[0]).unwrap();
        assert!(json.contains("aaa111"));
        assert!(!json.contains("bbb"));
    }
}
