//! Context summarization: trait, default implementation, and transcript rendering.

use std::sync::Arc;

use async_trait::async_trait;

use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::message::{Message, Role, Visibility};

use super::plugin::CompactionConfig;

/// Minimum token savings required to justify a compaction LLM call.
pub const MIN_COMPACTION_GAIN_TOKENS: usize = 1024;

/// Abstraction for generating conversation summaries during compaction.
///
/// The framework provides token estimation, boundary finding, and transcript rendering.
/// Implementors decide the summarization strategy (prompt, model, parameters).
#[async_trait]
pub trait ContextSummarizer: Send + Sync {
    /// Generate a summary from a conversation transcript.
    ///
    /// - `transcript`: rendered text of the messages to summarize (Internal messages already filtered)
    /// - `previous_summary`: if a prior compaction summary exists, passed here for cumulative updates
    /// - `executor`: LLM executor to use for summarization
    async fn summarize(
        &self,
        transcript: &str,
        previous_summary: Option<&str>,
        executor: &dyn LlmExecutor,
    ) -> Result<String, String>;
}

/// Default summarizer that reads prompts from [`CompactionConfig`].
///
/// Uses cumulative summarization: if a previous summary exists, the prompt asks
/// the LLM to update it with the new conversation span rather than re-summarize everything.
#[derive(Default)]
pub struct DefaultSummarizer {
    config: CompactionConfig,
}

