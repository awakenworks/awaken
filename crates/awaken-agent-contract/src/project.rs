//! Neutral projection events and the protocol `Transcoder` seam.
//!
//! This is the single shape every public protocol adapter projects from. A
//! committed step (the messages committed during a turn or resume, plus the
//! terminal phase) is folded into a sequence of neutral [`AgentEvent`]s by
//! [`project_messages`] / [`project_step`]; each protocol then implements one
//! [`Transcoder`] that maps those events to its own wire vocabulary. The fold is
//! shared; only the transcoder differs per protocol (static Strategy).

use serde_json::Value;

use crate::agent::content::ContentBlock;
use crate::agent::message::{Message, Role};
use crate::agent::run::{EndCause, Phase};

/// How a tool call was dispatched, as seen at projection time. This is the only
/// place the "who runs the tool" distinction is carried; each transcoder maps it
/// to its own tool-part shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolDisposition {
    /// The tool ran server-side; a [`AgentEvent::ToolResult`] follows.
    Executed,
    /// A client-executed tool the run parked on; the client runs it and returns
    /// the result.
    PendingClient,
    /// A built-in tool the run parked on, awaiting a permission decision.
    PendingBuiltin,
}

/// One neutral projection event. Carries no protocol vocabulary.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// The step began (a run/turn boundary).
    RunStarted,
    /// An assistant message's text content (text blocks only).
    AssistantMessage {
        id: String,
        content: Vec<ContentBlock>,
    },
    /// The assistant called a tool.
    ToolCall {
        id: String,
        name: String,
        input: Value,
        disposition: ToolDisposition,
    },
    /// A tool produced a result.
    ToolResult {
        id: String,
        content: Vec<ContentBlock>,
        is_error: bool,
    },
    /// The step parked awaiting a decision on the named pending tool.
    Waiting { pending_tool_use_id: Option<String> },
    /// The step reached a natural or budget-exhausted terminus.
    RunFinished { exhausted: bool },
}

/// Transcode neutral projection events into a protocol's wire events. One impl per
/// protocol — the only per-protocol part of the projection pipeline. `&mut self`
/// so a transcoder may carry per-stream state (e.g. a terminal guard, id minting).
pub trait Transcoder {
    /// The protocol's wire event type.
    type Output;

    /// Transcode one neutral event into zero or more wire events.
    fn transcode(&mut self, event: &AgentEvent) -> Vec<Self::Output>;

    /// Transcode a whole sequence in order.
    fn transcode_all(&mut self, events: &[AgentEvent]) -> Vec<Self::Output> {
        events
            .iter()
            .flat_map(|event| self.transcode(event))
            .collect()
    }
}

/// Fold a committed step's messages into per-message neutral events (no
/// `RunStarted`, no terminal). `pending` is `(tool_use_id, client_executed)` of
/// the tool the run parked on, when it parked — it classifies that tool's call.
pub fn project_messages(
    new_messages: &[Message],
    pending: Option<(&str, bool)>,
) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    for message in new_messages {
        match message.role {
            Role::Assistant => {
                let text: Vec<ContentBlock> = message
                    .content
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::Text { .. }))
                    .cloned()
                    .collect();
                if !text.is_empty() {
                    out.push(AgentEvent::AssistantMessage {
                        id: message.id.0.clone(),
                        content: text,
                    });
                }
                for block in &message.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        let disposition = match pending {
                            Some((pid, client)) if pid == id.as_str() => {
                                if client {
                                    ToolDisposition::PendingClient
                                } else {
                                    ToolDisposition::PendingBuiltin
                                }
                            }
                            _ => ToolDisposition::Executed,
                        };
                        out.push(AgentEvent::ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                            disposition,
                        });
                    }
                }
            }
            Role::Tool => {
                for block in &message.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                    } = block
                    {
                        out.push(AgentEvent::ToolResult {
                            id: tool_use_id.clone(),
                            content: content.clone(),
                            is_error: false,
                        });
                    }
                }
            }
            Role::User | Role::System => {}
        }
    }
    out
}

/// Fold a full committed step (with boundaries): `RunStarted`, the message
/// events, then a terminal event derived from `phase`.
pub fn project_step(
    new_messages: &[Message],
    phase: &Phase,
    pending: Option<(&str, bool)>,
) -> Vec<AgentEvent> {
    let mut out = vec![AgentEvent::RunStarted];
    out.extend(project_messages(new_messages, pending));
    out.push(terminal(phase, pending));
    out
}

/// The `Waiting` terminal event naming the pending tool. For callers that carry a
/// protocol stop reason rather than a [`Phase`].
pub fn terminal_waiting(pending_tool_use_id: Option<&str>) -> AgentEvent {
    AgentEvent::Waiting {
        pending_tool_use_id: pending_tool_use_id.map(str::to_string),
    }
}

/// The terminal projection event for a phase.
pub fn terminal(phase: &Phase, pending: Option<(&str, bool)>) -> AgentEvent {
    match phase {
        Phase::Waiting => AgentEvent::Waiting {
            pending_tool_use_id: pending.map(|p| p.0.to_string()),
        },
        Phase::Ended(EndCause::MaxSteps) => AgentEvent::RunFinished { exhausted: true },
        Phase::Ended(_) => AgentEvent::RunFinished { exhausted: false },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::message::Id;

    #[test]
    fn classifies_pending_client_tool() {
        let msg = Message {
            id: Id("a1".into()),
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "submit".into(),
                input: serde_json::json!({}),
            }],
        };
        let events = project_messages(&[msg], Some(("c1", true)));
        assert_eq!(
            events,
            vec![AgentEvent::ToolCall {
                id: "c1".into(),
                name: "submit".into(),
                input: serde_json::json!({}),
                disposition: ToolDisposition::PendingClient,
            }]
        );
    }

    #[test]
    fn step_wraps_with_start_and_terminal() {
        let msg = Message::text(Id("a1".into()), Role::Assistant, "hi");
        let events = project_step(&[msg], &Phase::Ended(EndCause::NaturalEnd), None);
        assert_eq!(events.first(), Some(&AgentEvent::RunStarted));
        assert_eq!(
            events.last(),
            Some(&AgentEvent::RunFinished { exhausted: false })
        );
    }
}
