use super::UIStreamEvent;
use serde_json::Value;
use std::collections::HashSet;
use tirea_contract::runtime::tool_call::ToolStatus;
use tirea_contract::{AgentEvent, TerminationReason, Transcoder};

pub(crate) const DATA_EVENT_STATE_SNAPSHOT: &str = "state-snapshot";
pub(crate) const DATA_EVENT_STATE_DELTA: &str = "state-delta";
pub(crate) const DATA_EVENT_MESSAGES_SNAPSHOT: &str = "messages-snapshot";
pub(crate) const DATA_EVENT_ACTIVITY_SNAPSHOT: &str = "activity-snapshot";
pub(crate) const DATA_EVENT_ACTIVITY_DELTA: &str = "activity-delta";
pub(crate) const DATA_EVENT_INFERENCE_COMPLETE: &str = "inference-complete";
pub(crate) const DATA_EVENT_REASONING_ENCRYPTED: &str = "reasoning-encrypted";
const RUN_INFO_EVENT_NAME: &str = "run-info";

/// Stateful encoder for AI SDK v6 UI Message Stream protocol.
///
/// Tracks text block lifecycle (open/close) across tool calls, ensuring
/// `text-start` and `text-end` are always properly paired. This mirrors the
/// pattern used by AG-UI encoders for AG-UI.
///
/// # Text lifecycle rules
///
/// - `TextDelta` with text closed → prepend `text-start`, open text
/// - `ToolCallStart` with text open → prepend `text-end`, close text
/// - `RunFinish` with text open → prepend `text-end` before `finish`
/// - `Error` → terminal, no `text-end` needed
#[derive(Debug)]
pub struct AiSdkEncoder {
    message_id: String,
    /// Prefix derived from run_id (first 8 chars), set on RunStart.
    run_id_prefix: String,
    text_open: bool,
    text_counter: u32,
    finished: bool,
    /// Whether an external message ID has been consumed from a StepStart event.
    message_id_set: bool,
    /// Reasoning blocks that are currently open (for delta-style streaming).
    open_reasoning_ids: HashSet<String>,
}

impl AiSdkEncoder {
    /// Create a new encoder.
    ///
    /// The encoder is fully initialized when the first `RunStart` event
    /// arrives, which sets the `message_id` from the run ID.
    pub fn new() -> Self {
        Self {
            message_id: String::new(),
            run_id_prefix: String::new(),
            text_open: false,
            text_counter: 0,
            finished: false,
            message_id_set: false,
            open_reasoning_ids: HashSet::new(),
        }
    }

    /// Current text block ID (e.g. `txt_0`, `txt_1`, ...).
    fn text_id(&self) -> String {
        format!("txt_{}", self.text_counter)
    }

    /// Emit `text-start` and mark text as open. Returns the new text ID.
    fn open_text(&mut self) -> UIStreamEvent {
        self.text_open = true;
        UIStreamEvent::text_start(self.text_id())
    }

    /// Emit `text-end` for the current text block and mark text as closed.
    /// Increments the counter so the next text block gets a fresh ID.
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

    fn close_reasoning_if_open(&mut self, id: &str, events: &mut Vec<UIStreamEvent>) {
        if self.open_reasoning_ids.remove(id) {
            events.push(UIStreamEvent::reasoning_end(id));
        }
    }

    /// Get the message ID.
    pub fn message_id(&self) -> &str {
        &self.message_id
    }

