//! Content-telemetry rendering for the GenAI spans (ADR-0050).
//!
//! Turns chat messages into the compact text form recorded under
//! `gen_ai.input.messages` / `gen_ai.output.messages`. The call site gates the
//! result on the [`CaptureDecision`](awaken_runtime_contract::CaptureDecision)
//! (default `Structured` records nothing) and scrubs it through the redactor.

use std::sync::Arc;

use awaken_runtime_contract::llm::ChatMessage;
use awaken_runtime_contract::{CaptureDecision, CaptureSink, ContentKind, DataSubjectId, Purpose};

/// The subject + sink to persist captured content to, when both are present.
pub(crate) type SinkTarget<'a> = Option<(&'a Arc<dyn CaptureSink>, &'a DataSubjectId)>;

/// Gate one piece of content on the capture decision (ADR-0050): if the level
/// permits it, record the redactor-scrubbed text onto the current span's
/// `field`, and — when a subject-tagged sink is wired — write it to that sink
/// (best-effort, off the committed path).
pub(crate) async fn emit_content(
    capture: &CaptureDecision,
    sink: SinkTarget<'_>,
    kind: ContentKind,
    field: &'static str,
    text: &str,
) {
    let Some(scrubbed) = capture.content(kind, text) else {
        return;
    };
    tracing::Span::current().record(field, scrubbed.as_ref());
    if let Some((sink, subject)) = sink {
        sink.record(subject, Purpose::TelemetryContent, kind, scrubbed.as_ref())
            .await;
    }
}

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