impl DefaultSummarizer {
    /// Create a summarizer with a specific compaction config.
    pub fn with_config(config: CompactionConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl ContextSummarizer for DefaultSummarizer {
    async fn summarize(
        &self,
        transcript: &str,
        previous_summary: Option<&str>,
        executor: &dyn LlmExecutor,
    ) -> Result<String, String> {
        let user_prompt = match previous_summary {
            Some(prev) if !prev.trim().is_empty() => format!(
                "Update the cumulative summary with the new conversation span.\n\n\
                 <existing-summary>\n{}\n</existing-summary>\n\n\
                 <new-conversation>\n{}\n</new-conversation>",
                prev.trim(),
                transcript.trim(),
            ),
            _ => self
                .config
                .summarizer_user_prompt
                .replace("{messages}", transcript.trim()),
        };

        let max_tokens = self.config.summary_max_tokens.unwrap_or(1024);
        let model = self.config.summary_model.clone().unwrap_or_default();

        let request = awaken_contract::contract::executor::InferenceRequest {
            model,
            messages: vec![
                Message::system(&self.config.summarizer_system_prompt),
                Message::user(user_prompt),
            ],
            tools: vec![],
            system: vec![],
            overrides: Some(awaken_contract::contract::inference::InferenceOverride {
                max_tokens: Some(max_tokens),
                ..Default::default()
            }),
            enable_prompt_cache: false,
        };

        let result = executor
            .execute(request)
            .await
            .map_err(|e| format!("summarization failed: {e}"))?;

        let text = result.text();
        if text.is_empty() {
            return Err("empty summary".into());
        }
        Ok(text)
    }
}

/// Render messages as a text transcript for LLM summarization.
///
/// Filters out `Visibility::Internal` messages — system-injected context that
/// gets re-injected each turn should not be included in the summary.
pub fn render_transcript(messages: &[Arc<Message>]) -> String {
    messages
        .iter()
        .filter(|m| m.visibility != Visibility::Internal)
        .filter_map(|m| {
            let text = m.text();
            if text.is_empty() {
                return None;
            }
            let role = match m.role {
                Role::System => "System",
                Role::User => "User",
                Role::Assistant => "Assistant",
                Role::Tool => "Tool",
            };
            Some(format!("[{role}]: {text}"))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Extract a previous compaction summary from the message list.
///
/// Looks for the first `internal_system` message containing `<conversation-summary>` tags.
pub fn extract_previous_summary(messages: &[Arc<Message>]) -> Option<String> {
    for msg in messages {
        if msg.role != Role::System || msg.visibility != Visibility::Internal {
            continue;
        }
        let text = msg.text();
        if let Some(start) = text.find("<conversation-summary>")
            && let Some(end) = text.find("</conversation-summary>")
        {
            let inner = &text[start + "<conversation-summary>".len()..end];
            let trimmed = inner.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::message::ToolCall;
    use serde_json::json;

    #[test]
    fn render_transcript_formats_correctly() {
        let messages = vec![
            Arc::new(Message::user("hello")),
            Arc::new(Message::assistant("hi there")),
        ];
        let transcript = render_transcript(&messages);
        assert!(transcript.contains("[User]: hello"));
        assert!(transcript.contains("[Assistant]: hi there"));
    }

    #[test]
    fn render_transcript_excludes_internal_messages() {
        let messages = vec![
            Arc::new(Message::internal_system("you are helpful")),
            Arc::new(Message::user("hello")),
            Arc::new(Message::assistant("hi")),
        ];
        let transcript = render_transcript(&messages);
        assert!(!transcript.contains("you are helpful"));
        assert!(transcript.contains("[User]: hello"));
    }

    #[test]
    fn extract_previous_summary_finds_summary() {
        let messages = vec![
            Arc::new(Message::internal_system(
                "<conversation-summary>\nPrevious summary text\n</conversation-summary>",
            )),
            Arc::new(Message::user("new msg")),
        ];
        let summary = extract_previous_summary(&messages);
        assert_eq!(summary.as_deref(), Some("Previous summary text"));
    }

    #[test]
    fn extract_previous_summary_none_without_summary() {
        let messages = vec![Arc::new(Message::user("hello"))];
        assert!(extract_previous_summary(&messages).is_none());
    }

    #[test]
    fn render_transcript_filters_internal_messages() {
        let messages = vec![
            Arc::new(Message::system("visible system")),
            Arc::new(Message::internal_system("hidden internal context")),
            Arc::new(Message::user("hello")),
            Arc::new(Message::assistant("hi")),
            Arc::new(Message::internal_system("another hidden")),
        ];
        let transcript = render_transcript(&messages);
        assert!(
            !transcript.contains("hidden internal context"),
            "internal messages should be filtered"
        );
        assert!(
            !transcript.contains("another hidden"),
            "all internal messages should be filtered"
        );
        assert!(transcript.contains("[System]: visible system"));
        assert!(transcript.contains("[User]: hello"));
        assert!(transcript.contains("[Assistant]: hi"));
    }

    #[test]
    fn render_transcript_formats_roles() {
        let messages = vec![
            Arc::new(Message::system("sys prompt")),
            Arc::new(Message::user("question")),
            Arc::new(Message::assistant("answer")),
            Arc::new(Message::tool("c1", "tool output")),
        ];
        let transcript = render_transcript(&messages);
        assert!(
            transcript.contains("[System]: sys prompt"),
            "system role format"
        );
        assert!(transcript.contains("[User]: question"), "user role format");
        assert!(
            transcript.contains("[Assistant]: answer"),
            "assistant role format"
        );
        assert!(
            transcript.contains("[Tool]: tool output"),
            "tool role format"
        );
    }

    #[test]
    fn render_transcript_empty_messages() {
        let messages: Vec<Arc<Message>> = vec![];
        let transcript = render_transcript(&messages);
        assert!(transcript.is_empty());
    }

    #[test]
    fn render_transcript_skips_empty_text_messages() {
        let messages = vec![
            Arc::new(Message::user("hello")),
            Arc::new(Message::assistant_with_tool_calls(
                "",
                vec![ToolCall::new("c1", "search", json!({}))],
            )),
            Arc::new(Message::assistant("visible")),
        ];
        let transcript = render_transcript(&messages);
        // The tool call message has empty text, should be skipped
        assert!(transcript.contains("[User]: hello"));
        assert!(transcript.contains("[Assistant]: visible"));
        // Count entries
        let entries: Vec<&str> = transcript.split("\n\n").filter(|s| !s.is_empty()).collect();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn extract_previous_summary_empty_summary_ignored() {
        let messages = vec![Arc::new(Message::internal_system(
            "<conversation-summary>   \n  \n  </conversation-summary>",
        ))];
        let summary = extract_previous_summary(&messages);
        assert!(
            summary.is_none(),
            "whitespace-only summary should be treated as empty"
        );
    }

    #[test]
    fn render_transcript_tool_messages_show_content() {
        let messages = vec![
            Arc::new(Message::user("search for something")),
            Arc::new(Message::tool("c1", "search result: found 5 items")),
        ];
        let transcript = render_transcript(&messages);
        assert!(transcript.contains("[Tool]: search result: found 5 items"));
    }

    #[test]
    fn extract_previous_summary_ignores_non_internal_system() {
        // Regular system message with summary tags should not be picked up
        let messages = vec![
            Arc::new(Message::system(
                "<conversation-summary>\nShould be ignored\n</conversation-summary>",
            )),
            Arc::new(Message::user("hello")),
        ];
        let summary = extract_previous_summary(&messages);
        assert!(
            summary.is_none(),
            "non-internal system message should not be extracted"
        );
    }

    #[test]
    fn compaction_config_default_values() {
        let config = CompactionConfig::default();
        assert!(
            config.summarizer_system_prompt.contains("summarizer"),
            "default system prompt should mention summarizer"
        );
        assert!(
            config.summarizer_user_prompt.contains("{messages}"),
            "default user prompt should contain {{messages}} template variable"
        );
        assert!(config.summary_max_tokens.is_none());
        assert!(config.summary_model.is_none());
        assert!(
            (config.min_savings_ratio - 0.3).abs() < f64::EPSILON,
            "default min_savings_ratio should be 0.3"
        );
    }

    #[test]
    fn compaction_config_serde_roundtrip() {
        let config = CompactionConfig {
            summarizer_system_prompt: "Custom prompt.".into(),
            summarizer_user_prompt: "Summarize: {messages}".into(),
            summary_max_tokens: Some(2048),
            summary_model: Some("gpt-4".into()),
            min_savings_ratio: 0.5,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: CompactionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.summarizer_system_prompt, "Custom prompt.");
        assert_eq!(parsed.summary_max_tokens, Some(2048));
        assert_eq!(parsed.summary_model.as_deref(), Some("gpt-4"));
    }

    #[test]
    fn compaction_config_key_binding() {
        use crate::context::plugin::CompactionConfigKey;
        use awaken_contract::registry_spec::PluginConfigKey;
        assert_eq!(CompactionConfigKey::KEY, "compaction");
    }
}