    /// Convert an `AgentEvent` to UI stream events with proper text lifecycle.
    pub fn on_agent_event(&mut self, ev: &AgentEvent) -> Vec<UIStreamEvent> {
        if self.finished {
            return Vec::new();
        }

        match ev {
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
                    serde_json::json!({ "encryptedValue": encrypted_value }),
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
                let mut events = vec![UIStreamEvent::tool_input_available(
                    id,
                    name,
                    arguments.clone(),
                )];
                if Self::is_permission_confirmation_tool(name) {
                    events.push(UIStreamEvent::tool_approval_request(id.clone(), id.clone()));
                }
                events
            }
            AgentEvent::ToolCallDone { id, result, .. } => match result.status {
                ToolStatus::Success | ToolStatus::Warning | ToolStatus::Pending => {
                    vec![UIStreamEvent::tool_output_available(id, result.to_json())]
                }
                ToolStatus::Error => {
                    let error_text = result
                        .message
                        .clone()
                        .or_else(|| {
                            result
                                .data
                                .get("error")
                                .and_then(|v| v.get("message"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| "tool output error".to_string());
                    vec![UIStreamEvent::tool_output_error(id, error_text)]
                }
            },

            AgentEvent::RunFinish { termination, .. } => {
                self.finished = true;
                let mut events = Vec::new();
                if self.text_open {
                    events.push(self.close_text());
                }
                events.extend(self.close_all_reasoning());
                if matches!(termination, TerminationReason::Cancelled) {
                    events.push(UIStreamEvent::abort("cancelled"));
                } else {
                    let finish_reason = Self::map_termination(termination);
                    events.push(UIStreamEvent::finish_with_reason(finish_reason));
                }
                events
            }

            AgentEvent::Error { message } => {
                self.finished = true;
                self.text_open = false;
                let mut events = self.close_all_reasoning();
                events.push(UIStreamEvent::error(message));
                events
            }

            AgentEvent::StepStart { message_id } => {
                if !self.message_id_set {
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
            AgentEvent::RunStart {
                run_id, thread_id, ..
            } => {
                self.run_id_prefix = run_id.chars().take(8).collect();
                self.message_id = format!("msg_{}", self.run_id_prefix);
                vec![
                    UIStreamEvent::message_start(&self.message_id),
                    UIStreamEvent::data(
                        RUN_INFO_EVENT_NAME,
                        serde_json::json!({
                            "protocol": "ai-sdk-ui-message-stream",
                            "protocolVersion": "v1",
                            "aiSdkVersion": super::AI_SDK_VERSION,
                            "threadId": thread_id,
                            "runId": run_id,
                        }),
                    ),
                ]
            }
            AgentEvent::InferenceComplete {
                model,
                usage,
                duration_ms,
            } => {
                let payload = serde_json::json!({
                    "model": model,
                    "usage": usage,
                    "duration_ms": duration_ms,
                });
                vec![UIStreamEvent::data(DATA_EVENT_INFERENCE_COMPLETE, payload)]
            }

            AgentEvent::StateSnapshot { snapshot } => {
                vec![UIStreamEvent::data(
                    DATA_EVENT_STATE_SNAPSHOT,
                    snapshot.clone(),
                )]
            }
            AgentEvent::StateDelta { delta } => {
                vec![UIStreamEvent::data(
                    DATA_EVENT_STATE_DELTA,
                    serde_json::Value::Array(delta.clone()),
                )]
            }
            AgentEvent::MessagesSnapshot { messages } => {
                vec![UIStreamEvent::data(
                    DATA_EVENT_MESSAGES_SNAPSHOT,
                    serde_json::Value::Array(messages.clone()),
                )]
            }
            AgentEvent::ActivitySnapshot {
                message_id,
                activity_type,
                content,
                replace,
            } => {
                let mut events = self.map_activity_snapshot(message_id, activity_type, content);
                let payload = serde_json::json!({
                    "messageId": message_id,
                    "activityType": activity_type,
                    "content": content,
                    "replace": replace,
                });
                events.push(UIStreamEvent::data(DATA_EVENT_ACTIVITY_SNAPSHOT, payload));
                events
            }
            AgentEvent::ActivityDelta {
                message_id,
                activity_type,
                patch,
            } => {
                let mut events = self.map_activity_delta(message_id, activity_type, patch);
                let payload = serde_json::json!({
                    "messageId": message_id,
                    "activityType": activity_type,
                    "patch": patch,
                });
                events.push(UIStreamEvent::data(DATA_EVENT_ACTIVITY_DELTA, payload));
                events
            }
            AgentEvent::ToolCallResumed { target_id, result } => {
                self.map_interaction_resolved(target_id, result)
            }
        }
    }

    fn map_termination(reason: &TerminationReason) -> &'static str {
        match reason {
            TerminationReason::NaturalEnd
            | TerminationReason::BehaviorRequested
            | TerminationReason::Suspended => "stop",
            TerminationReason::Cancelled => "other",
            TerminationReason::Error => "error",
            TerminationReason::Stopped(stopped) => match stopped.code.as_str() {
                "max_rounds_reached" | "timeout_reached" | "token_budget_exceeded" => "length",
                "tool_called" => "tool-calls",
                "content_matched" => "stop",
                "consecutive_errors_exceeded" | "loop_detected" => "error",
                _ => "other",
            },
        }
    }

    fn map_activity_snapshot(
        &mut self,
        message_id: &str,
        activity_type: &str,
        content: &Value,
    ) -> Vec<UIStreamEvent> {
        let activity_type = normalize_activity_type(activity_type);
        match activity_type.as_str() {
            "reasoning" => self.map_reasoning_snapshot(message_id, content),
            "source-url" => map_source_url_snapshot(message_id, content)
                .into_iter()
                .collect(),
            "source-document" => map_source_document_snapshot(message_id, content)
                .into_iter()
                .collect(),
            "file" => map_file_snapshot(content).into_iter().collect(),
            _ => Vec::new(),
        }
    }

    fn map_activity_delta(
        &mut self,
        message_id: &str,
        activity_type: &str,
        patch: &[Value],
    ) -> Vec<UIStreamEvent> {
        let activity_type = normalize_activity_type(activity_type);
        if activity_type != "reasoning" {
            return Vec::new();
        }

        let mut events = Vec::new();
        let reasoning_id = reasoning_id_for(message_id, None);
        let mut has_delta = false;

        for op in patch {
            if !is_patch_write_operation(op) {
                continue;
            }
            if let Some(delta) = extract_reasoning_text(op.get("value")) {
                if !delta.is_empty() {
                    self.start_reasoning_if_needed(&reasoning_id, &mut events);
                    events.push(UIStreamEvent::reasoning_delta(&reasoning_id, delta));
                    has_delta = true;
                }
            }
            if is_done_marker(op.get("value")) {
                self.close_reasoning_if_open(&reasoning_id, &mut events);
            }
        }

        if !has_delta && patch.iter().any(|op| is_done_marker(op.get("value"))) {
            self.close_reasoning_if_open(&reasoning_id, &mut events);
        }

        events
    }

    fn map_reasoning_snapshot(&mut self, message_id: &str, content: &Value) -> Vec<UIStreamEvent> {
        let reasoning_id = reasoning_id_for(message_id, content.get("id").and_then(Value::as_str));
        let mut events = Vec::new();

        let text = extract_reasoning_text(Some(content)).unwrap_or_default();
        let done = is_done_marker(Some(content));

        if !text.is_empty() {
            self.start_reasoning_if_needed(&reasoning_id, &mut events);
            events.push(UIStreamEvent::reasoning_delta(&reasoning_id, text));
        }

        if done || (matches!(content, Value::String(_)) || content.get("text").is_some()) {
            self.close_reasoning_if_open(&reasoning_id, &mut events);
        }

        events
    }

    fn map_interaction_resolved(&self, target_id: &str, result: &Value) -> Vec<UIStreamEvent> {
        if let Some(err) = result.get("error").and_then(Value::as_str) {
            return vec![UIStreamEvent::tool_output_error(target_id, err)];
        }
        if tirea_contract::SuspensionResponse::is_denied(result) {
            return vec![UIStreamEvent::tool_output_denied(target_id)];
        }
        vec![UIStreamEvent::tool_output_available(
            target_id,
            result.clone(),
        )]
    }

    fn is_permission_confirmation_tool(tool_name: &str) -> bool {
        tool_name.eq_ignore_ascii_case("PermissionConfirm")
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

fn normalize_activity_type(activity_type: &str) -> String {
    activity_type.trim().to_ascii_lowercase().replace('_', "-")
}

fn reasoning_id_for(message_id: &str, explicit: Option<&str>) -> String {
    explicit
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("reasoning_{message_id}"))
}

fn extract_reasoning_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => map
            .get("delta")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| map.get("text").and_then(Value::as_str).map(str::to_string)),
        _ => None,
    }
}

fn is_done_marker(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return false;
    };
    match value {
        Value::Object(map) => map.get("done").and_then(Value::as_bool).unwrap_or(false),
        _ => false,
    }
}

