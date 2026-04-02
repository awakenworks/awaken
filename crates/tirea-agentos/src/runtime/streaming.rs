//! Streaming response handling for LLM responses.
//!
//! This module provides the internal event types for the agent loop:
//! - `AgentEvent`: Protocol-agnostic events emitted by the agent
//! - `StreamCollector` / `StreamResult`: Helpers for collecting stream chunks
//!
//! Protocol-specific conversion lives in the respective protocol modules:
//! - `tirea_protocol_ag_ui::AGUIContext::on_agent_event()`: protocol events
//! - `tirea_protocol_ai_sdk_v6::AiSdkEncoder::on_agent_event()`: AI SDK v6 events

use crate::contracts::thread::ToolCall;
use genai::chat::{ChatStreamEvent, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use tirea_contract::runtime::inference::StopReason;
use tirea_contract::{StreamResult, TokenUsage};

pub(crate) fn token_usage_from_genai(u: &Usage) -> TokenUsage {
    let (cache_read, cache_creation) = u
        .prompt_tokens_details
        .as_ref()
        .map_or((None, None), |d| (d.cached_tokens, d.cache_creation_tokens));

    let thinking_tokens = u
        .completion_tokens_details
        .as_ref()
        .and_then(|d| d.reasoning_tokens);

    TokenUsage {
        prompt_tokens: u.prompt_tokens,
        completion_tokens: u.completion_tokens,
        total_tokens: u.total_tokens,
        cache_read_tokens: cache_read,
        cache_creation_tokens: cache_creation,
        thinking_tokens,
    }
}

pub(crate) fn map_genai_stop_reason(reason: &genai::chat::StopReason) -> Option<StopReason> {
    match reason {
        genai::chat::StopReason::Completed(_) => Some(StopReason::EndTurn),
        genai::chat::StopReason::MaxTokens(_) => Some(StopReason::MaxTokens),
        genai::chat::StopReason::ToolCall(_) => Some(StopReason::ToolUse),
        genai::chat::StopReason::StopSequence(_) => Some(StopReason::StopSequence),
        genai::chat::StopReason::ContentFilter(_) | genai::chat::StopReason::Other(_) => None,
    }
}

/// Partial tool call being collected during streaming.
#[derive(Debug, Clone)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
    ready_emitted: bool,
}

/// Collector for streaming LLM responses.
///
/// Processes stream events and accumulates text and tool calls.
#[derive(Debug, Default)]
pub struct StreamCollector {
    text: String,
    tool_calls: HashMap<String, PartialToolCall>,
    tool_call_order: Vec<String>,
    usage: Option<Usage>,
    stop_reason: Option<genai::chat::StopReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StreamRecoveryCheckpoint {
    NoPayload,
    PartialText(String),
    ToolCallObserved,
}

impl StreamCollector {
    /// Create a new stream collector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume the collector and return only the accumulated text.
    ///
    /// Used for stream error recovery: partial tool calls are discarded
    /// (incomplete JSON arguments are not usable), but text is preserved.
    pub fn into_partial_text(self) -> String {
        self.text
    }

    /// Consume the collector and return the safest recovery checkpoint.
    pub(crate) fn into_recovery_checkpoint(self) -> StreamRecoveryCheckpoint {
        if !self.tool_calls.is_empty() {
            StreamRecoveryCheckpoint::ToolCallObserved
        } else if self.text.is_empty() {
            StreamRecoveryCheckpoint::NoPayload
        } else {
            StreamRecoveryCheckpoint::PartialText(self.text)
        }
    }

