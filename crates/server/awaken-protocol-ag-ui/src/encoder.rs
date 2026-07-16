//! Project a committed step into AG-UI events. The step fold is shared
//! (`awaken_agent_contract::event`); this module owns the AG-UI *transcoder* —
//! the `Fact -> AgUiEvent` mapping. The encoder is per-stream: it holds the
//! thread/run ids and mints tool-result message ids, so it is stateful `&mut self`.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Message, Role};
use awaken_agent_contract::event::{
    Fact, HistorySink, ToolUseRef, Transcoder, fold_history, fold_messages,
};
use awaken_protocol_transport::{StepOutcome, blocks_text};
use serde_json::{Value, json};

use crate::types::AgUiEvent;

/// The AG-UI transcoder: neutral projection events to AG-UI events. `RunStarted`
/// and the terminal event become `RUN_STARTED` / `RUN_FINISHED` carrying the
/// stream's thread and run ids.
pub struct AgUiEncoder {
    thread_id: String,
    run_id: String,
    tool_result_seq: u64,
}

impl AgUiEncoder {
    pub fn new(thread_id: impl Into<String>, run_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            tool_result_seq: 0,
        }
    }
}

impl Transcoder for AgUiEncoder {
    type Output = AgUiEvent;

    fn transcode(&mut self, event: &Fact) -> Vec<AgUiEvent> {
        match event {
            Fact::RunStarted => vec![AgUiEvent::RunStarted {
                thread_id: self.thread_id.clone(),
                run_id: self.run_id.clone(),
            }],
            Fact::AssistantMessage { id, content } => {
                let text = blocks_text(content);
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![
                        AgUiEvent::TextMessageStart {
                            message_id: id.clone(),
                            role: "assistant".to_string(),
                        },
                        AgUiEvent::TextMessageContent {
                            message_id: id.clone(),
                            delta: text,
                        },
                        AgUiEvent::TextMessageEnd {
                            message_id: id.clone(),
                        },
                    ]
                }
            }
            Fact::ToolCall {
                id, name, input, ..
            } => vec![
                AgUiEvent::ToolCallStart {
                    tool_call_id: id.clone(),
                    tool_call_name: name.clone(),
                },
                AgUiEvent::ToolCallArgs {
                    tool_call_id: id.clone(),
                    delta: input.to_string(),
                },
                AgUiEvent::ToolCallEnd {
                    tool_call_id: id.clone(),
                },
            ],
            Fact::ToolResult { id, content, .. } => {
                let message_id = format!("{}-tr-{}", self.run_id, self.tool_result_seq);
                self.tool_result_seq += 1;
                vec![AgUiEvent::ToolCallResult {
                    message_id,
                    tool_call_id: id.clone(),
                    content: blocks_text(content),
                }]
            }
            Fact::Waiting { .. } | Fact::RunFinished { .. } => {
                vec![AgUiEvent::RunFinished {
                    thread_id: self.thread_id.clone(),
                    run_id: self.run_id.clone(),
                }]
            }
            // AG-UI runs end with either RUN_FINISHED or RUN_ERROR; a fault
            // maps to the latter, code-prefixed so clients can categorize.
            Fact::RunFailed { code, message } => vec![AgUiEvent::RunError {
                message: format!("{code}: {message}"),
            }],
            // An internal continuation-guard round is not an AG-UI wire frame; the
            // committed fold never emits it into this stream.
            Fact::Continuation { .. } => Vec::new(),
        }
    }
}

/// Project one committed step into an ordered AG-UI event stream (`RUN_STARTED` …
/// `RUN_FINISHED`).
pub fn encode_step(outcome: &StepOutcome, thread_id: &str, run_id: &str) -> Vec<AgUiEvent> {
    let pending = outcome
        .pending()
        .map(|p| (p.tool_use_id.as_str(), p.client_executed));
    let mut events = vec![Fact::RunStarted];
    events.extend(fold_messages(&outcome.new_messages, pending));
    // The terminal event owns the failed / parked / finished distinction — a fault
    // becomes `RunFailed`, which transcodes to `RUN_ERROR` instead of `RUN_FINISHED`.
    events.push(outcome.terminal_event());
    AgUiEncoder::new(thread_id, run_id).transcode_all(&events)
}