fn is_patch_write_operation(op: &Value) -> bool {
    op.get("op")
        .and_then(Value::as_str)
        .is_some_and(|name| name == "add" || name == "replace")
}

fn map_source_url_snapshot(message_id: &str, content: &Value) -> Option<UIStreamEvent> {
    let url = content.get("url")?.as_str()?;
    let source_id = content
        .get("sourceId")
        .and_then(Value::as_str)
        .or_else(|| content.get("id").and_then(Value::as_str))
        .unwrap_or(message_id);
    let title = content
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string);
    let provider_metadata = content.get("providerMetadata").cloned();
    Some(UIStreamEvent::SourceUrl {
        source_id: source_id.to_string(),
        url: url.to_string(),
        title,
        provider_metadata,
    })
}

fn map_source_document_snapshot(message_id: &str, content: &Value) -> Option<UIStreamEvent> {
    let media_type = content
        .get("mediaType")
        .or_else(|| content.get("media_type"))
        .and_then(Value::as_str)?;
    let title = content.get("title")?.as_str()?;
    let source_id = content
        .get("sourceId")
        .and_then(Value::as_str)
        .or_else(|| content.get("id").and_then(Value::as_str))
        .unwrap_or(message_id);
    let filename = content
        .get("filename")
        .and_then(Value::as_str)
        .map(str::to_string);
    let provider_metadata = content.get("providerMetadata").cloned();
    Some(UIStreamEvent::SourceDocument {
        source_id: source_id.to_string(),
        media_type: media_type.to_string(),
        title: title.to_string(),
        filename,
        provider_metadata,
    })
}