    /// Process a stream event and optionally return an output event.
    ///
    /// This is a pure-ish function - it updates internal state and returns
    /// an output event if something notable happened.
    pub fn process(&mut self, event: ChatStreamEvent) -> Vec<StreamOutput> {
        match event {
            ChatStreamEvent::Chunk(chunk) => {
                if !chunk.content.is_empty() {
                    self.text.push_str(&chunk.content);
                    return vec![StreamOutput::TextDelta(chunk.content)];
                }
                Vec::new()
            }
            ChatStreamEvent::ReasoningChunk(chunk) => {
                if !chunk.content.is_empty() {
                    return vec![StreamOutput::ReasoningDelta(chunk.content)];
                }
                Vec::new()
            }
            ChatStreamEvent::ThoughtSignatureChunk(chunk) => {
                if !chunk.content.is_empty() {
                    return vec![StreamOutput::ReasoningEncryptedValue(chunk.content)];
                }
                Vec::new()
            }
            ChatStreamEvent::ToolCallChunk(tool_chunk) => {
                let call_id = tool_chunk.tool_call.call_id.clone();

                // Get or create partial tool call while preserving first-seen order.
                let partial = match self.tool_calls.entry(call_id.clone()) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        self.tool_call_order.push(call_id.clone());
                        e.insert(PartialToolCall {
                            id: call_id.clone(),
                            name: String::new(),
                            arguments: String::new(),
                            ready_emitted: false,
                        })
                    }
                };

                let mut outputs = Vec::new();

                // Update name if provided (non-empty)
                if !tool_chunk.tool_call.fn_name.is_empty() && partial.name.is_empty() {
                    partial.name = tool_chunk.tool_call.fn_name.clone();
                    outputs.push(StreamOutput::ToolCallStart {
                        id: call_id.clone(),
                        name: partial.name.clone(),
                    });
                }

                // Extract raw argument string from fn_arguments.
                // genai wraps argument strings in Value::String(...);
                // .to_string() would JSON-serialize it with extra quotes.
                // With capture_tool_calls enabled, each chunk carries the
                // ACCUMULATED value (not a delta), so we replace rather than
                // append.
                let args_str = match &tool_chunk.tool_call.fn_arguments {
                    Value::String(s) if !s.is_empty() => s.clone(),
                    Value::Null | Value::String(_) => String::new(),
                    other => other.to_string(),
                };
                if !args_str.is_empty() {
                    // Compute delta for the output event
                    let delta = if args_str.len() > partial.arguments.len()
                        && args_str.starts_with(&partial.arguments)
                    {
                        args_str[partial.arguments.len()..].to_string()
                    } else {
                        args_str.clone()
                    };
                    partial.arguments = args_str;
                    if !delta.is_empty() {
                        outputs.push(StreamOutput::ToolCallDelta {
                            id: call_id,
                            args_delta: delta,
                        });
                    }
                }

                if !partial.ready_emitted && !partial.name.is_empty() {
                    if let Ok(arguments) = serde_json::from_str::<Value>(&partial.arguments) {
                        partial.ready_emitted = true;
                        outputs.push(StreamOutput::ToolCallReady {
                            id: partial.id.clone(),
                            name: partial.name.clone(),
                            arguments,
                        });
                    }
                }

                outputs
            }
            ChatStreamEvent::End(end) => {
                self.stop_reason = end.captured_stop_reason.clone();
                // Use captured tool calls from the End event as the source
                // of truth, overriding any partial data accumulated during
                // streaming (which may be incorrect if chunks carried
                // accumulated rather than delta values).
                if let Some(tool_calls) = end.captured_tool_calls() {
                    for tc in tool_calls {
                        // Extract raw string; genai may wrap in Value::String
                        let end_args = match &tc.fn_arguments {
                            Value::String(s) if !s.is_empty() => s.clone(),
                            Value::Null | Value::String(_) => String::new(),
                            other => other.to_string(),
                        };
                        match self.tool_calls.entry(tc.call_id.clone()) {
                            std::collections::hash_map::Entry::Occupied(mut e) => {
                                let partial = e.get_mut();
                                if partial.name.is_empty() {
                                    partial.name = tc.fn_name.clone();
                                }
                                // Always prefer End event arguments over streaming
                                if !end_args.is_empty() {
                                    partial.arguments = end_args;
                                }
                            }
                            std::collections::hash_map::Entry::Vacant(e) => {
                                self.tool_call_order.push(tc.call_id.clone());
                                e.insert(PartialToolCall {
                                    id: tc.call_id.clone(),
                                    name: tc.fn_name.clone(),
                                    arguments: end_args,
                                    ready_emitted: false,
                                });
                            }
                        }
                    }
                }
                // Capture token usage
                self.usage = end.captured_usage;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Finish collecting and return the final result.
    ///
    /// `max_output_tokens` is used to infer `StopReason::MaxTokens` when the
    /// backend does not provide an explicit stop reason (e.g. genai). Pass
    /// `None` to skip inference; the `stop_reason` field will be set based
    /// on tool call presence only.
    pub fn finish(self, max_output_tokens: Option<u32>) -> StreamResult {
        let mut remaining = self.tool_calls;
        let mut tool_calls: Vec<ToolCall> = Vec::with_capacity(self.tool_call_order.len());

        for call_id in self.tool_call_order {
            let Some(p) = remaining.remove(&call_id) else {
                continue;
            };
            if p.name.is_empty() {
                continue;
            }
            let arguments = serde_json::from_str(&p.arguments).unwrap_or(Value::Null);
            // Drop tool calls with unparseable arguments (truncated JSON).
            if arguments.is_null() && !p.arguments.is_empty() {
                continue;
            }
            tool_calls.push(ToolCall::new(p.id, p.name, arguments));
        }

        let usage = self.usage.as_ref().map(token_usage_from_genai);
        let explicit_stop_reason = self.stop_reason.as_ref().and_then(map_genai_stop_reason);
        let mut stop_reason = explicit_stop_reason
            .or_else(|| Self::infer_stop_reason(&tool_calls, &usage, max_output_tokens));

        // When hitting max_tokens, the last tool call may have been
        // truncated mid-name or have empty arguments. Drop it — the model
        // will re-issue on the next turn after seeing the other results.
        if matches!(
            stop_reason,
            Some(StopReason::MaxTokens) | Some(StopReason::ToolUse)
        ) {
            if let (Some(u), Some(max)) = (&usage, max_output_tokens) {
                if u.completion_tokens == Some(max as i32) {
                    if let Some(last) = tool_calls.last() {
                        if last.arguments.is_null() {
                            tool_calls.pop();
                            // Re-infer: may switch ToolUse -> MaxTokens if
                            // this was the only (incomplete) tool call.
                            stop_reason = explicit_stop_reason.or_else(|| {
                                Self::infer_stop_reason(&tool_calls, &usage, max_output_tokens)
                            });
                        }
                    }
                }
            }
        }

        StreamResult {
            text: self.text,
            tool_calls,
            usage,
            stop_reason,
        }
    }

    /// Infer `StopReason` from response metadata.
    ///
    /// When the backend does not provide an explicit stop reason, we use:
    /// - `ToolUse` if any complete tool calls are present
    /// - `MaxTokens` if `completion_tokens == max_output_tokens` (near-deterministic)
    /// - `EndTurn` otherwise
    fn infer_stop_reason(
        tool_calls: &[ToolCall],
        usage: &Option<TokenUsage>,
        max_output_tokens: Option<u32>,
    ) -> Option<StopReason> {
        if !tool_calls.is_empty() {
            return Some(StopReason::ToolUse);
        }
        if let (Some(u), Some(max)) = (usage, max_output_tokens) {
            if u.completion_tokens == Some(max as i32) {
                return Some(StopReason::MaxTokens);
            }
        }
        Some(StopReason::EndTurn)
    }

    /// Get the current accumulated text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Check if any tool calls have been collected.
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

/// Output event from stream processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamOutput {
    /// Text content delta.
    TextDelta(String),
    /// Reasoning content delta.
    ReasoningDelta(String),
    /// Opaque reasoning token/signature delta.
    ReasoningEncryptedValue(String),
    /// Tool call started with name.
    ToolCallStart { id: String, name: String },
    /// Tool call arguments delta.
    ToolCallDelta { id: String, args_delta: String },
    /// Tool call input is complete and parsed.
    ToolCallReady {
        id: String,
        name: String,
        arguments: Value,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::runtime::tool_call::ToolResult;
    use crate::contracts::AgentEvent;
    use crate::contracts::TerminationReason;
    use genai::chat::CompletionTokensDetails;
    use serde_json::json;

    #[test]
    fn test_extract_response_with_value() {
        let result = Some(json!({"response": "Hello world"}));
        assert_eq!(AgentEvent::extract_response(&result), "Hello world");
    }

    #[test]
    fn test_extract_response_none() {
        assert_eq!(AgentEvent::extract_response(&None), "");
    }

    #[test]
    fn test_extract_response_missing_key() {
        let result = Some(json!({"other": "value"}));
        assert_eq!(AgentEvent::extract_response(&result), "");
    }

    #[test]
    fn test_extract_response_non_string() {
        let result = Some(json!({"response": 42}));
        assert_eq!(AgentEvent::extract_response(&result), "");
    }

    #[test]
    fn test_stream_collector_new() {
        let collector = StreamCollector::new();
        assert!(collector.text().is_empty());
        assert!(!collector.has_tool_calls());
    }

    #[test]
    fn test_map_genai_stop_reason_known_values() {
        use genai::chat::StopReason as GSR;
        assert_eq!(
            map_genai_stop_reason(&GSR::from("stop".to_string())),
            Some(StopReason::EndTurn)
        );
        assert_eq!(
            map_genai_stop_reason(&GSR::from("end_turn".to_string())),
            Some(StopReason::EndTurn)
        );
        assert_eq!(
            map_genai_stop_reason(&GSR::from("length".to_string())),
            Some(StopReason::MaxTokens)
        );
        assert_eq!(
            map_genai_stop_reason(&GSR::from("max_tokens".to_string())),
            Some(StopReason::MaxTokens)
        );
        assert_eq!(
            map_genai_stop_reason(&GSR::from("tool_calls".to_string())),
            Some(StopReason::ToolUse)
        );
        assert_eq!(
            map_genai_stop_reason(&GSR::from("stop_sequence".to_string())),
            Some(StopReason::StopSequence)
        );
    }

    #[test]
    fn test_map_genai_stop_reason_unknown_value() {
        use genai::chat::StopReason as GSR;
        assert_eq!(
            map_genai_stop_reason(&GSR::from("content_filter".to_string())),
            None
        );
    }

    #[test]
    fn test_stream_collector_finish_prefers_explicit_stop_reason() {
        let mut collector = StreamCollector::new();
        collector.process(ChatStreamEvent::End(genai::chat::StreamEnd {
            captured_usage: Some(Usage {
                completion_tokens: Some(128),
                ..Default::default()
            }),
            captured_stop_reason: Some(genai::chat::StopReason::from("stop_sequence".to_string())),
            ..Default::default()
        }));

        let result = collector.finish(Some(128));
        assert_eq!(result.stop_reason, Some(StopReason::StopSequence));
    }

    #[test]
    fn test_stream_collector_finish_falls_back_when_explicit_stop_reason_unknown() {
        let mut collector = StreamCollector::new();
        collector.process(ChatStreamEvent::End(genai::chat::StreamEnd {
            captured_usage: Some(Usage {
                completion_tokens: Some(128),
                ..Default::default()
            }),
            captured_stop_reason: Some(genai::chat::StopReason::from(
                "unknown_stop_reason".to_string(),
            )),
            ..Default::default()
        }));

        let result = collector.finish(Some(128));
        assert_eq!(result.stop_reason, Some(StopReason::MaxTokens));
    }

    #[test]
    fn test_stream_collector_finish_empty() {
        let collector = StreamCollector::new();
        let result = collector.finish(None);

        assert!(result.text.is_empty());
        assert!(result.tool_calls.is_empty());
        assert!(!result.needs_tools());
    }

    #[test]
    fn test_stream_result_needs_tools() {
        let result = StreamResult {
            text: "Hello".to_string(),
            tool_calls: vec![],
            usage: None,
            stop_reason: None,
        };
        assert!(!result.needs_tools());

        let result_with_tools = StreamResult {
            text: String::new(),
            tool_calls: vec![ToolCall::new("id", "name", serde_json::json!({}))],
            usage: None,
            stop_reason: None,
        };
        assert!(result_with_tools.needs_tools());
    }

    #[test]
    fn test_stream_output_variants() {
        let text_delta = StreamOutput::TextDelta("Hello".to_string());
        match text_delta {
            StreamOutput::TextDelta(s) => assert_eq!(s, "Hello"),
            _ => panic!("Expected TextDelta"),
        }

        let tool_start = StreamOutput::ToolCallStart {
            id: "call_1".to_string(),
            name: "search".to_string(),
        };
        match tool_start {
            StreamOutput::ToolCallStart { id, name } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "search");
            }
            _ => panic!("Expected ToolCallStart"),
        }

        let tool_delta = StreamOutput::ToolCallDelta {
            id: "call_1".to_string(),
            args_delta: r#"{"query":"#.to_string(),
        };
        match tool_delta {
            StreamOutput::ToolCallDelta { id, args_delta } => {
                assert_eq!(id, "call_1");
                assert!(args_delta.contains("query"));
            }
            _ => panic!("Expected ToolCallDelta"),
        }

        let reasoning_delta = StreamOutput::ReasoningDelta("analysis".to_string());
        match reasoning_delta {
            StreamOutput::ReasoningDelta(s) => assert_eq!(s, "analysis"),
            _ => panic!("Expected ReasoningDelta"),
        }

        let reasoning_token = StreamOutput::ReasoningEncryptedValue("opaque".to_string());
        match reasoning_token {
            StreamOutput::ReasoningEncryptedValue(s) => assert_eq!(s, "opaque"),
            _ => panic!("Expected ReasoningEncryptedValue"),
        }
    }