/// Project the *authoritative tail* of a committed step, for a turn whose
/// in-flight prefix (`RUN_STARTED`, streamed `TEXT_MESSAGE_*`, and
/// `TOOL_CALL_START`/`TOOL_CALL_ARGS`) was already emitted live (see
/// [`crate::live::AgUiLiveTranscoder`]). Drops `RUN_STARTED` and assistant text
/// (already streamed) and the tool `START`/`ARGS` (already streamed); keeps the
/// closing `TOOL_CALL_END`, any `TOOL_CALL_RESULT`, and the terminal
/// `RUN_FINISHED`. The live prefix plus this tail form one well-formed run.
pub fn encode_close(outcome: &StepOutcome, thread_id: &str, run_id: &str) -> Vec<AgUiEvent> {
    let pending = outcome
        .pending()
        .map(|p| (p.tool_use_id.as_str(), p.client_executed));
    let events = fold_messages(&outcome.new_messages, pending);
    let mut out = Vec::new();
    let mut tool_result_seq = 0u64;
    for event in &events {
        match event {
            // Text was streamed live as TEXT_MESSAGE_* deltas.
            Fact::AssistantMessage { .. } => {}
            // START + ARGS were streamed live; close the streamed tool call.
            Fact::ToolCall { id, .. } => out.push(AgUiEvent::ToolCallEnd {
                tool_call_id: id.clone(),
            }),
            Fact::ToolResult { id, content, .. } => {
                out.push(AgUiEvent::ToolCallResult {
                    message_id: format!("{run_id}-tr-{tool_result_seq}"),
                    tool_call_id: id.clone(),
                    content: blocks_text(content),
                });
                tool_result_seq += 1;
            }
            _ => {}
        }
    }
    // The terminal event (`RUN_FINISHED`, or `RUN_ERROR` on a fault) transcoded the
    // same way `encode_step` closes; the live prefix already carried `RUN_STARTED`.
    out.extend(AgUiEncoder::new(thread_id, run_id).transcode(&outcome.terminal_event()));
    out
}

/// Project committed thread history into the AG-UI message shape — the read-model
/// counterpart to the streaming transcoder. AG-UI is client-forward (the client
/// replays history in each `RunAgentInput`), so a client that lost its state
/// rehydrates from this server-persisted list. Each message is
/// `{ id, role, content, ... }`, matching the AG-UI SDK `Message` union: an
/// assistant turn carries any `toolCalls` (OpenAI-style, `arguments` a JSON
/// string); a tool result becomes its own `role:"tool"` message keyed by the
/// `toolCallId` it answers. Empty messages (no text, no tool call) are dropped, as
/// the streaming encoder drops them.
pub fn encode_history(messages: &[Message]) -> Vec<Value> {
    let mut sink = AgUiHistorySink::default();
    fold_history(messages, &mut sink);
    sink.encoded
}

/// The AG-UI read-model strategy: a user/system/assistant message becomes
/// `{ id, role, content, toolCalls? }`; a tool result becomes its own standalone
/// `role:"tool"` message keyed by the `toolCallId` it answers.
#[derive(Default)]
struct AgUiHistorySink {
    encoded: Vec<Value>,
}

impl HistorySink for AgUiHistorySink {
    fn user_or_system(&mut self, id: &str, role: Role, content: &[ContentBlock]) {
        let role = if role == Role::User { "user" } else { "system" };
        self.encoded
            .push(json!({ "id": id, "role": role, "content": blocks_text(content) }));
    }

    fn assistant(&mut self, id: &str, content: &[ContentBlock], tools: &[ToolUseRef<'_>]) {
        let tool_calls: Vec<Value> = tools
            .iter()
            .map(|tool| {
                json!({
                    "id": tool.id,
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "arguments": serde_json::to_string(tool.input).unwrap_or_default(),
                    },
                })
            })
            .collect();
        let mut msg = serde_json::Map::new();
        msg.insert("id".into(), json!(id));
        msg.insert("role".into(), json!("assistant"));
        msg.insert("content".into(), json!(blocks_text(content)));
        if !tool_calls.is_empty() {
            msg.insert("toolCalls".into(), Value::Array(tool_calls));
        }
        self.encoded.push(Value::Object(msg));
    }

