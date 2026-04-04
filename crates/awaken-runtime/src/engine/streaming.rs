//! Streaming response collector: accumulates genai ChatStreamEvents into a StreamResult.

use std::collections::{HashMap, VecDeque};

use genai::chat::{ChatStreamEvent, StreamEnd};
use serde_json::Value;

use awaken_contract::contract::inference::{StreamResult, TokenUsage};
use awaken_contract::contract::message::ToolCall;

use super::convert::{map_stop_reason, map_usage};

/// Output from processing a single stream event.
pub enum StreamOutput {
    /// Text content delta.
    TextDelta(String),
    /// Reasoning content delta.
    ReasoningDelta(String),
    /// A new tool call was started.
    ToolCallStart { id: String, name: String },
    /// Tool call arguments delta (incremental JSON fragment).
    ToolCallDelta { id: String, args_delta: String },
    /// Nothing to emit for this event.
    None,
}

/// Accumulates streaming events into a final `StreamResult`.
pub struct StreamCollector {
    text: String,
    tool_calls: HashMap<String, PartialToolCall>,
    tool_call_order: Vec<String>,
    usage: Option<TokenUsage>,
    stop_reason: Option<awaken_contract::contract::inference::StopReason>,
    pending_outputs: VecDeque<StreamOutput>,
    /// Set to true after `ChatStreamEvent::End` is processed.
    end_seen: bool,
}

struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
    start_emitted: bool,
}

impl Default for StreamCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamCollector {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            end_seen: false,
            tool_calls: HashMap::new(),
            tool_call_order: Vec::new(),
            usage: None,
            stop_reason: None,
            pending_outputs: VecDeque::new(),
        }
    }

    /// Take the next deferred output produced by the previous provider event, if any.
    pub fn take_pending_output(&mut self) -> Option<StreamOutput> {
        self.pending_outputs.pop_front()
    }

    /// Process a genai stream event. Returns what to emit to the event sink.
    pub fn process(&mut self, event: ChatStreamEvent) -> StreamOutput {
        match event {
            ChatStreamEvent::Start => StreamOutput::None,

            ChatStreamEvent::Chunk(chunk) => {
                self.text.push_str(&chunk.content);
                StreamOutput::TextDelta(chunk.content)
            }

            ChatStreamEvent::ReasoningChunk(chunk) => StreamOutput::ReasoningDelta(chunk.content),

            ChatStreamEvent::ThoughtSignatureChunk(_) => StreamOutput::None,

            ChatStreamEvent::ToolCallChunk(tool_chunk) => {
                let call = &tool_chunk.tool_call;
                let id = call.call_id.clone();

                let existing = self.tool_calls.get(&id);
                let prev_args_len = existing.map(|e| e.arguments.len()).unwrap_or(0);

                let entry = self.tool_calls.entry(id.clone()).or_insert_with(|| {
                    self.tool_call_order.push(id.clone());
                    PartialToolCall {
                        id: id.clone(),
                        name: String::new(),
                        arguments: String::new(),
                        start_emitted: false,
                    }
                });

                if !call.fn_name.is_empty() {
                    entry.name = call.fn_name.clone();
                }

                // genai provides accumulated arguments as Value::String; extract
                // the raw string to avoid double-encoding via serde_json::to_string
                // (which would add JSON quotes/escapes and corrupt delta computation).
                let args_str = match &call.fn_arguments {
                    Value::String(s) => s.clone(),
                    other => serde_json::to_string(other).unwrap_or_default(),
                };
                let delta = if args_str.len() > prev_args_len {
                    args_str[prev_args_len..].to_string()
                } else {
                    String::new()
                };
                entry.arguments = args_str;

                let should_emit_start = !entry.start_emitted && !entry.name.is_empty();
                if should_emit_start {
                    entry.start_emitted = true;
                    if tool_args_have_content(&call.fn_arguments) {
                        self.pending_outputs.push_back(StreamOutput::ToolCallDelta {
                            id: id.clone(),
                            args_delta: entry.arguments.clone(),
                        });
                    }
                    StreamOutput::ToolCallStart {
                        id,
                        name: entry.name.clone(),
                    }
                } else if entry.start_emitted && !delta.is_empty() {
                    StreamOutput::ToolCallDelta {
                        id,
                        args_delta: delta,
                    }
                } else {
                    StreamOutput::None
                }
            }

            ChatStreamEvent::End(end) => {
                self.apply_end(end);
                StreamOutput::None
            }
        }
    }

    /// Take accumulated usage data, if any. Returns `Some` once, then `None`.
    pub fn take_usage(&mut self) -> Option<TokenUsage> {
        self.usage.take()
    }

    /// Whether `ChatStreamEvent::End` has been processed.
    pub fn end_seen(&self) -> bool {
        self.end_seen
    }

    fn apply_end(&mut self, end: StreamEnd) {
        self.end_seen = true;
        // Usage
        if let Some(ref usage) = end.captured_usage {
            self.usage = Some(map_usage(usage));
        }

        // Stop reason
        if let Some(ref reason) = end.captured_stop_reason {
            self.stop_reason = map_stop_reason(reason);
        }

        // Captured tool calls override streaming chunks (source of truth)
        if let Some(captured) = end.captured_tool_calls() {
            self.tool_calls.clear();
            self.tool_call_order.clear();
            for call in captured {
                let id = call.call_id.clone();
                self.tool_call_order.push(id.clone());
                self.tool_calls.insert(
                    id.clone(),
                    PartialToolCall {
                        id,
                        name: call.fn_name.clone(),
                        arguments: serde_json::to_string(&call.fn_arguments).unwrap_or_default(),
                        start_emitted: true,
                    },
                );
            }
        }

        // Captured text overrides accumulated text
        if let Some(text) = end.captured_first_text() {
            self.text = text.to_string();
        }
    }

    /// Finalize the collector into a `StreamResult`.
    pub fn finish(self) -> StreamResult {
        let mut tool_calls: Vec<ToolCall> = Vec::with_capacity(self.tool_call_order.len());
        let mut has_incomplete_tool_calls = false;

        let mut remaining = self.tool_calls;
        for call_id in &self.tool_call_order {
            let Some(p) = remaining.remove(call_id) else {
                continue;
            };
            if p.name.is_empty() {
                has_incomplete_tool_calls = true;
                continue;
            }
            let arguments: Value = serde_json::from_str(&p.arguments).unwrap_or(Value::Null);
            // Drop tool calls with unparseable arguments (truncated JSON)
            if arguments.is_null() && !p.arguments.is_empty() {
                has_incomplete_tool_calls = true;
                continue;
            }
            tool_calls.push(ToolCall::new(p.id, p.name, arguments));
        }

        let content = if self.text.is_empty() {
            vec![]
        } else {
            vec![awaken_contract::contract::content::ContentBlock::text(
                self.text,
            )]
        };

        StreamResult {
            content,
            tool_calls,
            usage: self.usage,
            stop_reason: self.stop_reason,
            has_incomplete_tool_calls,
        }
    }
}