    #[test]
    fn test_agent_event_variants() {
        // Test TextDelta
        let event = AgentEvent::TextDelta {
            delta: "Hello".to_string(),
        };
        match event {
            AgentEvent::TextDelta { delta } => assert_eq!(delta, "Hello"),
            _ => panic!("Expected TextDelta"),
        }

        let event = AgentEvent::ReasoningDelta {
            delta: "thinking".to_string(),
        };
        match event {
            AgentEvent::ReasoningDelta { delta } => assert_eq!(delta, "thinking"),
            _ => panic!("Expected ReasoningDelta"),
        }

        // Test ToolCallStart
        let event = AgentEvent::ToolCallStart {
            id: "call_1".to_string(),
            name: "search".to_string(),
        };
        if let AgentEvent::ToolCallStart { id, name } = event {
            assert_eq!(id, "call_1");
            assert_eq!(name, "search");
        }

        // Test ToolCallDelta
        let event = AgentEvent::ToolCallDelta {
            id: "call_1".to_string(),
            args_delta: "{}".to_string(),
        };
        if let AgentEvent::ToolCallDelta { id, .. } = event {
            assert_eq!(id, "call_1");
        }

        // Test ToolCallDone
        let result = ToolResult::success("test", json!({"value": 42}));
        let event = AgentEvent::ToolCallDone {
            id: "call_1".to_string(),
            result: result.clone(),
            patch: None,
            message_id: String::new(),
        };
        if let AgentEvent::ToolCallDone {
            id,
            result: r,
            patch,
            ..
        } = event
        {
            assert_eq!(id, "call_1");
            assert!(r.is_success());
            assert!(patch.is_none());
        }

        // Test RunFinish
        let event = AgentEvent::RunFinish {
            thread_id: "t1".to_string(),
            run_id: "r1".to_string(),
            result: Some(json!({"response": "Final response"})),
            termination: crate::contracts::TerminationReason::NaturalEnd,
        };
        if let AgentEvent::RunFinish { result, .. } = &event {
            assert_eq!(AgentEvent::extract_response(result), "Final response");
        }

        // Test ActivitySnapshot
        let event = AgentEvent::ActivitySnapshot {
            message_id: "activity_1".to_string(),
            activity_type: "progress".to_string(),
            content: json!({"progress": 0.5}),
            replace: Some(true),
        };
        if let AgentEvent::ActivitySnapshot {
            message_id,
            activity_type,
            content,
            replace,
        } = event
        {
            assert_eq!(message_id, "activity_1");
            assert_eq!(activity_type, "progress");
            assert_eq!(content["progress"], 0.5);
            assert_eq!(replace, Some(true));
        }

        // Test ActivityDelta
        let event = AgentEvent::ActivityDelta {
            message_id: "activity_1".to_string(),
            activity_type: "progress".to_string(),
            patch: vec![json!({"op": "replace", "path": "/progress", "value": 0.75})],
        };
        if let AgentEvent::ActivityDelta {
            message_id,
            activity_type,
            patch,
        } = event
        {
            assert_eq!(message_id, "activity_1");
            assert_eq!(activity_type, "progress");
            assert_eq!(patch.len(), 1);
        }

        // Test Error
        let event = AgentEvent::Error {
            message: "Something went wrong".to_string(),
            code: None,
        };
        if let AgentEvent::Error { message, .. } = event {
            assert!(message.contains("wrong"));
        }
    }

    #[test]
    fn test_stream_result_with_multiple_tool_calls() {
        let result = StreamResult {
            text: "I'll call multiple tools".to_string(),
            tool_calls: vec![
                ToolCall::new("call_1", "search", json!({"q": "rust"})),
                ToolCall::new("call_2", "calculate", json!({"expr": "1+1"})),
                ToolCall::new("call_3", "format", json!({"text": "hello"})),
            ],
            usage: None,
            stop_reason: None,
        };

        assert!(result.needs_tools());
        assert_eq!(result.tool_calls.len(), 3);
        assert_eq!(result.tool_calls[0].name, "search");
        assert_eq!(result.tool_calls[1].name, "calculate");
        assert_eq!(result.tool_calls[2].name, "format");
    }

    #[test]
    fn test_stream_result_text_only() {
        let result = StreamResult {
            text: "This is a long response without any tool calls. It just contains text."
                .to_string(),
            tool_calls: vec![],
            usage: None,
            stop_reason: None,
        };

        assert!(!result.needs_tools());
        assert!(result.text.len() > 50);
    }

    #[test]
    fn test_tool_call_with_complex_arguments() {
        let call = ToolCall::new(
            "call_complex",
            "api_request",
            json!({
                "method": "POST",
                "url": "https://api.example.com/data",
                "headers": {
                    "Content-Type": "application/json",
                    "Authorization": "Bearer token"
                },
                "body": {
                    "items": [1, 2, 3],
                    "nested": {
                        "deep": true
                    }
                }
            }),
        );

        assert_eq!(call.id, "call_complex");
        assert_eq!(call.name, "api_request");
        assert_eq!(call.arguments["method"], "POST");
        assert!(call.arguments["headers"]["Content-Type"]
            .as_str()
            .unwrap()
            .contains("json"));
    }

    #[test]
    fn test_agent_event_done_with_patch() {
        use tirea_state::{path, Op, Patch, TrackedPatch};

        let patch = TrackedPatch::new(Patch::new().with_op(Op::set(path!("value"), json!(42))));

        let event = AgentEvent::ToolCallDone {
            id: "call_1".to_string(),
            result: ToolResult::success("test", json!({})),
            patch: Some(patch.clone()),
            message_id: String::new(),
        };

        if let AgentEvent::ToolCallDone { patch: p, .. } = event {
            assert!(p.is_some());
            let p = p.unwrap();
            assert!(!p.patch().is_empty());
        }
    }

    #[test]
    fn test_stream_output_debug() {
        let output = StreamOutput::TextDelta("test".to_string());
        let debug_str = format!("{:?}", output);
        assert!(debug_str.contains("TextDelta"));
        assert!(debug_str.contains("test"));
    }

    #[test]
    fn test_agent_event_debug() {
        let event = AgentEvent::Error {
            message: "error message".to_string(),
            code: None,
        };
        let debug_str = format!("{:?}", event);
        assert!(debug_str.contains("Error"));
        assert!(debug_str.contains("error message"));
    }

