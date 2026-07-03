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
}
