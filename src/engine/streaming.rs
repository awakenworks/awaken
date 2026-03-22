//! Streaming response collector: accumulates genai ChatStreamEvents into a StreamResult.

use std::collections::HashMap;

use genai::chat::{ChatStreamEvent, StreamEnd};
use serde_json::Value;

use crate::contract::inference::StreamResult;
use crate::contract::message::ToolCall;

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
    stop_reason: Option<crate::contract::inference::StopReason>,
}

struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl StreamCollector {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            tool_calls: HashMap::new(),
            tool_call_order: Vec::new(),
            usage: None,
            stop_reason: None,
        }
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
                let is_new = existing.is_none();

                let entry = self.tool_calls.entry(id.clone()).or_insert_with(|| {
                    self.tool_call_order.push(id.clone());
                    PartialToolCall {
                        id: id.clone(),
                        name: String::new(),
                        arguments: String::new(),
                    }
                });

                if !call.fn_name.is_empty() {
                    entry.name = call.fn_name.clone();
                }

                // genai provides accumulated arguments, compute delta
                let args_str = serde_json::to_string(&call.fn_arguments).unwrap_or_default();
                let delta = if args_str.len() > prev_args_len {
                    args_str[prev_args_len..].to_string()
                } else {
                    String::new()
                };
                entry.arguments = args_str;

                if is_new && !entry.name.is_empty() {
                    StreamOutput::ToolCallStart {
                        id,
                        name: entry.name.clone(),
                    }
                } else if !delta.is_empty() {
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

    fn apply_end(&mut self, end: StreamEnd) {
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

        let mut remaining = self.tool_calls;
        for call_id in &self.tool_call_order {
            let Some(p) = remaining.remove(call_id) else {
                continue;
            };
            if p.name.is_empty() {
                continue;
            }
            let arguments: Value = serde_json::from_str(&p.arguments).unwrap_or(Value::Null);
            // Drop tool calls with unparseable arguments (truncated JSON)
            if arguments.is_null() && !p.arguments.is_empty() {
                continue;
            }
            tool_calls.push(ToolCall::new(p.id, p.name, arguments));
        }

        let content = if self.text.is_empty() {
            vec![]
        } else {
            vec![crate::contract::content::ContentBlock::text(self.text)]
        };

        StreamResult {
            content,
            tool_calls,
            usage: self.usage,
            stop_reason: self.stop_reason,
        }
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
    fn finish_with_no_events_returns_empty() {
        let c = StreamCollector::new();
        let result = c.finish();
        assert!(result.content.is_empty());
        assert!(result.tool_calls.is_empty());
        assert!(result.usage.is_none());
    }
}
