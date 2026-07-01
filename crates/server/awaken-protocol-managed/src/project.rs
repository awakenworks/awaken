//! Project committed neutral `Message`s into public outbound event kinds.
//!
//! Events are projections over committed facts (never live output). The user's
//! own input is not re-emitted; an assistant turn becomes an `agent.message`
//! (its text) plus an `agent.tool_use` per tool call; a tool message becomes an
//! `agent.tool_result`. A turn ends with `session.status_idle`.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Message, Role};

use crate::dto::{OutboundKind, StopReason};

/// Project the messages committed during one turn, in order, then the terminal
/// idle marker. `stop` is the run's terminal reason mapped by the caller.
pub fn project_turn(messages: &[Message], stop: StopReason) -> Vec<OutboundKind> {
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
                    out.push(OutboundKind::AgentMessage { content: text });
                }
                for block in &message.content {
                    if let ContentBlock::ToolUse { name, input, .. } = block {
                        out.push(OutboundKind::AgentToolUse {
                            name: name.clone(),
                            input: input.clone(),
                            evaluated_permission: Some("allow".to_string()),
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
                        out.push(OutboundKind::AgentToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content: content.clone(),
                            is_error: None,
                        });
                    }
                }
            }
            Role::User | Role::System => {}
        }
    }
    out.push(OutboundKind::SessionStatusIdle { stop_reason: stop });
    out
}
