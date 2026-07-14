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
    /// The run ended on an execution fault. `code` is the fault's stable
    /// snake_case classification (e.g. `unauthorized`, `context_overflow`),
    /// so a host can categorize the failure without parsing `message`.
    RunFailed { code: String, message: String },
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

/// A tool call the assistant made, borrowed from the committed message during a
/// [`project_history`] walk. Adapters shape it into their own tool-part vocabulary.
pub struct ToolUseRef<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub input: &'a Value,
}

/// The static-history counterpart to [`Transcoder`]: a sink that receives the
/// committed messages of a thread, oldest-first, already walked and correlated.
/// The fold ([`project_history`]) owns the shared logic — skip-empty, tool-call
/// extraction, and per-message tool-result indexing; each adapter implements this
/// to shape a message into its own wire form. This is why two adapters that
/// project tool results differently (AI SDK folds a result into the assistant's
/// tool part; AG-UI emits a standalone `tool` message) still share one fold.
pub trait HistorySink {
    /// A user or system message; `content` is its blocks (never all-empty — an
    /// empty message is skipped by the fold). The adapter extracts text/parts.
    fn user_or_system(&mut self, id: &str, role: Role, content: &[ContentBlock]);
    /// An assistant turn: its `content` blocks (text; tool-use blocks live here
    /// too) and the tool calls pre-extracted. Skipped by the fold only when the
    /// text is empty *and* there are no tool calls.
    fn assistant(&mut self, id: &str, content: &[ContentBlock], tools: &[ToolUseRef<'_>]);
    /// A tool result answering `tool_use_id`, its `content` blocks. `message_id`+
    /// `sub` locate it inside the neutral tool message (`sub` 0 = first result);
    /// an adapter emitting a standalone message keys on these, one folding it into
    /// the assistant part keys on `tool_use_id`.
    fn tool_result(
        &mut self,
        message_id: &str,
        sub: usize,
        tool_use_id: &str,
        content: &[ContentBlock],
    );
}

/// True when a block list carries no text (text blocks only) — the fold's
/// skip-empty test, matching the streaming projection.
fn text_is_empty(content: &[ContentBlock]) -> bool {
    !content
        .iter()
        .any(|b| matches!(b, ContentBlock::Text { text } if !text.is_empty()))
}

/// Walk a thread's committed messages (oldest-first) into `sink`, owning the
/// shared read-model logic so each adapter only shapes each message. An empty
/// user/system/assistant message is dropped, as the streaming projection drops it.
pub fn project_history(messages: &[Message], sink: &mut impl HistorySink) {
    for message in messages {
        match message.role {
            Role::User | Role::System => {
                if text_is_empty(&message.content) {
                    continue;
                }
                sink.user_or_system(&message.id.0, message.role, &message.content);
            }
            Role::Assistant => {
                let tools: Vec<ToolUseRef<'_>> = message
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, name, input } => Some(ToolUseRef {
                            id: id.as_str(),
                            name: name.as_str(),
                            input,
                        }),
                        _ => None,
                    })
                    .collect();
                if text_is_empty(&message.content) && tools.is_empty() {
                    continue;
                }
                sink.assistant(&message.id.0, &message.content, &tools);
            }
            Role::Tool => {
                let mut sub = 0usize;
                for block in &message.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                    } = block
                    {
                        sink.tool_result(&message.id.0, sub, tool_use_id, content);
                        sub += 1;
                    }
                }
            }
        }
    }
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

/// The terminal projection event for a phase. A fault projects as `RunFailed`
/// carrying its classification code, so hosts can tell a failed run from a
/// finished one without reading the committed phase.
pub fn terminal(phase: &Phase, pending: Option<(&str, bool)>) -> AgentEvent {
    match phase {
        // Not a terminus: a run committed mid-flight projects as its
        // in-progress signal. Hosts normally project only parked/ended phases.
        Phase::Running => AgentEvent::RunStarted,
        Phase::Waiting => AgentEvent::Waiting {
            pending_tool_use_id: pending.map(|p| p.0.to_string()),
        },
        Phase::Ended(EndCause::MaxSteps) => AgentEvent::RunFinished { exhausted: true },
        Phase::Ended(EndCause::Error(failure)) => AgentEvent::RunFailed {
            code: failure.code().to_string(),
            message: failure.message(),
        },
        // G26: indeterminate remote execution is explicit; it is never silently
        // converted to success.
        Phase::Ended(EndCause::Indeterminate) => AgentEvent::RunFailed {
            code: "indeterminate".to_string(),
            message: "execution outcome could not be determined".to_string(),
        },
        Phase::Ended(_) => AgentEvent::RunFinished { exhausted: false },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::message::Id;
    use crate::agent::run::Failure;

    #[test]
    fn error_terminal_projects_run_failed_with_the_fault_code() {
        let inference = Phase::Ended(EndCause::Error(Failure::Inference {
            code: "unauthorized".to_string(),
            message: "bad api key".to_string(),
        }));
        assert_eq!(
            terminal(&inference, None),
            AgentEvent::RunFailed {
                code: "unauthorized".to_string(),
                message: "bad api key".to_string(),
            }
        );

        let capability = Phase::Ended(EndCause::Error(Failure::CapabilityBound));
        assert!(matches!(
            terminal(&capability, None),
            AgentEvent::RunFailed { code, .. } if code == "capability_bound"
        ));
    }

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

    // G26: indeterminate remote execution is explicit; it is never silently
    // projected as success.
    #[test]
    fn indeterminate_projects_as_run_failed_not_run_finished() {
        let phase = Phase::Ended(EndCause::Indeterminate);
        let event = terminal(&phase, None);
        assert!(
            matches!(&event, AgentEvent::RunFailed { code, .. } if code == "indeterminate"),
            "Indeterminate must project to RunFailed, got {event:?}"
        );
    }

    #[test]
    fn indeterminate_round_trips_through_serde() {
        let cause = EndCause::Indeterminate;
        let json = serde_json::to_string(&cause).unwrap();
        let parsed: EndCause = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, EndCause::Indeterminate);
    }
}