fn map_file_snapshot(content: &Value) -> Option<UIStreamEvent> {
    let url = content.get("url")?.as_str()?;
    let media_type = content
        .get("mediaType")
        .or_else(|| content.get("media_type"))
        .and_then(Value::as_str)?;
    let provider_metadata = content.get("providerMetadata").cloned();
    Some(UIStreamEvent::File {
        url: url.to_string(),
        media_type: media_type.to_string(),
        provider_metadata,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tirea_contract::TokenUsage;

    #[test]
    fn inference_complete_emits_data_event() {
        let mut enc = AiSdkEncoder::new();
        let ev = AgentEvent::InferenceComplete {
            model: "gpt-4o".into(),
            usage: Some(TokenUsage {
                prompt_tokens: Some(100),
                completion_tokens: Some(50),
                ..Default::default()
            }),
            duration_ms: 1234,
        };
        let events = enc.on_agent_event(&ev);
        assert_eq!(events.len(), 1);
        match &events[0] {
            UIStreamEvent::Data {
                data_type, data, ..
            } => {
                assert_eq!(data_type, &format!("data-{DATA_EVENT_INFERENCE_COMPLETE}"));
                assert_eq!(data["model"], "gpt-4o");
                assert_eq!(data["duration_ms"], 1234);
                assert!(data["usage"].is_object(), "usage: {:?}", data["usage"]);
            }
            other => panic!("expected Data event, got: {:?}", other),
        }
    }

    #[test]
    fn inference_complete_without_usage() {
        let mut enc = AiSdkEncoder::new();
        let ev = AgentEvent::InferenceComplete {
            model: "gpt-4o-mini".into(),
            usage: None,
            duration_ms: 500,
        };
        let events = enc.on_agent_event(&ev);
        assert_eq!(events.len(), 1);
        match &events[0] {
            UIStreamEvent::Data {
                data_type, data, ..
            } => {
                assert_eq!(data_type, &format!("data-{DATA_EVENT_INFERENCE_COMPLETE}"));
                assert_eq!(data["model"], "gpt-4o-mini");
                assert!(data["usage"].is_null());
            }
            other => panic!("expected Data event, got: {:?}", other),
        }
    }

    #[test]
    fn reasoning_agent_event_emits_reasoning_stream_events() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ReasoningDelta {
            delta: "thinking".to_string(),
        });

        assert!(events
            .iter()
            .any(|ev| matches!(ev, UIStreamEvent::ReasoningStart { .. })));
        assert!(events.iter().any(
            |ev| matches!(ev, UIStreamEvent::ReasoningDelta { delta, .. } if delta == "thinking")
        ));
    }

    #[test]
    fn run_finish_closes_reasoning_started_from_reasoning_event() {
        let mut enc = AiSdkEncoder::new();
        let open_events = enc.on_agent_event(&AgentEvent::ReasoningDelta {
            delta: "step-1".to_string(),
        });
        assert!(open_events
            .iter()
            .any(|ev| matches!(ev, UIStreamEvent::ReasoningStart { .. })));

        let finish_events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "thread_1".to_string(),
            run_id: "run_reasoning_finish".to_string(),
            result: None,
            termination: TerminationReason::NaturalEnd,
        });
        assert!(
            finish_events
                .iter()
                .any(|ev| matches!(ev, UIStreamEvent::ReasoningEnd { .. })),
            "run finish should close open reasoning block"
        );
    }

    #[test]
    fn reasoning_encrypted_event_emits_transient_data_event() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ReasoningEncryptedValue {
            encrypted_value: "opaque-token".to_string(),
        });

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            UIStreamEvent::Data {
                data_type,
                data,
                transient: Some(true),
                ..
            } if data_type == "data-reasoning-encrypted"
                && data["encryptedValue"] == "opaque-token"
        ));
    }

    #[test]
    fn tool_call_done_with_error_status_emits_tool_output_error() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: "call_error_1".to_string(),
            result: tirea_contract::runtime::tool_call::ToolResult::error(
                "search",
                "tool backend failed",
            ),
            patch: None,
            message_id: "msg_tool_error_1".to_string(),
        });

        assert!(events.iter().any(|ev| matches!(
            ev,
            UIStreamEvent::ToolOutputError {
                tool_call_id,
                error_text,
                ..
            } if tool_call_id == "call_error_1" && error_text == "tool backend failed"
        )));
    }

    #[test]
    fn tool_call_lifecycle_emits_streaming_input_ready_and_output_events() {
        let mut enc = AiSdkEncoder::new();

        let start = enc.on_agent_event(&AgentEvent::ToolCallStart {
            id: "call_1".to_string(),
            name: "search".to_string(),
        });
        assert!(
            start.iter().any(
                |ev| matches!(ev, UIStreamEvent::ToolInputStart { tool_call_id, tool_name, .. }
                    if tool_call_id == "call_1" && tool_name == "search")
            ),
            "tool start should map to tool-input-start"
        );

        let delta = enc.on_agent_event(&AgentEvent::ToolCallDelta {
            id: "call_1".to_string(),
            args_delta: "{\"q\":\"ru".to_string(),
        });
        assert!(
            delta.iter().any(
                |ev| matches!(ev, UIStreamEvent::ToolInputDelta { tool_call_id, input_text_delta }
                    if tool_call_id == "call_1" && input_text_delta == "{\"q\":\"ru")
            ),
            "tool delta should map to tool-input-delta"
        );

        let ready = enc.on_agent_event(&AgentEvent::ToolCallReady {
            id: "call_1".to_string(),
            name: "search".to_string(),
            arguments: json!({ "q": "rust" }),
        });
        assert!(
            ready.iter().any(
                |ev| matches!(ev, UIStreamEvent::ToolInputAvailable { tool_call_id, tool_name, input, .. }
                    if tool_call_id == "call_1" && tool_name == "search" && input["q"] == "rust")
            ),
            "tool ready should map to tool-input-available"
        );

        let done = enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: "call_1".to_string(),
            result: tirea_contract::runtime::tool_call::ToolResult::success(
                "search",
                json!({ "items": [1, 2] }),
            ),
            patch: None,
            message_id: "msg_tool_1".to_string(),
        });
        assert!(
            done.iter().any(
                |ev| matches!(ev, UIStreamEvent::ToolOutputAvailable { tool_call_id, output, .. }
                    if tool_call_id == "call_1" && output["data"]["items"][0] == 1)
            ),
            "tool done should map to tool-output-available"
        );
    }

    #[test]
    fn permission_tool_ready_emits_tool_approval_request() {
        let mut enc = AiSdkEncoder::new();
        let ready = enc.on_agent_event(&AgentEvent::ToolCallReady {
            id: "fc_perm_1".to_string(),
            name: "PermissionConfirm".to_string(),
            arguments: json!({ "tool_name": "echo", "tool_args": { "message": "x" } }),
        });
        assert!(
            ready.iter().any(|ev| matches!(
                ev,
                UIStreamEvent::ToolApprovalRequest { approval_id, tool_call_id }
                if approval_id == "fc_perm_1" && tool_call_id == "fc_perm_1"
            )),
            "permission tool should emit tool-approval-request"
        );
    }

    #[test]
    fn interaction_resolved_denied_emits_tool_output_denied() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
            target_id: "fc_perm_1".to_string(),
            result: json!({ "approved": false, "reason": "nope" }),
        });
        assert!(
            events.iter().any(|ev| matches!(
                ev,
                UIStreamEvent::ToolOutputDenied { tool_call_id }
                if tool_call_id == "fc_perm_1"
            )),
            "denied interaction should emit tool-output-denied"
        );
    }

    #[test]
    fn interaction_resolved_error_emits_tool_output_error() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
            target_id: "ask_call_2".to_string(),
            result: json!({ "approved": false, "error": "frontend validation failed" }),
        });
        assert!(
            events.iter().any(|ev| matches!(
                ev,
                UIStreamEvent::ToolOutputError { tool_call_id, error_text, .. }
                if tool_call_id == "ask_call_2" && error_text == "frontend validation failed"
            )),
            "errored interaction should emit tool-output-error"
        );
    }

    #[test]
    fn interaction_resolved_output_payload_emits_tool_output_available() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
            target_id: "ask_call_1".to_string(),
            result: json!({ "message": "blue" }),
        });
        assert!(
            events.iter().any(|ev| matches!(
                ev,
                UIStreamEvent::ToolOutputAvailable { tool_call_id, output, .. }
                if tool_call_id == "ask_call_1" && output["message"] == "blue"
            )),
            "ask interaction resolution should emit tool-output-available"
        );
    }

    #[test]
    fn step_events_emit_start_step_and_finish_step() {
        let mut enc = AiSdkEncoder::new();

        let step_start = enc.on_agent_event(&AgentEvent::StepStart {
            message_id: "msg_external".to_string(),
        });
        assert!(
            step_start
                .iter()
                .any(|ev| matches!(ev, UIStreamEvent::StartStep)),
            "step start should map to start-step"
        );

        let step_end = enc.on_agent_event(&AgentEvent::StepEnd);
        assert!(
            step_end
                .iter()
                .any(|ev| matches!(ev, UIStreamEvent::FinishStep)),
            "step end should map to finish-step"
        );
    }

    #[test]
    fn cancelled_run_emits_abort_and_closes_open_blocks() {
        let mut enc = AiSdkEncoder::new();

        let text_events = enc.on_agent_event(&AgentEvent::TextDelta {
            delta: "hello".to_string(),
        });
        assert_eq!(text_events.len(), 2);

        let reasoning_events = enc.on_agent_event(&AgentEvent::ActivityDelta {
            message_id: "m1".to_string(),
            activity_type: "reasoning".to_string(),
            patch: vec![json!({
                "op": "add",
                "path": "/delta",
                "value": {"delta": "thinking"}
            })],
        });
        assert!(
            reasoning_events
                .iter()
                .any(|ev| matches!(ev, UIStreamEvent::ReasoningStart { .. })),
            "reasoning block should start from activity delta"
        );

        let finish_events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "thread_1".to_string(),
            run_id: "run_cancelled".to_string(),
            result: None,
            termination: TerminationReason::Cancelled,
        });
        assert!(
            finish_events
                .iter()
                .any(|ev| matches!(ev, UIStreamEvent::TextEnd { .. })),
            "cancel should close open text block"
        );
        assert!(
            finish_events
                .iter()
                .any(|ev| matches!(ev, UIStreamEvent::ReasoningEnd { .. })),
            "cancel should close open reasoning block"
        );
        assert!(
            finish_events
                .iter()
                .any(|ev| matches!(ev, UIStreamEvent::Abort { .. })),
            "cancelled run should emit abort event"
        );
    }

    #[test]
    fn activity_snapshot_reasoning_emits_reasoning_and_data_events() {
        let mut enc = AiSdkEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
            message_id: "m2".to_string(),
            activity_type: "reasoning".to_string(),
            content: json!({"text":"let me think"}),
            replace: Some(true),
        });

        assert!(events
            .iter()
            .any(|ev| matches!(ev, UIStreamEvent::ReasoningStart { .. })));
        assert!(
            events
                .iter()
                .any(|ev| matches!(ev, UIStreamEvent::ReasoningDelta { delta, .. } if delta == "let me think"))
        );
        assert!(events
            .iter()
            .any(|ev| matches!(ev, UIStreamEvent::ReasoningEnd { .. })));
        assert!(
            events.iter().any(
                |ev| matches!(ev, UIStreamEvent::Data { data_type, .. } if data_type == "data-activity-snapshot")
            ),
            "activity snapshot data event should remain for backward compatibility"
        );
    }

    #[test]
    fn activity_snapshot_source_url_document_and_file_emit_native_events() {
        let mut enc = AiSdkEncoder::new();

        let url_events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
            message_id: "src_1".to_string(),
            activity_type: "source_url".to_string(),
            content: json!({
                "url": "https://example.com",
                "title": "Example"
            }),
            replace: Some(true),
        });
        assert!(url_events.iter().any(
            |ev| matches!(ev, UIStreamEvent::SourceUrl { url, .. } if url == "https://example.com")
        ));

        let doc_events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
            message_id: "src_2".to_string(),
            activity_type: "source-document".to_string(),
            content: json!({
                "sourceId": "doc_1",
                "mediaType": "application/pdf",
                "title": "Doc",
                "filename": "doc.pdf"
            }),
            replace: Some(true),
        });
        assert!(doc_events.iter().any(|ev| matches!(
            ev,
            UIStreamEvent::SourceDocument {
                source_id,
                media_type,
                title,
                filename,
                ..
            } if source_id == "doc_1"
                && media_type == "application/pdf"
                && title == "Doc"
                && filename.as_deref() == Some("doc.pdf")
        )));

        let file_events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
            message_id: "src_3".to_string(),
            activity_type: "file".to_string(),
            content: json!({
                "url": "https://example.com/a.png",
                "mediaType": "image/png"
            }),
            replace: Some(true),
        });
        assert!(
            file_events
                .iter()
                .any(|ev| matches!(ev, UIStreamEvent::File { url, media_type, .. } if url == "https://example.com/a.png" && media_type == "image/png"))
        );
    }

    #[test]
    fn activity_delta_reasoning_done_closes_reasoning_block() {
        let mut enc = AiSdkEncoder::new();

        let first = enc.on_agent_event(&AgentEvent::ActivityDelta {
            message_id: "m3".to_string(),
            activity_type: "reasoning".to_string(),
            patch: vec![json!({
                "op": "add",
                "path": "/delta",
                "value": {"delta":"step-1"}
            })],
        });
        assert!(first
            .iter()
            .any(|ev| matches!(ev, UIStreamEvent::ReasoningStart { .. })));
        assert!(first.iter().any(
            |ev| matches!(ev, UIStreamEvent::ReasoningDelta { delta, .. } if delta == "step-1")
        ));

        let second = enc.on_agent_event(&AgentEvent::ActivityDelta {
            message_id: "m3".to_string(),
            activity_type: "reasoning".to_string(),
            patch: vec![json!({
                "op": "replace",
                "path": "/status",
                "value": {"done": true}
            })],
        });
        assert!(
            second
                .iter()
                .any(|ev| matches!(ev, UIStreamEvent::ReasoningEnd { .. })),
            "done marker should close reasoning block"
        );
    }
}
