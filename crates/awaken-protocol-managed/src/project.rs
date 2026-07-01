//! Project committed neutral messages into public Managed Agents events.
//!
//! The message fold is shared (`awaken_agent_contract::project`); this module
//! only owns the Managed *transcoder* — the `AgentEvent -> OutboundKind` mapping
//! — and the terminal `session.status_idle` shape. An assistant turn becomes an
//! `agent.message` plus an `agent.tool_use` (or `agent.custom_tool_use`) per call;
//! a tool message becomes an `agent.tool_result`; a step ends with
//! `session.status_idle`. A pending client tool projects as `agent.custom_tool_use`;
//! a pending built-in tool as `agent.tool_use{ask}`; anything else ran
//! (`agent.tool_use{allow}`).

use awaken_agent_contract::project::{
    AgentEvent, ToolDisposition, Transcoder, project_messages as fold, terminal_waiting,
};

use crate::dto::{OutboundKind, StopReason};

/// One projected event, with an optional stable id. A tool-use event carries the
/// tool call's own id (so a `user.tool_confirmation` can reference it); other
/// events let the adapter mint an `evt_*` id.
pub struct ProjectedEvent {
    pub id: Option<String>,
    pub kind: OutboundKind,
}

impl ProjectedEvent {
    fn minted(kind: OutboundKind) -> Self {
        Self { id: None, kind }
    }
    fn with_id(id: String, kind: OutboundKind) -> Self {
        Self { id: Some(id), kind }
    }
}

/// The Managed Agents transcoder: neutral projection events to public
/// `OutboundKind`. `RunStarted` is dropped (Managed has no per-step start event);
/// terminal events become `session.status_idle`.
#[derive(Default)]
pub struct ManagedEncoder;

impl Transcoder for ManagedEncoder {
    type Output = ProjectedEvent;

    fn transcode(&mut self, event: &AgentEvent) -> Vec<ProjectedEvent> {
        match event {
            AgentEvent::RunStarted => Vec::new(),
            AgentEvent::AssistantMessage { content, .. } => {
                vec![ProjectedEvent::minted(OutboundKind::AgentMessage {
                    content: content.clone(),
                })]
            }
            AgentEvent::ToolCall {
                id,
                name,
                input,
                disposition,
            } => {
                let kind = match disposition {
                    ToolDisposition::PendingClient => OutboundKind::AgentCustomToolUse {
                        name: name.clone(),
                        input: input.clone(),
                    },
                    ToolDisposition::PendingBuiltin => OutboundKind::AgentToolUse {
                        name: name.clone(),
                        input: input.clone(),
                        evaluated_permission: Some("ask".to_string()),
                    },
                    ToolDisposition::Executed => OutboundKind::AgentToolUse {
                        name: name.clone(),
                        input: input.clone(),
                        evaluated_permission: Some("allow".to_string()),
                    },
                };
                vec![ProjectedEvent::with_id(id.clone(), kind)]
            }
            AgentEvent::ToolResult { id, content, .. } => {
                vec![ProjectedEvent::minted(OutboundKind::AgentToolResult {
                    tool_use_id: id.clone(),
                    content: content.clone(),
                    is_error: None,
                })]
            }
            AgentEvent::Waiting {
                pending_tool_use_id,
            } => vec![ProjectedEvent::minted(OutboundKind::SessionStatusIdle {
                stop_reason: StopReason::RequiresAction {
                    event_ids: pending_tool_use_id.clone().into_iter().collect(),
                },
            })],
            AgentEvent::RunFinished { exhausted } => {
                let stop_reason = if *exhausted {
                    StopReason::RetriesExhausted
                } else {
                    StopReason::EndTurn
                };
                vec![ProjectedEvent::minted(OutboundKind::SessionStatusIdle {
                    stop_reason,
                })]
            }
        }
    }
}

/// Project just the agent-visible events for a batch of committed messages (no
/// terminal `session.status_idle`). Used by both a turn and an outcome iteration.
/// `pending` is `(tool_use_id, client_executed)` of the tool the run parked on.
pub fn project_messages(
    messages: &[awaken_agent_contract::agent::message::Message],
    pending: Option<(&str, bool)>,
) -> Vec<ProjectedEvent> {
    ManagedEncoder.transcode_all(&fold(messages, pending))
}

/// Project the messages committed during one step, then a terminal
/// `session.status_idle` derived from `stop`. When `stop` is `RequiresAction` the
/// pending tool's id populates `requires_action.event_ids`.
pub fn project_turn(
    messages: &[awaken_agent_contract::agent::message::Message],
    stop: StopReason,
    pending: Option<(&str, bool)>,
) -> Vec<ProjectedEvent> {
    let mut events = fold(messages, pending);
    events.push(terminal_event(stop, pending));
    ManagedEncoder.transcode_all(&events)
}

/// The neutral terminal event for a Managed `stop_reason`. `RequiresAction`'s
/// event ids are refilled from the pending tool.
fn terminal_event(stop: StopReason, pending: Option<(&str, bool)>) -> AgentEvent {
    match stop {
        StopReason::RequiresAction { .. } => terminal_waiting(pending.map(|p| p.0)),
        StopReason::RetriesExhausted => AgentEvent::RunFinished { exhausted: true },
        StopReason::EndTurn => AgentEvent::RunFinished { exhausted: false },
    }
}