fn tool_args_have_content(arguments: &Value) -> bool {
    match arguments {
        Value::Null => false,
        Value::String(s) => !s.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::chat::{StreamChunk, ToolChunk};

    #[test]
    fn text_chunks_accumulate() {
        let mut c = StreamCollector::new();
        let o1 = c.process(ChatStreamEvent::Chunk(StreamChunk {
            content: "Hello ".into(),
        }));
        assert!(matches!(o1, StreamOutput::TextDelta(ref s) if s == "Hello "));

        let o2 = c.process(ChatStreamEvent::Chunk(StreamChunk {
            content: "world".into(),
        }));
        assert!(matches!(o2, StreamOutput::TextDelta(ref s) if s == "world"));

        let result = c.finish();
        assert_eq!(result.text(), "Hello world");
    }

    #[test]
    fn tool_call_chunks_accumulate() {
        use genai::chat::{ToolCall as GToolCall, ToolChunk};

        let mut c = StreamCollector::new();

        // First chunk: tool call start
        let o1 = c.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
            tool_call: GToolCall {
                call_id: "c1".into(),
                fn_name: "search".into(),
                fn_arguments: serde_json::json!({}),
                thought_signatures: None,
            },
        }));
        assert!(
            matches!(o1, StreamOutput::ToolCallStart { ref id, ref name } if id == "c1" && name == "search")
        );

        let result = c.finish();
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "search");
    }

    #[test]
    fn truncated_tool_call_json_dropped() {
        let mut c = StreamCollector::new();

        // Manually insert a partial tool call with truncated JSON
        c.tool_call_order.push("c1".into());
        c.tool_calls.insert(
            "c1".into(),
            super::PartialToolCall {
                id: "c1".into(),
                name: "search".into(),
                arguments: r#"{"query": "rust"#.into(), // truncated JSON
                start_emitted: true,
            },
        );

        let result = c.finish();
        // Truncated JSON should be dropped
        assert!(result.tool_calls.is_empty());
    }

    #[test]
    fn captured_end_overrides_streamed_text() {
        use genai::chat::{StreamChunk, StreamEnd};

        let mut c = StreamCollector::new();

        // Stream some text
        c.process(ChatStreamEvent::Chunk(StreamChunk {
            content: "partial".into(),
        }));

        // End event with captured text that overrides
        let end = StreamEnd {
            captured_content: Some(genai::chat::MessageContent::from_text("final text")),
            ..Default::default()
        };
        c.process(ChatStreamEvent::End(end));

        let result = c.finish();
        assert_eq!(result.text(), "final text");
    }

    #[test]
    fn usage_mapped_from_end_event() {
        use genai::chat::{StreamEnd, Usage};

        let mut c = StreamCollector::new();

        let end = StreamEnd {
            captured_usage: Some(Usage {
                prompt_tokens: Some(100),
                completion_tokens: Some(50),
                total_tokens: Some(150),
                ..Default::default()
            }),
            ..Default::default()
        };
        c.process(ChatStreamEvent::End(end));

        let result = c.finish();
        let usage = result.usage.unwrap();
        assert_eq!(usage.prompt_tokens, Some(100));
        assert_eq!(usage.completion_tokens, Some(50));
        assert_eq!(usage.total_tokens, Some(150));
    }

    #[test]
    fn finish_with_no_events_returns_empty() {
        let c = StreamCollector::new();
        let result = c.finish();
        assert!(result.content.is_empty());
        assert!(result.tool_calls.is_empty());
        assert!(result.usage.is_none());
    }

    // -- Task 2 tests --------------------------------------------------------

    #[test]
    fn collector_accumulates_text_deltas() {
        let mut c = StreamCollector::new();
        let chunks = ["The ", "quick ", "brown ", "fox"];
        for chunk in &chunks {
            let out = c.process(ChatStreamEvent::Chunk(StreamChunk {
                content: (*chunk).into(),
            }));
            assert!(matches!(out, StreamOutput::TextDelta(ref s) if s == chunk));
        }
        let result = c.finish();
        assert_eq!(result.text(), "The quick brown fox");
        assert_eq!(result.content.len(), 1);
    }

    #[test]
    fn collector_tracks_tool_call_start_and_delta() {
        use genai::chat::ToolCall as GToolCall;

        let mut c = StreamCollector::new();

        // First chunk: start with empty args
        let o1 = c.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
            tool_call: GToolCall {
                call_id: "tc1".into(),
                fn_name: "get_weather".into(),
                fn_arguments: serde_json::json!({}),
                thought_signatures: None,
            },
        }));
        assert!(
            matches!(o1, StreamOutput::ToolCallStart { ref id, ref name }
                if id == "tc1" && name == "get_weather")
        );

        // Second chunk: accumulated args grow
        let o2 = c.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
            tool_call: GToolCall {
                call_id: "tc1".into(),
                fn_name: "get_weather".into(),
                fn_arguments: serde_json::json!({"city": "London"}),
                thought_signatures: None,
            },
        }));
        assert!(matches!(o2, StreamOutput::ToolCallDelta { ref id, .. } if id == "tc1"));

        let result = c.finish();
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "get_weather");
        assert_eq!(result.tool_calls[0].arguments["city"], "London");
    }

    #[test]
    fn collector_handles_usage_event() {
        use genai::chat::{StreamEnd, Usage};

        let mut c = StreamCollector::new();

        let end = StreamEnd {
            captured_usage: Some(Usage {
                prompt_tokens: Some(200),
                completion_tokens: Some(80),
                total_tokens: Some(280),
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
            ..Default::default()
        };
        c.process(ChatStreamEvent::End(end));

        let result = c.finish();
        let usage = result.usage.expect("usage should be present");
        assert_eq!(usage.prompt_tokens, Some(200));
        assert_eq!(usage.completion_tokens, Some(80));
        assert_eq!(usage.total_tokens, Some(280));
        assert!(usage.cache_read_tokens.is_none());
        assert!(usage.thinking_tokens.is_none());
    }

    #[test]
    fn collector_handles_stop_event() {
        use genai::chat::StreamEnd;

        let mut c = StreamCollector::new();

        let end = StreamEnd {
            captured_stop_reason: Some(genai::chat::StopReason::Completed("stop".into())),
            ..Default::default()
        };
        c.process(ChatStreamEvent::End(end));

        let result = c.finish();
        assert_eq!(
            result.stop_reason,
            Some(awaken_contract::contract::inference::StopReason::EndTurn)
        );
    }

    #[test]
    fn collector_drops_truncated_tool_calls() {
        let mut c = StreamCollector::new();

        // Insert two tool calls: one valid, one with truncated JSON
        c.tool_call_order.push("valid".into());
        c.tool_calls.insert(
            "valid".into(),
            PartialToolCall {
                id: "valid".into(),
                name: "search".into(),
                arguments: r#"{"q":"hello"}"#.into(),
                start_emitted: true,
            },
        );

        c.tool_call_order.push("bad".into());
        c.tool_calls.insert(
            "bad".into(),
            PartialToolCall {
                id: "bad".into(),
                name: "calc".into(),
                arguments: r#"{"expr": "2+"#.into(), // truncated
                start_emitted: true,
            },
        );

        let result = c.finish();
        // Only the valid tool call should survive
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].id, "valid");
    }

    #[test]
    fn collector_preserves_tool_call_order() {
        use genai::chat::ToolCall as GToolCall;

        let mut c = StreamCollector::new();

        let tool_names = ["alpha", "beta", "gamma"];
        for (i, name) in tool_names.iter().enumerate() {
            c.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
                tool_call: GToolCall {
                    call_id: format!("c{i}"),
                    fn_name: (*name).into(),
                    fn_arguments: serde_json::json!({}),
                    thought_signatures: None,
                },
            }));
        }

        let result = c.finish();
        assert_eq!(result.tool_calls.len(), 3);
        assert_eq!(result.tool_calls[0].name, "alpha");
        assert_eq!(result.tool_calls[1].name, "beta");
        assert_eq!(result.tool_calls[2].name, "gamma");
    }

    #[test]
    fn collector_nameless_tool_call_dropped() {
        let mut c = StreamCollector::new();
        c.tool_call_order.push("c1".into());
        c.tool_calls.insert(
            "c1".into(),
            PartialToolCall {
                id: "c1".into(),
                name: String::new(), // no name
                arguments: r#"{"x":1}"#.into(),
                start_emitted: false,
            },
        );

        let result = c.finish();
        assert!(result.tool_calls.is_empty());
    }

    #[test]
    fn collector_reasoning_delta_not_accumulated_in_text() {
        let mut c = StreamCollector::new();

        c.process(ChatStreamEvent::Chunk(StreamChunk {
            content: "visible".into(),
        }));
        let out = c.process(ChatStreamEvent::ReasoningChunk(StreamChunk {
            content: "thinking...".into(),
        }));
        assert!(matches!(out, StreamOutput::ReasoningDelta(ref s) if s == "thinking..."));

        let result = c.finish();
        // Reasoning should NOT be in the accumulated text
        assert_eq!(result.text(), "visible");
    }

    #[test]
    fn collector_start_event_emits_none() {
        let mut c = StreamCollector::new();
        let out = c.process(ChatStreamEvent::Start);
        assert!(matches!(out, StreamOutput::None));
    }

    #[test]
    fn collector_end_captured_tool_calls_override_streamed() {
        use genai::chat::{MessageContent, StreamEnd, ToolCall as GToolCall};

        let mut c = StreamCollector::new();

        // Stream a tool call chunk first
        c.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
            tool_call: GToolCall {
                call_id: "streamed".into(),
                fn_name: "old_tool".into(),
                fn_arguments: serde_json::json!({}),
                thought_signatures: None,
            },
        }));

        // End event with captured tool calls that override
        let captured_call = GToolCall {
            call_id: "captured".into(),
            fn_name: "new_tool".into(),
            fn_arguments: serde_json::json!({"a": 1}),
            thought_signatures: None,
        };
        let end = StreamEnd {
            captured_content: Some(MessageContent::from_tool_calls(vec![captured_call])),
            ..Default::default()
        };
        c.process(ChatStreamEvent::End(end));

        let result = c.finish();
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "new_tool");
        assert_eq!(result.tool_calls[0].id, "captured");
    }

    /// Verify that Value::String fn_arguments (genai OpenAI streaming format)
    /// are handled correctly without double-encoding via serde_json::to_string.
    #[test]
    fn collector_handles_string_valued_accumulated_tool_args() {
        use genai::chat::ToolCall as GToolCall;

        let mut c = StreamCollector::new();

        // Chunk 1: tool call start with empty accumulated args (Value::String(""))
        let o1 = c.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
            tool_call: GToolCall {
                call_id: "tc1".into(),
                fn_name: "calculator".into(),
                fn_arguments: Value::String(String::new()),
                thought_signatures: None,
            },
        }));
        assert!(matches!(o1, StreamOutput::ToolCallStart { ref name, .. } if name == "calculator"));

        // Chunk 2: accumulated args = r#"{"ex"#
        let o2 = c.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
            tool_call: GToolCall {
                call_id: "tc1".into(),
                fn_name: "calculator".into(),
                fn_arguments: Value::String(r#"{"ex"#.into()),
                thought_signatures: None,
            },
        }));
        assert!(matches!(o2, StreamOutput::ToolCallDelta { ref id, .. } if id == "tc1"));

        // Chunk 3: accumulated args = r#"{"expression":"137*42"}"#
        let o3 = c.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
            tool_call: GToolCall {
                call_id: "tc1".into(),
                fn_name: "calculator".into(),
                fn_arguments: Value::String(r#"{"expression":"137*42"}"#.into()),
                thought_signatures: None,
            },
        }));
        assert!(matches!(o3, StreamOutput::ToolCallDelta { ref id, .. } if id == "tc1"));

        let result = c.finish();
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "calculator");
        assert_eq!(result.tool_calls[0].arguments["expression"], "137*42");
    }

    #[test]
    fn collector_emits_initial_object_args_after_tool_call_start() {
        use genai::chat::ToolCall as GToolCall;

        let mut c = StreamCollector::new();

        let start = c.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
            tool_call: GToolCall {
                call_id: "tc1".into(),
                fn_name: "render_openui_ui".into(),
                fn_arguments: serde_json::json!({"prompt": "build a dashboard"}),
                thought_signatures: None,
            },
        }));

        assert!(matches!(
            start,
            StreamOutput::ToolCallStart { ref id, ref name }
                if id == "tc1" && name == "render_openui_ui"
        ));

        let delta = c
            .take_pending_output()
            .expect("initial tool arguments should be preserved");
        assert!(matches!(
            delta,
            StreamOutput::ToolCallDelta {
                ref id,
                ref args_delta
            } if id == "tc1" && args_delta == r#"{"prompt":"build a dashboard"}"#
        ));
        assert!(c.take_pending_output().is_none());

        let result = c.finish();
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "render_openui_ui");
        assert_eq!(
            result.tool_calls[0].arguments["prompt"],
            "build a dashboard"
        );
    }

    #[test]
    fn collector_replays_buffered_args_when_tool_name_arrives_late() {
        use genai::chat::ToolCall as GToolCall;

        let mut c = StreamCollector::new();

        let first = c.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
            tool_call: GToolCall {
                call_id: "tc1".into(),
                fn_name: String::new(),
                fn_arguments: Value::String(r#"{"prompt":"late"}"#.into()),
                thought_signatures: None,
            },
        }));
        assert!(matches!(first, StreamOutput::None));
        assert!(c.take_pending_output().is_none());

        let second = c.process(ChatStreamEvent::ToolCallChunk(ToolChunk {
            tool_call: GToolCall {
                call_id: "tc1".into(),
                fn_name: "render_openui_ui".into(),
                fn_arguments: Value::String(r#"{"prompt":"late"}"#.into()),
                thought_signatures: None,
            },
        }));
        assert!(matches!(
            second,
            StreamOutput::ToolCallStart { ref id, ref name }
                if id == "tc1" && name == "render_openui_ui"
        ));

        let delta = c
            .take_pending_output()
            .expect("buffered arguments should be replayed after ToolCallStart");
        assert!(matches!(
            delta,
            StreamOutput::ToolCallDelta {
                ref id,
                ref args_delta
            } if id == "tc1" && args_delta == r#"{"prompt":"late"}"#
        ));
    }
}