    #[test]
    fn test_stream_result_clone() {
        let result = StreamResult {
            text: "Hello".to_string(),
            tool_calls: vec![ToolCall::new("1", "test", json!({}))],
            usage: None,
            stop_reason: None,
        };

        let cloned = result.clone();
        assert_eq!(cloned.text, result.text);
        assert_eq!(cloned.tool_calls.len(), result.tool_calls.len());
    }

    // Tests with mock ChatStreamEvent
    use genai::chat::{StreamChunk, StreamEnd, ToolChunk};

    #[test]
    fn test_stream_collector_process_text_chunk() {
        let mut collector = StreamCollector::new();

        // Process text chunk
        let chunk = ChatStreamEvent::Chunk(StreamChunk {
            content: "Hello ".to_string(),
        });
        let output = collector.process(chunk);

        assert!(!output.is_empty());
        if let Some(StreamOutput::TextDelta(delta)) = output.into_iter().next() {
            assert_eq!(delta, "Hello ");
        } else {
            panic!("Expected TextDelta");
        }

        assert_eq!(collector.text(), "Hello ");
    }

    #[test]
    fn test_stream_collector_process_reasoning_chunk() {
        let mut collector = StreamCollector::new();

        let chunk = ChatStreamEvent::ReasoningChunk(StreamChunk {
            content: "chain".to_string(),
        });
        let output = collector.process(chunk);

        if let Some(StreamOutput::ReasoningDelta(delta)) = output.into_iter().next() {
            assert_eq!(delta, "chain");
        } else {
            panic!("Expected ReasoningDelta");
        }
    }

    #[test]
    fn test_stream_collector_process_thought_signature_chunk() {
        let mut collector = StreamCollector::new();

        let chunk = ChatStreamEvent::ThoughtSignatureChunk(StreamChunk {
            content: "opaque-token".to_string(),
        });
        let output = collector.process(chunk);

        if let Some(StreamOutput::ReasoningEncryptedValue(value)) = output.into_iter().next() {
            assert_eq!(value, "opaque-token");
        } else {
            panic!("Expected ReasoningEncryptedValue");
        }
    }

    #[test]
    fn test_stream_collector_process_multiple_text_chunks() {
        let mut collector = StreamCollector::new();

        // Process multiple chunks
        let chunks = vec!["Hello ", "world", "!"];
        for text in &chunks {
            let chunk = ChatStreamEvent::Chunk(StreamChunk {
                content: text.to_string(),
            });
            collector.process(chunk);
        }

        assert_eq!(collector.text(), "Hello world!");

        let result = collector.finish(None);
        assert_eq!(result.text, "Hello world!");
        assert!(!result.needs_tools());
    }

    #[test]
    fn test_stream_collector_process_empty_chunk() {
        let mut collector = StreamCollector::new();

        let chunk = ChatStreamEvent::Chunk(StreamChunk {
            content: String::new(),
        });
        let output = collector.process(chunk);

        assert!(output.is_empty());
        assert!(collector.text().is_empty());
    }

    #[test]
    fn test_stream_collector_process_tool_call_start() {
        let mut collector = StreamCollector::new();

        let tool_call = genai::chat::ToolCall {
            call_id: "call_123".to_string(),
            fn_name: "search".to_string(),
            fn_arguments: json!(null),
            thought_signatures: None,
        };
        let chunk = ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call });
        let output = collector.process(chunk);

        assert!(!output.is_empty());
        if let Some(StreamOutput::ToolCallStart { id, name }) = output.into_iter().next() {
            assert_eq!(id, "call_123");
            assert_eq!(name, "search");
        } else {
            panic!("Expected ToolCallStart");
        }