    fn tool_result(
        &mut self,
        message_id: &str,
        sub: usize,
        tool_use_id: &str,
        content: &[ContentBlock],
    ) {
        // A neutral tool message may bundle several results; each becomes its own
        // AG-UI ToolMessage. The first reuses the message id; extras suffix it so
        // ids stay unique.
        let id = if sub == 0 {
            message_id.to_string()
        } else {
            format!("{message_id}-{sub}")
        };
        self.encoded.push(json!({
            "id": id,
            "role": "tool",
            "content": blocks_text(content),
            "toolCallId": tool_use_id,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::{Id, Message, Role};
    use awaken_protocol_transport::{Pending, StepFailure, Terminal};
    use serde_json::json;

    #[test]
    fn terminal_failure_surfaces_run_error() {
        let outcome = StepOutcome {
            terminal: Terminal::Failed(StepFailure {
                code: "inference_failed".into(),
                message: "upstream is down".into(),
            }),
            ..Default::default()
        };
        let step = encode_step(&outcome, "t1", "r1");
        assert!(
            step.iter().any(|e| matches!(e, AgUiEvent::RunError { .. })),
            "RUN_ERROR: {step:?}"
        );
        assert!(
            !step
                .iter()
                .any(|e| matches!(e, AgUiEvent::RunFinished { .. })),
            "a failed run does not also RUN_FINISHED: {step:?}",
        );
        let close = encode_close(&outcome, "t1", "r1");
        assert!(
            close
                .iter()
                .any(|e| matches!(e, AgUiEvent::RunError { .. })),
            "close RUN_ERROR: {close:?}"
        );
    }

    #[test]
    fn plain_turn_brackets_with_run_events() {
        let outcome = StepOutcome {
            new_messages: vec![Message::text(Id("a1".into()), Role::Assistant, "hello")],
            terminal: Terminal::Finished,
        };
        let events = encode_step(&outcome, "t1", "r1");
        assert_eq!(
            events.first(),
            Some(&AgUiEvent::RunStarted {
                thread_id: "t1".into(),
                run_id: "r1".into()
            })
        );
        assert_eq!(
            events.last(),
            Some(&AgUiEvent::RunFinished {
                thread_id: "t1".into(),
                run_id: "r1".into()
            })
        );
        assert!(
            events.iter().any(
                |e| matches!(e, AgUiEvent::TextMessageContent { delta, .. } if delta == "hello")
            )
        );
    }

    #[test]
    fn close_emits_end_and_finish_without_start_or_args() {
        let outcome = StepOutcome {
            new_messages: vec![
                Message::text(Id("a1".into()), Role::Assistant, "reading"),
                Message {
                    id: Id("a2".into()),
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "c1".into(),
                        name: "read".into(),
                        input: json!({"path": "x"}),
                    }],
                },
            ],
            terminal: Terminal::Waiting {
                pending: Some(Pending {
                    tool_use_id: "c1".into(),
                    name: "read".into(),
                    input: json!({"path": "x"}),
                    client_executed: true,
                }),
            },
        };
        let events = encode_close(&outcome, "t1", "r1");
        // No start/text/args — those were streamed live.
        assert!(events.iter().all(|e| !matches!(
            e,
            AgUiEvent::RunStarted { .. }
                | AgUiEvent::TextMessageStart { .. }
                | AgUiEvent::TextMessageContent { .. }
                | AgUiEvent::ToolCallStart { .. }
                | AgUiEvent::ToolCallArgs { .. }
        )));
        assert!(events.contains(&AgUiEvent::ToolCallEnd {
            tool_call_id: "c1".into(),
        }));
        assert_eq!(
            events.last(),
            Some(&AgUiEvent::RunFinished {
                thread_id: "t1".into(),
                run_id: "r1".into(),
            })
        );
    }

    #[test]
    fn tool_call_emits_start_args_end() {
        let outcome = StepOutcome {
            new_messages: vec![Message {
                id: Id("a1".into()),
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "submit".into(),
                    input: json!({"q": 1}),
                }],
            }],
            terminal: Terminal::Waiting {
                pending: Some(Pending {
                    tool_use_id: "c1".into(),
                    name: "submit".into(),
                    input: json!({"q": 1}),
                    client_executed: true,
                }),
            },
        };
        let events = encode_step(&outcome, "t1", "r1");
        assert!(events.iter().any(|e| matches!(e, AgUiEvent::ToolCallStart { tool_call_id, tool_call_name } if tool_call_id == "c1" && tool_call_name == "submit")));
        assert!(events.iter().any(
            |e| matches!(e, AgUiEvent::ToolCallArgs { tool_call_id, .. } if tool_call_id == "c1")
        ));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgUiEvent::RunFinished { .. }))
        );
    }

    #[test]
    fn run_failed_transcodes_to_a_run_error() {
        let mut enc = AgUiEncoder::new("t1", "r1");
        let events = enc.transcode(&Fact::RunFailed {
            code: "overloaded".into(),
            message: "try later".into(),
        });
        assert!(matches!(
            events.as_slice(),
            [AgUiEvent::RunError { message }]
                if message.contains("overloaded") && message.contains("try later")
        ));
    }

    #[test]
    fn waiting_transcodes_to_a_run_finished_not_an_interrupt() {
        // Documents the current shape: a parked built-in tool surfaces as a plain
        // RUN_FINISHED (no dedicated RUN_INTERRUPTED event exists in this adapter).
        let mut enc = AgUiEncoder::new("t1", "r1");
        let events = enc.transcode(&Fact::Waiting {
            pending_tool_use_id: Some("c1".into()),
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgUiEvent::RunFinished { .. }))
        );
    }

    #[test]
    fn successive_tool_results_get_distinct_message_ids() {
        let mut enc = AgUiEncoder::new("t1", "r1");
        let first = enc.transcode(&Fact::ToolResult {
            id: "c1".into(),
            content: vec![ContentBlock::text("one")],
            is_error: false,
        });
        let second = enc.transcode(&Fact::ToolResult {
            id: "c2".into(),
            content: vec![ContentBlock::text("two")],
            is_error: false,
        });
        let id = |evs: &[AgUiEvent]| match &evs[0] {
            AgUiEvent::ToolCallResult { message_id, .. } => message_id.clone(),
            other => panic!("expected a tool-call result, got {other:?}"),
        };
        let a = id(&first);
        let b = id(&second);
        assert_eq!(a, "r1-tr-0");
        assert_eq!(b, "r1-tr-1");
        assert_ne!(a, b, "each tool result must carry a distinct message id");
    }

    #[test]
    fn an_empty_assistant_message_transcodes_to_nothing() {
        let mut enc = AgUiEncoder::new("t1", "r1");
        let events = enc.transcode(&Fact::AssistantMessage {
            id: "a1".into(),
            content: vec![],
        });
        assert!(events.is_empty());
    }

    #[test]
    fn an_assistant_message_transcodes_to_a_bracketed_text_message() {
        let mut enc = AgUiEncoder::new("t1", "r1");
        let events = enc.transcode(&Fact::AssistantMessage {
            id: "a1".into(),
            content: vec![ContentBlock::text("hello")],
        });
        assert!(matches!(
            events.first(),
            Some(AgUiEvent::TextMessageStart { .. })
        ));
        assert!(matches!(
            events.last(),
            Some(AgUiEvent::TextMessageEnd { .. })
        ));
        assert!(
            events.iter().any(
                |e| matches!(e, AgUiEvent::TextMessageContent { delta, .. } if delta == "hello")
            )
        );
    }

    #[test]
    fn a_tool_call_transcodes_to_start_args_end() {
        use awaken_agent_contract::event::ToolDisposition;
        let mut enc = AgUiEncoder::new("t1", "r1");
        let events = enc.transcode(&Fact::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            input: json!({ "path": "x" }),
            disposition: ToolDisposition::PendingBuiltin,
        });
        assert!(matches!(events[0], AgUiEvent::ToolCallStart { .. }));
        assert!(matches!(events[1], AgUiEvent::ToolCallArgs { .. }));
        assert!(matches!(events[2], AgUiEvent::ToolCallEnd { .. }));
    }

    #[test]
    fn history_projects_user_and_assistant_to_ag_ui_messages() {
        let messages = vec![
            Message::text(Id("u1".into()), Role::User, "hi there"),
            Message::text(Id("a1".into()), Role::Assistant, "hello back"),
        ];
        let encoded = encode_history(&messages);
        assert_eq!(
            encoded,
            vec![
                json!({ "id": "u1", "role": "user", "content": "hi there" }),
                json!({ "id": "a1", "role": "assistant", "content": "hello back" }),
            ]
        );
    }

    #[test]
    fn history_projects_a_system_message() {
        let messages = vec![Message::text(Id("s1".into()), Role::System, "be terse")];
        let encoded = encode_history(&messages);
        assert_eq!(
            encoded,
            vec![json!({ "id": "s1", "role": "system", "content": "be terse" })]
        );
    }

    #[test]
    fn history_projects_assistant_tool_calls_as_openai_function_calls() {
        let messages = vec![Message {
            id: Id("a1".into()),
            role: Role::Assistant,
            content: vec![
                ContentBlock::text("calling"),
                ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "read".into(),
                    input: json!({ "path": "x" }),
                },
            ],
        }];
        let encoded = encode_history(&messages);
        assert_eq!(
            encoded,
            vec![json!({
                "id": "a1",
                "role": "assistant",
                "content": "calling",
                "toolCalls": [{
                    "id": "c1",
                    "type": "function",
                    "function": { "name": "read", "arguments": "{\"path\":\"x\"}" },
                }],
            })]
        );
    }

    #[test]
    fn history_projects_a_tool_result_as_a_keyed_tool_message() {
        let messages = vec![Message {
            id: Id("t1".into()),
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "c1".into(),
                content: vec![ContentBlock::text("42")],
            }],
        }];
        let encoded = encode_history(&messages);
        assert_eq!(
            encoded,
            vec![json!({ "id": "t1", "role": "tool", "content": "42", "toolCallId": "c1" })]
        );
    }

    #[test]
    fn history_expands_bundled_tool_results_with_distinct_ids() {
        let messages = vec![Message {
            id: Id("t1".into()),
            role: Role::Tool,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "c1".into(),
                    content: vec![ContentBlock::text("a")],
                },
                ContentBlock::ToolResult {
                    tool_use_id: "c2".into(),
                    content: vec![ContentBlock::text("b")],
                },
            ],
        }];
        let encoded = encode_history(&messages);
        let ids: Vec<&str> = encoded.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["t1", "t1-1"]);
    }

    #[test]
    fn history_drops_an_empty_message() {
        let messages = vec![
            Message::text(Id("u1".into()), Role::User, ""),
            Message {
                id: Id("a1".into()),
                role: Role::Assistant,
                content: vec![],
            },
        ];
        assert!(encode_history(&messages).is_empty());
    }

    // Post the projection change: an assistant message whose only block is an
    // empty-string Text is dropped by `fold_messages`, so the AG-UI stream
    // brackets the run with RUN_STARTED/RUN_FINISHED and emits no spurious
    // TEXT_MESSAGE_* frames.
    #[test]
    fn an_empty_text_only_assistant_turn_emits_no_text_message() {
        let outcome = StepOutcome {
            new_messages: vec![Message::new(
                Id("a1".into()),
                Role::Assistant,
                vec![ContentBlock::text("")],
            )],
            terminal: Terminal::Finished,
        };
        let events = encode_step(&outcome, "t1", "r1");
        assert!(
            events.iter().all(|e| !matches!(
                e,
                AgUiEvent::TextMessageStart { .. }
                    | AgUiEvent::TextMessageContent { .. }
                    | AgUiEvent::TextMessageEnd { .. }
            )),
            "an all-empty-text assistant turn must emit no text message: {events:?}"
        );
        assert!(matches!(events.first(), Some(AgUiEvent::RunStarted { .. })));
        assert!(matches!(events.last(), Some(AgUiEvent::RunFinished { .. })));
    }

    // encode_close tail for a streamed turn that ran a server tool to completion:
    // the committed step carries the assistant tool-use *and* its tool-role result.
    // The live prefix already streamed START+ARGS, so the tail closes the call with
    // TOOL_CALL_END and emits the TOOL_CALL_RESULT (minted `<run>-tr-0`) before
    // RUN_FINISHED — the inline-seq path distinct from the streaming transcoder.
    #[test]
    fn close_emits_tool_end_then_result_for_a_server_executed_tool() {
        let outcome = StepOutcome {
            new_messages: vec![
                Message {
                    id: Id("a1".into()),
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "c1".into(),
                        name: "read".into(),
                        input: json!({"path": "x"}),
                    }],
                },
                Message {
                    id: Id("t1".into()),
                    role: Role::Tool,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "c1".into(),
                        content: vec![ContentBlock::text("42")],
                    }],
                },
            ],
            terminal: Terminal::Finished,
        };
        let events = encode_close(&outcome, "t1", "r1");
        // START/ARGS were streamed live; the tail must not re-open them.
        assert!(events.iter().all(|e| !matches!(
            e,
            AgUiEvent::ToolCallStart { .. } | AgUiEvent::ToolCallArgs { .. }
        )));
        let end = events
            .iter()
            .position(
                |e| matches!(e, AgUiEvent::ToolCallEnd { tool_call_id } if tool_call_id == "c1"),
            )
            .expect("TOOL_CALL_END for the streamed call");
        let result = events
            .iter()
            .position(|e| {
                matches!(
                    e,
                    AgUiEvent::ToolCallResult { message_id, tool_call_id, content }
                        if message_id == "r1-tr-0" && tool_call_id == "c1" && content == "42"
                )
            })
            .expect("TOOL_CALL_RESULT keyed by the answered call, minted r1-tr-0");
        assert!(end < result, "END precedes RESULT: {events:?}");
        assert!(matches!(events.last(), Some(AgUiEvent::RunFinished { .. })));
    }
}
