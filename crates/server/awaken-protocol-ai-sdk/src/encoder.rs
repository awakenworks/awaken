//! Project a committed step (and thread history) into AI SDK UI Message Stream
//! parts. The step fold is shared (`awaken_agent_contract::project`); this module
//! owns the AI SDK *transcoder* — the `AgentEvent -> UIStreamEvent` mapping — and
//! the history read-model fold.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Message, Role};
use awaken_agent_contract::project::{
    AgentEvent, ToolDisposition, Transcoder, project_messages, terminal_waiting,
};
use awaken_protocol_transport::{StepOutcome, blocks_text};
use serde_json::Value;

use crate::types::{UIStreamEvent, history_message, text_parts};

/// The AI SDK v6 transcoder: neutral projection events to UI Message Stream parts.
/// A pending tool (client- or built-in) is *not* provider-executed, so the client
/// renders its interaction; a step boundary becomes `start`/`finish` frames.
#[derive(Default)]
pub struct AiSdkEncoder;

impl Transcoder for AiSdkEncoder {
    type Output = UIStreamEvent;

    fn transcode(&mut self, event: &AgentEvent) -> Vec<UIStreamEvent> {
        match event {
            AgentEvent::RunStarted => vec![UIStreamEvent::Start, UIStreamEvent::StartStep],
            AgentEvent::AssistantMessage { id, content } => {
                let text = blocks_text(content);
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![
                        UIStreamEvent::TextStart { id: id.clone() },
                        UIStreamEvent::TextDelta {
                            id: id.clone(),
                            delta: text,
                        },
                        UIStreamEvent::TextEnd { id: id.clone() },
                    ]
                }
            }
            AgentEvent::ToolCall {
                id,
                name,
                input,
                disposition,
            } => vec![UIStreamEvent::ToolInputAvailable {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                input: input.clone(),
                provider_executed: matches!(disposition, ToolDisposition::Executed),
            }],
            AgentEvent::ToolResult {
                id,
                content,
                is_error,
            } => {
                if *is_error {
                    vec![UIStreamEvent::ToolOutputError {
                        tool_call_id: id.clone(),
                        error_text: blocks_text(content),
                    }]
                } else {
                    vec![UIStreamEvent::ToolOutputAvailable {
                        tool_call_id: id.clone(),
                        output: parse_output(content),
                    }]
                }
            }
            AgentEvent::Waiting { .. } => {
                vec![
                    UIStreamEvent::FinishStep,
                    UIStreamEvent::finish("tool-calls"),
                ]
            }
            AgentEvent::RunFinished { .. } => {
                vec![UIStreamEvent::FinishStep, UIStreamEvent::finish("stop")]
            }
            AgentEvent::RunFailed { code, message } => vec![
                UIStreamEvent::error(format!("{code}: {message}")),
                UIStreamEvent::FinishStep,
                UIStreamEvent::finish("error"),
            ],
        }
    }
}

/// Project one committed step into an ordered UI Message Stream. Each response is a
/// self-contained stream: `start` … `finish`.
pub fn encode_step(outcome: &StepOutcome) -> Vec<UIStreamEvent> {
    let pending = outcome
        .pending
        .as_ref()
        .map(|p| (p.tool_use_id.as_str(), p.client_executed));
    let mut events = vec![AgentEvent::RunStarted];
    events.extend(project_messages(&outcome.new_messages, pending));
    events.push(if outcome.waiting {
        terminal_waiting(outcome.pending.as_ref().map(|p| p.tool_use_id.as_str()))
    } else {
        AgentEvent::RunFinished {
            exhausted: outcome.exhausted,
        }
    });
    AiSdkEncoder.transcode_all(&events)
}

/// Project the *authoritative tail* of a committed step, for a turn whose
/// in-flight prefix — `start`/`start-step`, streamed `text-*`, and
/// `tool-input-start`/`tool-input-delta` — was already emitted live (see
/// [`crate::live::LiveTranscoder`]). Drops `RunStarted` (already `start`ed) and
/// assistant text (already streamed as deltas); keeps the parsed authoritative
/// `tool-input-available`, any tool output, and the `finish` frames. The live
/// prefix plus this tail form one well-formed UI Message Stream.
pub fn encode_close(outcome: &StepOutcome) -> Vec<UIStreamEvent> {
    let pending = outcome
        .pending
        .as_ref()
        .map(|p| (p.tool_use_id.as_str(), p.client_executed));
    let mut events = project_messages(&outcome.new_messages, pending);
    events.push(if outcome.waiting {
        terminal_waiting(outcome.pending.as_ref().map(|p| p.tool_use_id.as_str()))
    } else {
        AgentEvent::RunFinished {
            exhausted: outcome.exhausted,
        }
    });
    // The live channel already carried `start`/`start-step` and every text delta;
    // emitting them again would double the stream. Keep only tool + finish frames.
    events.retain(|e| !matches!(e, AgentEvent::AssistantMessage { .. }));
    AiSdkEncoder.transcode_all(&events)
}