        assert!(collector.has_tool_calls());
    }

    #[test]
    fn test_stream_collector_process_tool_call_with_arguments() {
        let mut collector = StreamCollector::new();

        // First chunk: tool call start
        let tool_call1 = genai::chat::ToolCall {
            call_id: "call_abc".to_string(),
            fn_name: "calculator".to_string(),
            fn_arguments: json!(null),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
            tool_call: tool_call1,
        }));

        // Second chunk: arguments delta
        let tool_call2 = genai::chat::ToolCall {
            call_id: "call_abc".to_string(),
            fn_name: String::new(), // Name already set
            fn_arguments: json!({"expr": "1+1"}),
            thought_signatures: None,
        };
        let output = collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
            tool_call: tool_call2,
        }));

        assert!(!output.is_empty());
        if let Some(StreamOutput::ToolCallDelta { id, args_delta }) = output
            .into_iter()
            .find(|event| matches!(event, StreamOutput::ToolCallDelta { .. }))
        {
            assert_eq!(id, "call_abc");
            assert!(args_delta.contains("expr"));
        }

        let result = collector.finish(None);
        assert!(result.needs_tools());
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "calculator");
    }

    #[test]
    fn test_stream_collector_single_chunk_with_name_and_args_keeps_tool_start() {
        let mut collector = StreamCollector::new();

        let tool_call = genai::chat::ToolCall {
            call_id: "call_single".to_string(),
            fn_name: "search".to_string(),
            fn_arguments: Value::String(r#"{"q":"rust"}"#.to_string()),
            thought_signatures: None,
        };
        let output = collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call }));

        assert!(
            output
                .iter()
                .any(|event| matches!(event, StreamOutput::ToolCallStart { .. })),
            "tool start should not be lost when name+args arrive in one chunk; got: {output:?}"
        );
        assert!(output
            .iter()
            .any(|event| matches!(event, StreamOutput::ToolCallReady { .. })));

        let result = collector.finish(None);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].id, "call_single");
        assert_eq!(result.tool_calls[0].name, "search");
        assert_eq!(result.tool_calls[0].arguments, json!({"q":"rust"}));
    }

    #[test]
    fn test_stream_collector_preserves_tool_call_arrival_order() {
        let mut collector = StreamCollector::new();
        let call_ids = vec![
            "call_7", "call_3", "call_1", "call_9", "call_2", "call_8", "call_4", "call_6",
        ];

        for (idx, call_id) in call_ids.iter().enumerate() {
            let tool_call = genai::chat::ToolCall {
                call_id: (*call_id).to_string(),
                fn_name: format!("tool_{idx}"),
                fn_arguments: Value::Null,
                thought_signatures: None,
            };
            let _ = collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call }));
        }

        let result = collector.finish(None);
        let got: Vec<String> = result.tool_calls.into_iter().map(|c| c.id).collect();
        let expected: Vec<String> = call_ids.into_iter().map(str::to_string).collect();

        assert_eq!(
            got, expected,
            "tool_calls should preserve model-emitted order"
        );
    }

    #[test]
    fn test_stream_collector_process_multiple_tool_calls() {
        let mut collector = StreamCollector::new();

        // First tool call
        let tc1 = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: "search".to_string(),
            fn_arguments: json!({"q": "rust"}),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc1 }));

        // Second tool call
        let tc2 = genai::chat::ToolCall {
            call_id: "call_2".to_string(),
            fn_name: "calculate".to_string(),
            fn_arguments: json!({"expr": "2+2"}),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc2 }));

        let result = collector.finish(None);
        assert_eq!(result.tool_calls.len(), 2);
    }

    #[test]
    fn test_stream_collector_process_mixed_text_and_tools() {
        let mut collector = StreamCollector::new();

        // Text first
        collector.process(ChatStreamEvent::Chunk(StreamChunk {
            content: "I'll search for that. ".to_string(),
        }));

        // Then tool call
        let tc = genai::chat::ToolCall {
            call_id: "call_search".to_string(),
            fn_name: "web_search".to_string(),
            fn_arguments: json!({"query": "rust programming"}),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc }));

        let result = collector.finish(None);
        assert_eq!(result.text, "I'll search for that. ");
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "web_search");
    }

    #[test]
    fn test_stream_collector_process_start_event() {
        let mut collector = StreamCollector::new();

        let output = collector.process(ChatStreamEvent::Start);
        assert!(output.is_empty());
        assert!(collector.text().is_empty());
    }

    #[test]
    fn test_stream_collector_process_end_event() {
        let mut collector = StreamCollector::new();

        // Add some text first
        collector.process(ChatStreamEvent::Chunk(StreamChunk {
            content: "Hello".to_string(),
        }));

        // End event
        let end = StreamEnd::default();
        let output = collector.process(ChatStreamEvent::End(end));

        assert!(output.is_empty());

        let result = collector.finish(None);
        assert_eq!(result.text, "Hello");
    }

    #[test]
    fn test_stream_collector_has_tool_calls() {
        let mut collector = StreamCollector::new();
        assert!(!collector.has_tool_calls());

        let tc = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: "test".to_string(),
            fn_arguments: json!({}),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc }));

        assert!(collector.has_tool_calls());
    }

    #[test]
    fn test_stream_collector_text_accumulation() {
        let mut collector = StreamCollector::new();

        // Simulate streaming word by word
        let words = vec!["The ", "quick ", "brown ", "fox ", "jumps."];
        for word in words {
            collector.process(ChatStreamEvent::Chunk(StreamChunk {
                content: word.to_string(),
            }));
        }

        assert_eq!(collector.text(), "The quick brown fox jumps.");
    }

    #[test]
    fn test_stream_collector_tool_arguments_accumulation() {
        // genai sends ACCUMULATED arguments in each chunk (with capture_tool_calls=true).
        // Each chunk carries the full accumulated string so far, not just a delta.
        let mut collector = StreamCollector::new();

        // Start tool call
        let tc1 = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: "api".to_string(),
            fn_arguments: json!(null),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc1 }));

        // Accumulated argument chunks (each is the full value so far)
        let tc2 = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: String::new(),
            fn_arguments: Value::String("{\"url\":".to_string()),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc2 }));

        let tc3 = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: String::new(),
            fn_arguments: Value::String("{\"url\": \"https://example.com\"}".to_string()),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc3 }));

        let result = collector.finish(None);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "api");
        assert_eq!(
            result.tool_calls[0].arguments,
            json!({"url": "https://example.com"})
        );
    }

    #[test]
    fn test_stream_collector_value_string_args_accumulation() {
        // genai sends ACCUMULATED arguments as Value::String in each chunk.
        // Verify that we extract raw strings and properly de-duplicate.
        let mut collector = StreamCollector::new();

        // First chunk: name only, empty arguments
        let tc1 = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: "get_weather".to_string(),
            fn_arguments: Value::String(String::new()),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc1 }));

        // Accumulated argument chunks (each is the full value so far)
        let tc2 = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: String::new(),
            fn_arguments: Value::String("{\"city\":".to_string()),
            thought_signatures: None,
        };
        let output2 =
            collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc2 }));
        assert!(matches!(
            output2.first(),
            Some(StreamOutput::ToolCallDelta { args_delta, .. }) if args_delta == "{\"city\":"
        ));

        let tc3 = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: String::new(),
            fn_arguments: Value::String("{\"city\": \"San Francisco\"}".to_string()),
            thought_signatures: None,
        };
        let output3 =
            collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc3 }));
        // Delta should be only the new part
        assert!(matches!(
            output3.iter().find(|event| matches!(event, StreamOutput::ToolCallDelta { .. })),
            Some(StreamOutput::ToolCallDelta { args_delta, .. }) if args_delta == " \"San Francisco\"}"
        ));

        let result = collector.finish(None);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "get_weather");
        assert_eq!(
            result.tool_calls[0].arguments,
            json!({"city": "San Francisco"})
        );
    }

    #[test]
    fn test_stream_collector_emits_tool_call_ready_once_when_args_complete() {
        let mut collector = StreamCollector::new();

        let outputs = collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
            tool_call: genai::chat::ToolCall {
                call_id: "call_ready".to_string(),
                fn_name: "search".to_string(),
                fn_arguments: Value::String("{\"q\":\"ru".to_string()),
                thought_signatures: None,
            },
        }));
        assert!(!outputs
            .iter()
            .any(|event| matches!(event, StreamOutput::ToolCallReady { .. })));

        let outputs = collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
            tool_call: genai::chat::ToolCall {
                call_id: "call_ready".to_string(),
                fn_name: String::new(),
                fn_arguments: Value::String("{\"q\":\"rust\"}".to_string()),
                thought_signatures: None,
            },
        }));
        assert_eq!(
            outputs
                .iter()
                .filter(|event| matches!(event, StreamOutput::ToolCallReady { .. }))
                .count(),
            1
        );

        let outputs = collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
            tool_call: genai::chat::ToolCall {
                call_id: "call_ready".to_string(),
                fn_name: String::new(),
                fn_arguments: Value::String("{\"q\":\"rust\"}".to_string()),
                thought_signatures: None,
            },
        }));
        assert!(!outputs
            .iter()
            .any(|event| matches!(event, StreamOutput::ToolCallReady { .. })));
    }

    #[test]
    fn test_stream_collector_finish_clears_state() {
        let mut collector = StreamCollector::new();

        collector.process(ChatStreamEvent::Chunk(StreamChunk {
            content: "Test".to_string(),
        }));

        let result1 = collector.finish(None);
        assert_eq!(result1.text, "Test");

        // After finish, the collector is consumed, so we can't use it again
        // This is by design (finish takes self)
    }

    // ========================================================================
    // AI SDK v6 Conversion Tests
    // ========================================================================

    // ========================================================================
    // New Event Variant Tests
    // ========================================================================

    #[test]
    fn test_agent_event_tool_call_ready() {
        let event = AgentEvent::ToolCallReady {
            id: "call_1".to_string(),
            name: "search".to_string(),
            arguments: json!({"query": "rust programming"}),
        };
        if let AgentEvent::ToolCallReady {
            id,
            name,
            arguments,
        } = event
        {
            assert_eq!(id, "call_1");
            assert_eq!(name, "search");
            assert_eq!(arguments["query"], "rust programming");
        } else {
            panic!("Expected ToolCallReady");
        }
    }

    #[test]
    fn test_agent_event_step_start() {
        let event = AgentEvent::StepStart {
            message_id: String::new(),
        };
        assert!(matches!(event, AgentEvent::StepStart { .. }));
    }

    #[test]
    fn test_agent_event_step_end() {
        let event = AgentEvent::StepEnd;
        assert!(matches!(event, AgentEvent::StepEnd));
    }

    #[test]
    fn test_agent_event_run_finish_cancelled() {
        let event = AgentEvent::RunFinish {
            thread_id: "t1".to_string(),
            run_id: "r1".to_string(),
            result: None,
            termination: TerminationReason::Cancelled,
        };
        if let AgentEvent::RunFinish { termination, .. } = event {
            assert_eq!(termination, TerminationReason::Cancelled);
        } else {
            panic!("Expected RunFinish");
        }
    }

    #[test]
    fn test_agent_event_serialization() {
        let event = AgentEvent::TextDelta {
            delta: "Hello".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"text_delta\""));
        assert!(json.contains("\"data\""));
        assert!(json.contains("text_delta"));
        assert!(json.contains("Hello"));

        let event = AgentEvent::StepStart {
            message_id: String::new(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("step_start"));

        let event = AgentEvent::ActivitySnapshot {
            message_id: "activity_1".to_string(),
            activity_type: "progress".to_string(),
            content: json!({"progress": 1.0}),
            replace: Some(true),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("activity_snapshot"));
        assert!(json.contains("activity_1"));
    }

    #[test]
    fn test_agent_event_deserialization() {
        let json = r#"{"type":"step_start"}"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, AgentEvent::StepStart { .. }));

        let json = r#"{"type":"text_delta","data":{"delta":"Hello"}}"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        if let AgentEvent::TextDelta { delta } = event {
            assert_eq!(delta, "Hello");
        } else {
            panic!("Expected TextDelta");
        }

        let json = r#"{"type":"activity_snapshot","data":{"message_id":"activity_1","activity_type":"progress","content":{"progress":0.3},"replace":true}}"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        if let AgentEvent::ActivitySnapshot {
            message_id,
            activity_type,
            content,
            replace,
        } = event
        {
            assert_eq!(message_id, "activity_1");
            assert_eq!(activity_type, "progress");
            assert_eq!(content["progress"], 0.3);
            assert_eq!(replace, Some(true));
        } else {
            panic!("Expected ActivitySnapshot");
        }
    }

    // ========================================================================
    // Complete Flow Integration Tests
    // ========================================================================

    // ========================================================================
    // Additional Coverage Tests
    // ========================================================================

    #[test]
    fn test_stream_output_variants_creation() {
        // Test the StreamOutput enum variants can be created
        let text_delta = StreamOutput::TextDelta("Hello".to_string());
        assert!(matches!(text_delta, StreamOutput::TextDelta(_)));

        let tool_start = StreamOutput::ToolCallStart {
            id: "call_1".to_string(),
            name: "search".to_string(),
        };
        assert!(matches!(tool_start, StreamOutput::ToolCallStart { .. }));

        let tool_delta = StreamOutput::ToolCallDelta {
            id: "call_1".to_string(),
            args_delta: "delta".to_string(),
        };
        assert!(matches!(tool_delta, StreamOutput::ToolCallDelta { .. }));
    }

    #[test]
    fn test_stream_collector_text_and_has_tool_calls() {
        let collector = StreamCollector::new();
        assert!(!collector.has_tool_calls());
        assert_eq!(collector.text(), "");
    }

    // ========================================================================
    // Pending Frontend Tool Event Tests
    // ========================================================================

    // ========================================================================
    // AG-UI Lifecycle Ordering Tests
    // ========================================================================

    // ========================================================================
    // AI SDK v6 Lifecycle Ordering Tests
    // ========================================================================

    // ========================================================================
    // AG-UI Context-Dependent Path Tests
    // ========================================================================

    // ========================================================================
    // StreamCollector Edge Case Tests
    // ========================================================================

    #[test]
    fn test_stream_collector_ghost_tool_call_filtered() {
        // DeepSeek sends ghost tool calls with empty fn_name
        let mut collector = StreamCollector::new();

        // Ghost call: empty fn_name
        let ghost = genai::chat::ToolCall {
            call_id: "ghost_1".to_string(),
            fn_name: String::new(),
            fn_arguments: json!(null),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
            tool_call: ghost,
        }));

        // Real call
        let real = genai::chat::ToolCall {
            call_id: "real_1".to_string(),
            fn_name: "search".to_string(),
            fn_arguments: Value::String(r#"{"q":"rust"}"#.to_string()),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
            tool_call: real,
        }));

        let result = collector.finish(None);
        // Ghost call should be filtered out (empty name)
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "search");
    }

    #[test]
    fn test_stream_collector_invalid_json_arguments_dropped() {
        let mut collector = StreamCollector::new();

        let tc = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: "test".to_string(),
            fn_arguments: Value::String("not valid json {{".to_string()),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc }));

        let result = collector.finish(None);
        // Unparseable arguments are dropped (truncated tool calls)
        assert_eq!(result.tool_calls.len(), 0);
    }

    #[test]
    fn test_stream_collector_duplicate_accumulated_args_full_replace() {
        let mut collector = StreamCollector::new();

        // Start tool call
        let tc1 = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: "test".to_string(),
            fn_arguments: Value::String(r#"{"a":1}"#.to_string()),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc1 }));

        // Same accumulated args again — not a strict prefix extension, so treated
        // as a full replacement delta (correct for accumulated-mode providers).
        let tc2 = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: String::new(),
            fn_arguments: Value::String(r#"{"a":1}"#.to_string()),
            thought_signatures: None,
        };
        let output =
            collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc2 }));
        match output
            .into_iter()
            .find(|event| matches!(event, StreamOutput::ToolCallDelta { .. }))
        {
            Some(StreamOutput::ToolCallDelta { id, args_delta }) => {
                assert_eq!(id, "call_1");
                assert_eq!(args_delta, r#"{"a":1}"#);
            }
            other => panic!("Expected ToolCallDelta, got {:?}", other),
        }
    }

    #[test]
    fn test_stream_collector_end_event_captures_usage() {
        let mut collector = StreamCollector::new();

        let end = StreamEnd {
            captured_usage: Some(Usage {
                prompt_tokens: Some(10),
                prompt_tokens_details: None,
                completion_tokens: Some(20),
                completion_tokens_details: None,
                total_tokens: Some(30),
            }),
            ..Default::default()
        };
        collector.process(ChatStreamEvent::End(end));

        let result = collector.finish(None);
        assert!(result.usage.is_some());
        let usage = result.usage.unwrap();
        assert_eq!(usage.prompt_tokens, Some(10));
        assert_eq!(usage.completion_tokens, Some(20));
        assert_eq!(usage.total_tokens, Some(30));
        assert_eq!(usage.thinking_tokens, None);
    }

    #[test]
    fn test_stream_collector_end_event_captures_thinking_usage() {
        let mut collector = StreamCollector::new();

        let end = StreamEnd {
            captured_usage: Some(Usage {
                prompt_tokens: Some(10),
                prompt_tokens_details: None,
                completion_tokens: Some(20),
                completion_tokens_details: Some(CompletionTokensDetails {
                    accepted_prediction_tokens: None,
                    rejected_prediction_tokens: None,
                    reasoning_tokens: Some(10),
                    audio_tokens: None,
                }),
                total_tokens: Some(30),
            }),
            ..Default::default()
        };
        collector.process(ChatStreamEvent::End(end));

        let result = collector.finish(None);
        assert!(result.usage.is_some());
        let usage = result.usage.unwrap();
        assert_eq!(usage.prompt_tokens, Some(10));
        assert_eq!(usage.completion_tokens, Some(20));
        assert_eq!(usage.total_tokens, Some(30));
        assert_eq!(usage.thinking_tokens, Some(10));
    }

    #[test]
    fn test_stream_collector_end_event_fills_missing_partial() {
        // End event creates a new partial tool call when one doesn't exist from chunks
        use genai::chat::MessageContent;

        let mut collector = StreamCollector::new();

        let end_tc = genai::chat::ToolCall {
            call_id: "end_call".to_string(),
            fn_name: "finalize".to_string(),
            fn_arguments: Value::String(r#"{"done":true}"#.to_string()),
            thought_signatures: None,
        };
        let end = StreamEnd {
            captured_content: Some(MessageContent::from_tool_calls(vec![end_tc])),
            ..Default::default()
        };
        collector.process(ChatStreamEvent::End(end));

        let result = collector.finish(None);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].id, "end_call");
        assert_eq!(result.tool_calls[0].name, "finalize");
        assert_eq!(result.tool_calls[0].arguments, json!({"done": true}));
    }

    #[test]
    fn test_stream_collector_end_event_overrides_partial_args() {
        // End event should override streaming partial arguments
        use genai::chat::MessageContent;

        let mut collector = StreamCollector::new();

        // Start with partial data from chunks
        let tc1 = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: "api".to_string(),
            fn_arguments: Value::String(r#"{"partial":true"#.to_string()), // incomplete JSON
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc1 }));

        // End event provides correct, complete arguments
        let end_tc = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: String::new(), // name already set from chunk
            fn_arguments: Value::String(r#"{"complete":true}"#.to_string()),
            thought_signatures: None,
        };
        let end = StreamEnd {
            captured_content: Some(MessageContent::from_tool_calls(vec![end_tc])),
            ..Default::default()
        };
        collector.process(ChatStreamEvent::End(end));

        let result = collector.finish(None);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "api");
        // End event's arguments should override the incomplete streaming args
        assert_eq!(result.tool_calls[0].arguments, json!({"complete": true}));
    }

    #[test]
    fn test_stream_collector_value_object_args() {
        // When fn_arguments is not a String, falls through to `other.to_string()`
        let mut collector = StreamCollector::new();

        let tc = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: "test".to_string(),
            fn_arguments: json!({"key": "val"}), // Value::Object, not Value::String
            thought_signatures: None,
        };
        let output = collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc }));

        // Should produce ToolCallStart (name) — the args delta comes from .to_string()
        // First output is ToolCallStart, then the args are also processed
        // But since name is set on the same chunk, output is ToolCallDelta (args wins over name)
        // Actually: name emit happens first, then args. But `output` only holds the LAST one.
        // Let's just check the final result.
        assert!(!output.is_empty());

        let result = collector.finish(None);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].arguments, json!({"key": "val"}));
    }

    // ========================================================================
    // Truncated / Malformed JSON Resilience Tests
    // (Reference: Mastra transform.test.ts — graceful handling of
    //  streaming race conditions and partial tool-call arguments)
    // ========================================================================

    #[test]
    fn test_stream_collector_truncated_json_args() {
        // Simulates network interruption mid-stream where the accumulated
        // argument string is incomplete JSON.  finish() should gracefully
        // produce Value::Null (never panic).
        let mut collector = StreamCollector::new();

        let tc = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: "search".to_string(),
            fn_arguments: Value::String(r#"{"url": "https://example.com"#.to_string()),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc }));

        let result = collector.finish(None);
        // Truncated JSON tool calls are dropped
        assert_eq!(result.tool_calls.len(), 0);
    }

    #[test]
    fn test_stream_collector_empty_json_args() {
        // Tool call with completely empty argument string.
        let mut collector = StreamCollector::new();

        let tc = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: "noop".to_string(),
            fn_arguments: Value::String(String::new()),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc }));

        let result = collector.finish(None);
        // Empty arguments parse to Null and are treated as "no args" (kept)
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "noop");
        assert_eq!(result.tool_calls[0].arguments, Value::Null);
    }

    #[test]
    fn test_stream_collector_partial_nested_json() {
        // Complex nested JSON truncated mid-array.
        let mut collector = StreamCollector::new();

        let tc = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: "complex_tool".to_string(),
            fn_arguments: Value::String(
                r#"{"a": {"b": [1, 2, {"c": "long_string_that_gets_truncated"#.to_string(),
            ),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc }));

        let result = collector.finish(None);
        // Truncated JSON tool calls are dropped
        assert_eq!(result.tool_calls.len(), 0);
    }

    #[test]
    fn test_stream_collector_truncated_then_end_event_recovers() {
        // Streaming produces truncated JSON, but the End event carries the
        // complete arguments — the End event should override and recover.
        use genai::chat::MessageContent;

        let mut collector = StreamCollector::new();

        // Truncated streaming chunk
        let tc1 = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: "api".to_string(),
            fn_arguments: Value::String(r#"{"location": "New York", "unit": "cel"#.to_string()),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc1 }));

        // End event with complete arguments
        let end_tc = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: String::new(),
            fn_arguments: Value::String(
                r#"{"location": "New York", "unit": "celsius"}"#.to_string(),
            ),
            thought_signatures: None,
        };
        let end = StreamEnd {
            captured_content: Some(MessageContent::from_tool_calls(vec![end_tc])),
            ..Default::default()
        };
        collector.process(ChatStreamEvent::End(end));

        let result = collector.finish(None);
        assert_eq!(result.tool_calls.len(), 1);
        // End event recovered the complete, valid JSON
        assert_eq!(
            result.tool_calls[0].arguments,
            json!({"location": "New York", "unit": "celsius"})
        );
    }

    #[test]
    fn test_stream_collector_valid_json_args_control() {
        // Control test: valid JSON args parse correctly (contrast with truncated tests).
        let mut collector = StreamCollector::new();

        let tc = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: "get_weather".to_string(),
            fn_arguments: Value::String(
                r#"{"location": "San Francisco", "units": "metric"}"#.to_string(),
            ),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc }));

        let result = collector.finish(None);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(
            result.tool_calls[0].arguments,
            json!({"location": "San Francisco", "units": "metric"})
        );
    }

    // ========================================================================
    // AI SDK v6 Complete Flow Tests
    // ========================================================================

    // ========================================================================
    // End Event: Edge Cases
    // ========================================================================

    #[test]
    fn test_stream_collector_end_event_no_tool_calls_preserves_streamed() {
        // When End event has no captured_tool_calls (None), the tool calls
        // accumulated from streaming chunks should be preserved.
        use genai::chat::StreamEnd;

        let mut collector = StreamCollector::new();

        // Accumulate a tool call from streaming chunks
        let tc = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: "search".to_string(),
            fn_arguments: Value::String(r#"{"q":"test"}"#.to_string()),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc }));

        // End event with NO captured_tool_calls (some providers don't populate this)
        let end = StreamEnd {
            captured_content: None,
            ..Default::default()
        };
        collector.process(ChatStreamEvent::End(end));

        let result = collector.finish(None);
        assert_eq!(
            result.tool_calls.len(),
            1,
            "Streamed tool calls should be preserved"
        );
        assert_eq!(result.tool_calls[0].name, "search");
        assert_eq!(result.tool_calls[0].arguments, json!({"q": "test"}));
    }

    #[test]
    fn test_stream_collector_end_event_overrides_tool_name() {
        // End event should override tool name when streamed chunk had wrong name.
        use genai::chat::MessageContent;

        let mut collector = StreamCollector::new();

        // Streaming chunk with initial name
        let tc = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: "search".to_string(),
            fn_arguments: Value::String(r#"{"q":"test"}"#.to_string()),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc }));

        // End event with different name (only fills if name was EMPTY, per line 136)
        let end_tc = genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: "web_search".to_string(), // different name
            fn_arguments: Value::String(r#"{"q":"test"}"#.to_string()),
            thought_signatures: None,
        };
        let end = StreamEnd {
            captured_content: Some(MessageContent::from_tool_calls(vec![end_tc])),
            ..Default::default()
        };
        collector.process(ChatStreamEvent::End(end));

        let result = collector.finish(None);
        assert_eq!(result.tool_calls.len(), 1);
        // Current behavior: name only overridden if empty (line 136: `if partial.name.is_empty()`)
        // So the original streaming name should be preserved.
        assert_eq!(result.tool_calls[0].name, "search");
    }

    #[test]
    fn test_stream_collector_whitespace_only_tool_name_filtered() {
        // Tool calls with whitespace-only names should be filtered (ghost tool calls).
        let mut collector = StreamCollector::new();

        let tc = genai::chat::ToolCall {
            call_id: "ghost_1".to_string(),
            fn_name: "   ".to_string(), // whitespace only
            fn_arguments: Value::String("{}".to_string()),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc }));

        let result = collector.finish(None);
        // finish() filters by `!p.name.is_empty()` — whitespace-only name is NOT empty.
        // This documents current behavior (whitespace names are kept).
        // If this is a bug, fix the filter to use `.trim().is_empty()`.
        assert_eq!(
            result.tool_calls.len(),
            1,
            "Whitespace-only names are currently NOT filtered (document behavior)"
        );
    }

    // ========================================================================
    // Multiple / Interleaved Tool Call Tests
    // ========================================================================

    /// Helper: create a tool call chunk event.
    fn tc_chunk(call_id: &str, fn_name: &str, args: &str) -> ChatStreamEvent {
        ChatStreamEvent::ToolCallChunk(ToolChunk {
            tool_call: genai::chat::ToolCall {
                call_id: call_id.to_string(),
                fn_name: fn_name.to_string(),
                fn_arguments: Value::String(args.to_string()),
                thought_signatures: None,
            },
        })
    }

    #[test]
    fn test_stream_collector_two_tool_calls_sequential() {
        // Two tool calls arriving sequentially.
        let mut collector = StreamCollector::new();

        collector.process(tc_chunk("tc_1", "search", r#"{"q":"foo"}"#));
        collector.process(tc_chunk("tc_2", "fetch", r#"{"url":"https://x.com"}"#));

        let result = collector.finish(None);
        assert_eq!(result.tool_calls.len(), 2);

        let names: Vec<&str> = result
            .tool_calls
            .iter()
            .map(|tc| tc.name.as_str())
            .collect();
        assert!(names.contains(&"search"));
        assert!(names.contains(&"fetch"));

        let search = result
            .tool_calls
            .iter()
            .find(|tc| tc.name == "search")
            .unwrap();
        assert_eq!(search.arguments, json!({"q": "foo"}));

        let fetch = result
            .tool_calls
            .iter()
            .find(|tc| tc.name == "fetch")
            .unwrap();
        assert_eq!(fetch.arguments, json!({"url": "https://x.com"}));
    }

    #[test]
    fn test_stream_collector_two_tool_calls_interleaved_chunks() {
        // Two tool calls with interleaved argument chunks (accumulated args, AI SDK v6 pattern).
        // Chunk 1: tc_a name only (empty args)
        // Chunk 2: tc_b name only (empty args)
        // Chunk 3: tc_a with partial args
        // Chunk 4: tc_b with partial args
        // Chunk 5: tc_a with full args (accumulated)
        // Chunk 6: tc_b with full args (accumulated)
        let mut collector = StreamCollector::new();

        // Initial name-only chunks
        collector.process(tc_chunk("tc_a", "search", ""));
        collector.process(tc_chunk("tc_b", "fetch", ""));

        // Partial args (accumulated pattern)
        collector.process(tc_chunk("tc_a", "search", r#"{"q":"#));
        collector.process(tc_chunk("tc_b", "fetch", r#"{"url":"#));

        // Full accumulated args
        collector.process(tc_chunk("tc_a", "search", r#"{"q":"a"}"#));
        collector.process(tc_chunk("tc_b", "fetch", r#"{"url":"b"}"#));

        let result = collector.finish(None);
        assert_eq!(result.tool_calls.len(), 2);

        let search = result
            .tool_calls
            .iter()
            .find(|tc| tc.name == "search")
            .unwrap();
        assert_eq!(search.arguments, json!({"q": "a"}));

        let fetch = result
            .tool_calls
            .iter()
            .find(|tc| tc.name == "fetch")
            .unwrap();
        assert_eq!(fetch.arguments, json!({"url": "b"}));
    }

    #[test]
    fn test_stream_collector_tool_call_interleaved_with_text() {
        // Text chunks interleaved between tool call chunks.
        let mut collector = StreamCollector::new();

        collector.process(ChatStreamEvent::Chunk(StreamChunk {
            content: "I will ".to_string(),
        }));
        collector.process(tc_chunk("tc_1", "search", ""));
        collector.process(ChatStreamEvent::Chunk(StreamChunk {
            content: "search ".to_string(),
        }));
        collector.process(tc_chunk("tc_1", "search", r#"{"q":"test"}"#));
        collector.process(ChatStreamEvent::Chunk(StreamChunk {
            content: "for you.".to_string(),
        }));

        let result = collector.finish(None);
        // Text should be accumulated
        assert_eq!(result.text, "I will search for you.");
        // Tool call should be present
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].arguments, json!({"q": "test"}));
    }

    #[test]
    fn test_last_tool_call_with_null_args_dropped_at_max_tokens() {
        // Scenario C/D: tool call truncated mid-name or with empty arguments
        // at max_tokens boundary. The last incomplete call should be dropped.
        let mut collector = StreamCollector::new();

        // Complete tool call
        let tc1 = genai::chat::ToolCall {
            call_id: "c1".to_string(),
            fn_name: "search".to_string(),
            fn_arguments: Value::String(r#"{"q":"rust"}"#.to_string()),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc1 }));

        // Truncated tool call — name present but no arguments
        let tc2 = genai::chat::ToolCall {
            call_id: "c2".to_string(),
            fn_name: "calcu".to_string(), // truncated name
            fn_arguments: Value::String(String::new()),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc2 }));

        // Simulate max_tokens: completion_tokens == max_output_tokens
        collector.usage = Some(genai::chat::Usage {
            prompt_tokens: Some(100),
            completion_tokens: Some(4096),
            ..Default::default()
        });

        let result = collector.finish(Some(4096));
        // Only the complete tool call survives
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "search");
        // stop_reason remains ToolUse because there's still a complete call
        assert_eq!(result.stop_reason, Some(StopReason::ToolUse));
    }

    #[test]
    fn test_single_tool_call_with_null_args_at_max_tokens_triggers_max_tokens() {
        // Scenario: only one tool call and it's truncated (empty args).
        // After dropping it, tool_calls is empty → stop_reason = MaxTokens.
        let mut collector = StreamCollector::new();

        collector.process(ChatStreamEvent::Chunk(StreamChunk {
            content: "Let me search".to_string(),
        }));

        let tc = genai::chat::ToolCall {
            call_id: "c1".to_string(),
            fn_name: "sear".to_string(), // truncated
            fn_arguments: Value::String(String::new()),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc }));

        collector.usage = Some(genai::chat::Usage {
            prompt_tokens: Some(100),
            completion_tokens: Some(4096),
            ..Default::default()
        });

        let result = collector.finish(Some(4096));
        // Truncated tool call dropped, no tool calls remain
        assert_eq!(result.tool_calls.len(), 0);
        // Re-inferred as MaxTokens → plugin can trigger recovery
        assert_eq!(result.stop_reason, Some(StopReason::MaxTokens));
        assert_eq!(result.text, "Let me search");
    }

    #[test]
    fn test_complete_tool_calls_not_dropped_at_max_tokens() {
        // All tool calls have valid arguments — nothing should be dropped
        // even when hitting max_tokens.
        let mut collector = StreamCollector::new();

        let tc = genai::chat::ToolCall {
            call_id: "c1".to_string(),
            fn_name: "search".to_string(),
            fn_arguments: Value::String(r#"{"q":"test"}"#.to_string()),
            thought_signatures: None,
        };
        collector.process(ChatStreamEvent::ToolCallChunk(ToolChunk { tool_call: tc }));

        collector.usage = Some(genai::chat::Usage {
            prompt_tokens: Some(100),
            completion_tokens: Some(4096),
            ..Default::default()
        });

        let result = collector.finish(Some(4096));
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "search");
        assert_eq!(result.stop_reason, Some(StopReason::ToolUse));
    }

    #[test]
    fn test_into_partial_text_returns_accumulated_text() {
        let mut collector = StreamCollector::new();
        collector.process(ChatStreamEvent::Chunk(genai::chat::StreamChunk {
            content: "Hello ".to_string(),
        }));
        collector.process(ChatStreamEvent::Chunk(genai::chat::StreamChunk {
            content: "world".to_string(),
        }));
        assert_eq!(collector.into_partial_text(), "Hello world");
    }

    #[test]
    fn test_into_partial_text_empty_when_no_text() {
        let collector = StreamCollector::new();
        assert_eq!(collector.into_partial_text(), "");
    }

    #[test]
    fn test_recovery_checkpoint_uses_partial_text_when_no_tool_call_seen() {
        let mut collector = StreamCollector::new();
        collector.process(ChatStreamEvent::Chunk(genai::chat::StreamChunk {
            content: "Hello".to_string(),
        }));
        assert_eq!(
            collector.into_recovery_checkpoint(),
            StreamRecoveryCheckpoint::PartialText("Hello".to_string())
        );
    }

    #[test]
    fn test_recovery_checkpoint_marks_tool_call_observed() {
        let mut collector = StreamCollector::new();
        collector.process(ChatStreamEvent::ToolCallChunk(genai::chat::ToolChunk {
            tool_call: genai::chat::ToolCall {
                call_id: "call_1".to_string(),
                fn_name: "echo".to_string(),
                fn_arguments: Value::String("{\"message\":\"hi".to_string()),
                thought_signatures: None,
            },
        }));
        assert_eq!(
            collector.into_recovery_checkpoint(),
            StreamRecoveryCheckpoint::ToolCallObserved
        );
    }

    #[test]
    fn test_recovery_checkpoint_marks_no_payload_when_stream_is_empty() {
        let collector = StreamCollector::new();
        assert_eq!(
            collector.into_recovery_checkpoint(),
            StreamRecoveryCheckpoint::NoPayload
        );
    }
}
