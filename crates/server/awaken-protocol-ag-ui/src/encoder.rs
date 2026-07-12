//! Project a committed step into AG-UI events. The step fold is shared
//! (`awaken_agent_contract::project`); this module owns the AG-UI *transcoder* —
//! the `AgentEvent -> AgUiEvent` mapping. The encoder is per-stream: it holds the
//! thread/run ids and mints tool-result message ids, so it is stateful `&mut self`.

use awaken_agent_contract::project::{AgentEvent, Transcoder, project_messages, terminal_waiting};
use awaken_protocol_transport::{StepOutcome, blocks_text};

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

    fn transcode(&mut self, event: &AgentEvent) -> Vec<AgUiEvent> {
        match event {
            AgentEvent::RunStarted => vec![AgUiEvent::RunStarted {
                thread_id: self.thread_id.clone(),
                run_id: self.run_id.clone(),
            }],
            AgentEvent::AssistantMessage { id, content } => {
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
            AgentEvent::ToolCall {
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
            AgentEvent::ToolResult { id, content, .. } => {
                let message_id = format!("{}-tr-{}", self.run_id, self.tool_result_seq);
                self.tool_result_seq += 1;
                vec![AgUiEvent::ToolCallResult {
                    message_id,
                    tool_call_id: id.clone(),
                    content: blocks_text(content),
                }]
            }
            AgentEvent::Waiting { .. } | AgentEvent::RunFinished { .. } => {
                vec![AgUiEvent::RunFinished {
                    thread_id: self.thread_id.clone(),
                    run_id: self.run_id.clone(),
                }]
            }
            // AG-UI runs end with either RUN_FINISHED or RUN_ERROR; a fault
            // maps to the latter, code-prefixed so clients can categorize.
            AgentEvent::RunFailed { code, message } => vec![AgUiEvent::RunError {
                message: format!("{code}: {message}"),
            }],
        }
    }
}

/// Project one committed step into an ordered AG-UI event stream (`RUN_STARTED` …
/// `RUN_FINISHED`).
pub fn encode_step(outcome: &StepOutcome, thread_id: &str, run_id: &str) -> Vec<AgUiEvent> {
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
        .pending
        .as_ref()
        .map(|p| (p.tool_use_id.as_str(), p.client_executed));
    let events = project_messages(&outcome.new_messages, pending);
    let mut out = Vec::new();
    let mut tool_result_seq = 0u64;
    for event in &events {
        match event {
            // Text was streamed live as TEXT_MESSAGE_* deltas.
            AgentEvent::AssistantMessage { .. } => {}
            // START + ARGS were streamed live; close the streamed tool call.
            AgentEvent::ToolCall { id, .. } => out.push(AgUiEvent::ToolCallEnd {
                tool_call_id: id.clone(),
            }),
            AgentEvent::ToolResult { id, content, .. } => {
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
    out.push(AgUiEvent::RunFinished {
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::{Id, Message, Role};
    use awaken_protocol_transport::Pending;
    use serde_json::json;

    #[test]
    fn plain_turn_brackets_with_run_events() {
        let outcome = StepOutcome {
            new_messages: vec![Message::text(Id("a1".into()), Role::Assistant, "hello")],
            waiting: false,
            exhausted: false,
            pending: None,
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
            waiting: true,
            exhausted: false,
            pending: Some(Pending {
                tool_use_id: "c1".into(),
                name: "read".into(),
                input: json!({"path": "x"}),
                client_executed: true,
            }),
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
            waiting: true,
            exhausted: false,
            pending: Some(Pending {
                tool_use_id: "c1".into(),
                name: "submit".into(),
                input: json!({"q": 1}),
                client_executed: true,
            }),
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
        let events = enc.transcode(&AgentEvent::RunFailed {
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
        let events = enc.transcode(&AgentEvent::Waiting {
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
        let first = enc.transcode(&AgentEvent::ToolResult {
            id: "c1".into(),
            content: vec![ContentBlock::text("one")],
            is_error: false,
        });
        let second = enc.transcode(&AgentEvent::ToolResult {
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
        let events = enc.transcode(&AgentEvent::AssistantMessage {
            id: "a1".into(),
            content: vec![],
        });
        assert!(events.is_empty());
    }

    #[test]
    fn an_assistant_message_transcodes_to_a_bracketed_text_message() {
        let mut enc = AgUiEncoder::new("t1", "r1");
        let events = enc.transcode(&AgentEvent::AssistantMessage {
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
        use awaken_agent_contract::project::ToolDisposition;
        let mut enc = AgUiEncoder::new("t1", "r1");
        let events = enc.transcode(&AgentEvent::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            input: json!({ "path": "x" }),
            disposition: ToolDisposition::PendingBuiltin,
        });
        assert!(matches!(events[0], AgUiEvent::ToolCallStart { .. }));
        assert!(matches!(events[1], AgUiEvent::ToolCallArgs { .. }));
        assert!(matches!(events[2], AgUiEvent::ToolCallEnd { .. }));
    }
}
