//! Project committed neutral `Message`s into public outbound events.
//!
//! Events are projections over committed facts. An assistant turn becomes an
//! `agent.message` (its text) plus an `agent.tool_use` per tool call; a tool
//! message becomes an `agent.tool_result`; a turn ends with `session.status_idle`.
//! When the run parked for approval, the pending tool's `agent.tool_use` is marked
//! `evaluated_permission: "ask"` and the terminal `session.status_idle` carries
//! `requires_action` referencing it — so the client knows to send a
//! `user.tool_confirmation`.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Message, Role};

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

/// Project the messages committed during one step. `pending` is the tool-use id
/// the run parked on (when `stop` is `RequiresAction`), which marks that tool
/// `ask` and populates `requires_action.event_ids`.
pub fn project_turn(
    messages: &[Message],
    stop: StopReason,
    pending: Option<&str>,
) -> Vec<ProjectedEvent> {
    let mut out = Vec::new();
    for message in messages {
        match message.role {
            Role::Assistant => {
                let text: Vec<ContentBlock> = message
                    .content
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::Text { .. }))
                    .cloned()
                    .collect();
                if !text.is_empty() {
                    out.push(ProjectedEvent::minted(OutboundKind::AgentMessage {
                        content: text,
                    }));
                }
                for block in &message.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        let permission = if pending == Some(id.as_str()) {
                            "ask"
                        } else {
                            "allow"
                        };
                        out.push(ProjectedEvent::with_id(
                            id.clone(),
                            OutboundKind::AgentToolUse {
                                name: name.clone(),
                                input: input.clone(),
                                evaluated_permission: Some(permission.to_string()),
                            },
                        ));
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
                        out.push(ProjectedEvent::minted(OutboundKind::AgentToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content: content.clone(),
                            is_error: None,
                        }));
                    }
                }
            }
            Role::User | Role::System => {}
        }
    }
    let stop_reason = match stop {
        StopReason::RequiresAction { .. } => StopReason::RequiresAction {
            event_ids: pending.map(|p| vec![p.to_string()]).unwrap_or_default(),
        },
        other => other,
    };
    out.push(ProjectedEvent::minted(OutboundKind::SessionStatusIdle {
        stop_reason,
    }));
    out
}
