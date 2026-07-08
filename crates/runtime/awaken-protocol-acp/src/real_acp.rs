//! Official ACP codec projection (ADR-0041 Slice 3, `real-acp` feature).
//!
//! The anti-corruption layer's core: map the official `agent-client-protocol`
//! `SessionUpdate` / `StopReason` onto this crate's neutral [`AgentEvent`] /
//! [`TerminationReason`]. Substituting this for the newline-JSON stand-in leaves the
//! projection + [`crate::RunFactAppender`] contract unchanged — the store never sees
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
            // Carry the tool's raw arguments through the ACL (the model's request).
            // The agent runs the tool inside its own OS jail, so the tool *result*
            // is its internal state, not surfaced to our transcript.
            input: tool_call
                .raw_input
                .clone()
                .unwrap_or(serde_json::Value::Null),
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

/// Reduce an official `agent-client-protocol` transport error to the neutral
/// [`RawAcpError`](crate::RawAcpError) the classifier consumes: the rendered
/// message, plus the structured `errorKind`/`error_kind` and JSON-RPC code when
/// present. So [`classify_error`](crate::classify_error) works identically over
/// the real codec and the newline-JSON stand-in.
#[must_use]
pub fn raw_error_from_acp(err: &agent_client_protocol::Error) -> crate::RawAcpError {
    let kind = err.data.as_ref().and_then(|data| {
        data.get("errorKind")
            .or_else(|| data.get("error_kind"))
            .and_then(|k| k.as_str())
            .map(str::to_string)
    });
    crate::RawAcpError {
        message: err.message.clone(),
        kind,
        code: Some(i32::from(err.code)),
        retry_after_secs: None,
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