/// Parse a tool result's text as JSON, falling back to a string.
fn parse_output(content: &[ContentBlock]) -> Value {
    let text = blocks_text(content);
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

/// Fold committed thread messages into AI SDK `UIMessage`s for the history
/// endpoint. Assistant tool calls merge with their later tool result into a single
/// `output-available` part (`providerExecuted: true`). This is a read-model fold,
/// distinct from the streaming projection above.
pub fn encode_history(messages: &[Message]) -> Vec<Value> {
    let mut encoded: Vec<Value> = Vec::new();
    // tool_call_id -> (message index in `encoded`, part index in that message)
    let mut pending_parts: std::collections::HashMap<String, (usize, usize)> =
        std::collections::HashMap::new();

    for message in messages {
        match message.role {
            Role::User | Role::System => {
                let parts = text_parts(&message.content);
                if parts.is_empty() {
                    continue;
                }
                let role = if message.role == Role::User {
                    "user"
                } else {
                    "system"
                };
                encoded.push(history_message(&message.id.0, role, parts));
            }
            Role::Assistant => {
                let mut parts = text_parts(&message.content);
                let message_index = encoded.len();
                for block in &message.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        let part_index = parts.len();
                        parts.push(serde_json::json!({
                            "type": format!("tool-{name}"),
                            "toolName": name,
                            "toolCallId": id,
                            "state": "input-available",
                            "input": input,
                            "providerExecuted": true,
                        }));
                        pending_parts.insert(id.clone(), (message_index, part_index));
                    }
                }
                if parts.is_empty() {
                    continue;
                }
                encoded.push(history_message(&message.id.0, "assistant", parts));
            }
            Role::Tool => {
                for block in &message.content {
                    let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                    } = block
                    else {
                        continue;
                    };
                    let Some((mi, pi)) = pending_parts.remove(tool_use_id) else {
                        continue;
                    };
                    if let Some(part) = encoded
                        .get_mut(mi)
                        .and_then(|m| m.get_mut("parts"))
                        .and_then(Value::as_array_mut)
                        .and_then(|parts| parts.get_mut(pi))
                        .and_then(Value::as_object_mut)
                    {
                        part.insert("state".into(), Value::String("output-available".into()));
                        part.insert("output".into(), parse_output(content));
                    }
                }
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::Id;
    use awaken_protocol_transport::Pending;
    use serde_json::json;

    fn assistant_tool(id: &str, call: &str, name: &str, args: Value) -> Message {
        Message {
            id: Id(id.to_string()),
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: call.to_string(),
                name: name.to_string(),
                input: args,
            }],
        }
    }

    #[test]
    fn client_pending_tool_is_not_provider_executed() {
        let outcome = StepOutcome {
            new_messages: vec![assistant_tool("a1", "c1", "submit_answer", json!({}))],
            waiting: true,
            exhausted: false,
            pending: Some(Pending {
                tool_use_id: "c1".into(),
                name: "submit_answer".into(),
                input: json!({}),
                client_executed: true,
            }),
        };
        let events = encode_step(&outcome);
        let tool = events
            .iter()
            .find_map(|e| match e {
                UIStreamEvent::ToolInputAvailable {
                    provider_executed, ..
                } => Some(*provider_executed),
                _ => None,
            })
            .unwrap();
        assert!(!tool, "pending client tool must not be provider-executed");
        assert!(events.contains(&UIStreamEvent::finish("tool-calls")));
    }

    #[test]
    fn plain_assistant_turn_finishes_stop() {
        let outcome = StepOutcome {
            new_messages: vec![Message::text(Id("a1".into()), Role::Assistant, "hello")],
            waiting: false,
            exhausted: false,
            pending: None,
        };
        let events = encode_step(&outcome);
        assert!(events.contains(&UIStreamEvent::finish("stop")));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, UIStreamEvent::TextDelta { delta, .. } if delta == "hello"))
        );
    }

    #[test]
    fn close_emits_authoritative_tail_without_start_or_text() {
        let outcome = StepOutcome {
            new_messages: vec![
                Message::text(Id("a1".into()), Role::Assistant, "let me read"),
                assistant_tool("a2", "c1", "read", json!({"path": "x"})),
            ],
            waiting: true,
            exhausted: false,
            pending: Some(Pending {
                tool_use_id: "c1".into(),
                name: "read".into(),
                input: json!({"path": "x"}),
                client_executed: true,
            }),
        };
        let events = encode_close(&outcome);
        // No start/step/text — those were streamed live.
        assert!(events.iter().all(|e| !matches!(
            e,
            UIStreamEvent::Start
                | UIStreamEvent::StartStep
                | UIStreamEvent::TextStart { .. }
                | UIStreamEvent::TextDelta { .. }
                | UIStreamEvent::TextEnd { .. }
        )));
        // The authoritative parsed input arrives, plus the finish frames.
        assert!(events.iter().any(|e| matches!(
            e,
            UIStreamEvent::ToolInputAvailable { tool_call_id, input, .. }
                if tool_call_id == "c1" && input == &json!({"path": "x"})
        )));
        assert!(events.contains(&UIStreamEvent::finish("tool-calls")));
    }

    #[test]
    fn history_merges_tool_call_with_result() {
        let messages = vec![
            Message::text(Id("u1".into()), Role::User, "go"),
            assistant_tool("a1", "c1", "read", json!({"path": "x"})),
            Message {
                id: Id("t1".into()),
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "c1".into(),
                    content: vec![ContentBlock::Text {
                        text: "\"data\"".into(),
                    }],
                }],
            },
        ];
        let encoded = encode_history(&messages);
        assert_eq!(encoded.len(), 2);
        let part = &encoded[1]["parts"][0];
        assert_eq!(part["type"], "tool-read");
        assert_eq!(part["state"], "output-available");
        assert_eq!(part["output"], json!("data"));
    }

    #[test]
    fn a_tool_result_with_no_matching_call_is_dropped_from_history() {
        let messages = vec![
            Message::text(Id("u1".into()), Role::User, "go"),
            Message {
                id: Id("t1".into()),
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "orphan".into(),
                    content: vec![ContentBlock::Text {
                        text: "\"data\"".into(),
                    }],
                }],
            },
        ];
        let encoded = encode_history(&messages);
        // Only the user message survives; an orphan result has no call to merge into.
        assert_eq!(encoded.len(), 1);
        assert_eq!(encoded[0]["role"], "user");
    }

    #[test]
    fn tool_result_error_maps_to_tool_output_error() {
        let events = AiSdkEncoder.transcode(&AgentEvent::ToolResult {
            id: "c1".into(),
            content: vec![ContentBlock::text("it broke")],
            is_error: true,
        });
        assert!(matches!(
            events.as_slice(),
            [UIStreamEvent::ToolOutputError { tool_call_id, error_text }]
                if tool_call_id == "c1" && error_text == "it broke"
        ));
    }

    #[test]
    fn tool_result_success_maps_to_tool_output_available() {
        let events = AiSdkEncoder.transcode(&AgentEvent::ToolResult {
            id: "c1".into(),
            content: vec![ContentBlock::text("ok")],
            is_error: false,
        });
        assert!(matches!(
            events.as_slice(),
            [UIStreamEvent::ToolOutputAvailable { tool_call_id, .. }] if tool_call_id == "c1"
        ));
    }

    #[test]
    fn run_failed_maps_to_error_then_finish_error() {
        let events = AiSdkEncoder.transcode(&AgentEvent::RunFailed {
            code: "overloaded".into(),
            message: "try later".into(),
        });
        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[0],
            UIStreamEvent::Error { error_text }
                if error_text.contains("overloaded") && error_text.contains("try later")
        ));
        assert!(matches!(events[1], UIStreamEvent::FinishStep));
        assert!(matches!(&events[2], UIStreamEvent::Finish { .. }));
    }

    #[test]
    fn run_started_transcodes_to_start_and_start_step() {
        let events = AiSdkEncoder.transcode(&AgentEvent::RunStarted);
        assert!(matches!(
            events.as_slice(),
            [UIStreamEvent::Start, UIStreamEvent::StartStep]
        ));
    }

    #[test]
    fn waiting_transcodes_to_finish_step_then_tool_calls_finish() {
        let events = AiSdkEncoder.transcode(&AgentEvent::Waiting {
            pending_tool_use_id: Some("c1".into()),
        });
        assert!(matches!(events[0], UIStreamEvent::FinishStep));
        assert!(matches!(
            &events[1],
            UIStreamEvent::Finish { finish_reason: Some(r), .. } if r == "tool-calls"
        ));
    }

    #[test]
    fn a_tool_call_transcodes_to_tool_input_available() {
        use awaken_agent_contract::project::ToolDisposition;
        let events = AiSdkEncoder.transcode(&AgentEvent::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            input: json!({ "path": "x" }),
            disposition: ToolDisposition::PendingBuiltin,
        });
        assert!(matches!(
            &events[0],
            UIStreamEvent::ToolInputAvailable { tool_call_id, provider_executed: false, .. }
                if tool_call_id == "c1"
        ));
    }
}
