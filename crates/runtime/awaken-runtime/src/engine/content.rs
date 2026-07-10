//! Content-telemetry rendering for the GenAI spans (ADR-0050).
//!
//! Turns chat messages into the compact text form recorded under
//! `gen_ai.input.messages` / `gen_ai.output.messages`. The call site gates the
//! result on the [`CaptureDecision`](awaken_runtime_contract::CaptureDecision)
//! (default `Structured` records nothing) and scrubs it through the redactor.

use awaken_runtime_contract::llm::ChatMessage;

/// Render chat messages to a compact text form for content telemetry. Only each
/// block's text — the PII-bearing part — is included; non-text blocks (e.g.
/// images) are omitted. The result is redactor-scrubbed before recording.
pub(crate) fn render_chat_messages(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .map(|m| {
            format!(
                "{:?}: {}",
                m.role,
                awaken_agent_contract::agent::content::extract_text(&m.content)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::render_chat_messages;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_runtime_contract::llm::{ChatMessage, ChatRole};

    #[test]
    fn render_chat_messages_joins_role_and_text() {
        let msgs = vec![
            ChatMessage {
                role: ChatRole::System,
                content: vec![ContentBlock::text("be nice")],
            },
            ChatMessage {
                role: ChatRole::User,
                content: vec![ContentBlock::text("hi there")],
            },
        ];
        assert_eq!(
            render_chat_messages(&msgs),
            "System: be nice\nUser: hi there"
        );
    }
}
