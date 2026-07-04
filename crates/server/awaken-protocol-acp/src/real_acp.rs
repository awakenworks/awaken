//! Official ACP codec projection (ADR-0041 Slice 3, `real-acp` feature).
//!
//! The anti-corruption layer's core: map the official `agent-client-protocol`
//! `SessionUpdate` / `StopReason` onto this crate's neutral [`AgentEvent`] /
//! [`TerminationReason`]. Substituting this for the newline-JSON stand-in leaves the
//! projection + [`crate::RunEventSink`] contract unchanged — the store never sees
//! ACP vocabulary. Pure data mapping, so it is unit-tested without a live agent.

use agent_client_protocol::{ContentBlock, SessionUpdate, StopReason};

use crate::{AgentEvent, TerminationReason};

/// Project one ACP `SessionUpdate` into a neutral [`AgentEvent`]. Returns `None` for
/// updates with no runtime projection (plans, mode/config updates, user input echoes).
#[must_use]
pub fn project_update(update: &SessionUpdate) -> Option<AgentEvent> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            text_of(&chunk.content).map(|text| AgentEvent::Message { text })
        }
        SessionUpdate::ToolCall(tool_call) => Some(AgentEvent::ToolCall {
            name: tool_call.title.clone(),
            input: serde_json::Value::Null,
        }),
        // Thoughts, tool-call updates, plans, mode/config/session-info and user echoes
        // have no runtime AgentEvent projection.
        _ => None,
    }
}

fn text_of(content: &ContentBlock) -> Option<String> {
    match content {
        ContentBlock::Text(text) => Some(text.text.clone()),
        _ => None,
    }
}

/// Map an ACP prompt `StopReason` to a neutral [`TerminationReason`].
#[must_use]
pub fn termination_from_stop_reason(reason: StopReason) -> TerminationReason {
    match reason {
        StopReason::EndTurn => TerminationReason::NaturalEnd,
        StopReason::Cancelled => TerminationReason::Cancelled,
        StopReason::Refusal => TerminationReason::Refusal,
        StopReason::MaxTokens | StopReason::MaxTurnRequests => TerminationReason::TimedOut,
        _ => TerminationReason::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::ContentChunk;

    #[test]
    fn agent_message_chunk_projects_to_a_neutral_message() {
        let update = SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from("hi")));
        assert_eq!(
            project_update(&update),
            Some(AgentEvent::Message { text: "hi".into() })
        );
    }

    #[test]
    fn a_user_message_chunk_has_no_runtime_projection() {
        let update = SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::from("hey")));
        assert_eq!(project_update(&update), None);
    }

    #[test]
    fn stop_reasons_map_to_neutral_terminations() {
        assert_eq!(
            termination_from_stop_reason(StopReason::EndTurn),
            TerminationReason::NaturalEnd
        );
        assert_eq!(
            termination_from_stop_reason(StopReason::Cancelled),
            TerminationReason::Cancelled
        );
        assert_eq!(
            termination_from_stop_reason(StopReason::Refusal),
            TerminationReason::Refusal
        );
        assert_eq!(
            termination_from_stop_reason(StopReason::MaxTokens),
            TerminationReason::TimedOut
        );
    }
}
