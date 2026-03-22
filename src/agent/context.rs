//! Built-in context management: hard truncation, LLM compaction, summarizer trait.

use async_trait::async_trait;

use crate::contract::content::ContentBlock;
use crate::contract::executor::LlmExecutor;
use crate::contract::inference::ContextWindowPolicy;
use crate::contract::message::{Message, Role, Visibility};
use crate::contract::tool::ToolDescriptor;
use crate::contract::transform::{
    InferenceRequestTransform, TransformOutput, estimate_message_tokens, estimate_tokens,
    estimate_tool_tokens, patch_dangling_tool_calls,
};

// ---------------------------------------------------------------------------
// Artifact compaction
// ---------------------------------------------------------------------------

/// Token threshold above which a tool result is compacted to a preview.
pub const ARTIFACT_COMPACT_THRESHOLD_TOKENS: usize = 2048;

/// Maximum characters retained in a compacted artifact preview.
pub const ARTIFACT_PREVIEW_MAX_CHARS: usize = 1600;

/// Maximum lines retained in a compacted artifact preview.
pub const ARTIFACT_PREVIEW_MAX_LINES: usize = 24;

/// Compact a single artifact string if it exceeds the token threshold.
///
/// Returns the original content unchanged when estimated tokens are within budget.
/// Otherwise truncates to [`ARTIFACT_PREVIEW_MAX_CHARS`] / [`ARTIFACT_PREVIEW_MAX_LINES`]
/// (whichever is shorter) and appends a compaction indicator.
pub fn compact_artifact(content: &str) -> String {
    let estimated_tokens = content.len() / 4;
    if estimated_tokens < ARTIFACT_COMPACT_THRESHOLD_TOKENS {
        return content.to_string();
    }

    // Truncate by line limit first, then by char limit
    let mut char_count = 0usize;
    let mut line_count = 0usize;
    let mut end_byte = 0usize;

    for (idx, ch) in content.char_indices() {
        if char_count >= ARTIFACT_PREVIEW_MAX_CHARS || line_count >= ARTIFACT_PREVIEW_MAX_LINES {
            break;
        }
        if ch == '\n' {
            line_count += 1;
        }
        char_count += 1;
        end_byte = idx + ch.len_utf8();
    }

    let preview = &content[..end_byte];
    format!(
        "{preview}\n\n[Content compacted: original ~{estimated_tokens} tokens, showing first {char_count} chars]"
    )
}

/// Compact tool result messages that exceed the artifact token threshold.
///
/// Iterates over all `Role::Tool` messages and replaces oversized text content
/// blocks with a truncated preview plus compaction indicator.
pub fn compact_tool_results(messages: &mut [Message]) {
    for msg in messages.iter_mut() {
        if msg.role != Role::Tool {
            continue;
        }
        let mut modified = false;
        let new_content: Vec<ContentBlock> = msg
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => {
                    let compacted = compact_artifact(text);
                    if compacted.len() != text.len() {
                        modified = true;
                    }
                    ContentBlock::Text { text: compacted }
                }
                other => other.clone(),
            })
            .collect();
        if modified {
            msg.content = new_content;
        }
    }
}

// ---------------------------------------------------------------------------
// ContextSummarizer trait
// ---------------------------------------------------------------------------

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

/// Default summarizer with a domain-agnostic prompt.
///
/// Uses cumulative summarization: if a previous summary exists, the prompt asks
/// the LLM to update it with the new conversation span rather than re-summarize everything.
pub struct DefaultSummarizer {
    /// Maximum output tokens for the summary.
    pub max_tokens: u32,
}

impl Default for DefaultSummarizer {
    fn default() -> Self {
        Self { max_tokens: 1024 }
    }
}

const DEFAULT_SYSTEM_PROMPT: &str = "\
You maintain a durable conversation summary for an agent runtime. \
Produce a concise but lossless working summary for future turns. \
Preserve user goals, constraints, preferences, decisions, completed work, \
important findings, identifiers, and unresolved follow-ups. \
Output plain text only; do not mention the summarization process.";

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
            _ => format!(
                "Summarize the following conversation:\n\n\
                 <conversation>\n{}\n</conversation>",
                transcript.trim(),
            ),
        };

        let request = crate::contract::executor::InferenceRequest {
            model: String::new(), // executor decides the model
            messages: vec![
                Message::system(DEFAULT_SYSTEM_PROMPT),
                Message::user(user_prompt),
            ],
            tools: vec![],
            system: vec![],
            overrides: Some(crate::contract::inference::InferenceOverride {
                max_tokens: Some(self.max_tokens),
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

/// Minimum token savings required to justify a compaction LLM call.
pub const MIN_COMPACTION_GAIN_TOKENS: usize = 1024;

/// Built-in request transform: truncate messages to fit the token budget.
///
/// Preserves all system messages and the most recent conversation messages.
/// Adjusts split points to avoid orphaning tool call/result pairs.
pub struct ContextTransform {
    policy: ContextWindowPolicy,
}

impl ContextTransform {
    pub fn new(policy: ContextWindowPolicy) -> Self {
        Self { policy }
    }
}

impl InferenceRequestTransform for ContextTransform {
    fn transform(
        &self,
        mut messages: Vec<Message>,
        tool_descriptors: &[ToolDescriptor],
    ) -> TransformOutput {
        // Compact oversized tool results before truncation
        compact_tool_results(&mut messages);

        let tool_tokens = estimate_tool_tokens(tool_descriptors);
        let available = self
            .policy
            .max_context_tokens
            .saturating_sub(self.policy.max_output_tokens)
            .saturating_sub(tool_tokens);

        let total = estimate_tokens(&messages);
        if total <= available {
            return TransformOutput { messages };
        }

        // Split into system prefix and history
        let system_end = messages
            .iter()
            .position(|m| m.role != Role::System)
            .unwrap_or(messages.len());

        let system_tokens: usize = messages[..system_end]
            .iter()
            .map(estimate_message_tokens)
            .sum();
        let history_budget = available.saturating_sub(system_tokens);

        // Find split point: walk backward from end, accumulating tokens
        let history = &messages[system_end..];
        let split = find_split_point(history, history_budget, self.policy.min_recent_messages);
        let absolute_split = system_end + split;

        // Remove truncated messages
        let dropped = absolute_split.saturating_sub(system_end);
        if absolute_split > system_end {
            messages.drain(system_end..absolute_split);
        }
        let kept = messages.len();

        // Repair dangling tool calls after truncation
        patch_dangling_tool_calls(&mut messages);

        tracing::debug!(dropped, kept, "truncation_applied");

        TransformOutput { messages }
    }
}

/// Find the split point in history that fits the token budget.
///
/// Always keeps at least `min_recent` messages from the end.
/// Adjusts boundaries to avoid splitting tool call/result pairs.
fn find_split_point(history: &[Message], budget_tokens: usize, min_recent: usize) -> usize {
    if history.is_empty() {
        return 0;
    }

    let must_keep = min_recent.min(history.len());
    let must_keep_start = history.len().saturating_sub(must_keep);

    let mut used_tokens = 0usize;
    let mut candidate_split = history.len();

    for i in (0..history.len()).rev() {
        let msg_tokens = estimate_message_tokens(&history[i]);
        let new_total = used_tokens + msg_tokens;

        if i >= must_keep_start {
            used_tokens = new_total;
            candidate_split = i;
            continue;
        }

        if new_total > budget_tokens {
            break;
        }

        used_tokens = new_total;
        candidate_split = i;
    }

    adjust_split_for_tool_pairs(history, candidate_split)
}

/// Adjust split to avoid orphaning tool call/result pairs.
fn adjust_split_for_tool_pairs(history: &[Message], mut split: usize) -> usize {
    // If first kept message is Tool, move split backward to include paired Assistant
    while split > 0 && history[split].role == Role::Tool {
        split -= 1;
    }

    // If last dropped message is Assistant with tool_calls,
    // move split forward to drop orphaned tool results
    if split > 0 {
        let last_dropped = &history[split - 1];
        if last_dropped.role == Role::Assistant && last_dropped.tool_calls.is_some() {
            while split < history.len() && history[split].role == Role::Tool {
                split += 1;
            }
        }
    }

    split
}

// ---------------------------------------------------------------------------
// LLM compaction
// ---------------------------------------------------------------------------

/// Find a safe compaction boundary in the message history.
///
/// Returns the index of the last message that can be safely compacted
/// (all tool call/result pairs are complete before this point).
pub fn find_compaction_boundary(
    messages: &[std::sync::Arc<Message>],
    start: usize,
    end: usize,
) -> Option<usize> {
    use std::collections::HashSet;

    let mut open_calls = HashSet::<String>::new();
    let mut best_boundary = None;

    for (idx, msg) in messages.iter().enumerate().skip(start).take(end - start) {
        if let Some(ref calls) = msg.tool_calls {
            for call in calls {
                open_calls.insert(call.id.clone());
            }
        }

        if msg.role == Role::Tool {
            if let Some(ref call_id) = msg.tool_call_id {
                open_calls.remove(call_id);
            }
        }

        // Safe boundary: all tool calls resolved and next isn't a tool result
        let next_is_tool = messages
            .get(idx + 1)
            .is_some_and(|next| next.role == Role::Tool);

        if open_calls.is_empty() && !next_is_tool {
            best_boundary = Some(idx);
        }
    }

    best_boundary
}

/// Render messages as a text transcript for LLM summarization.
///
/// Filters out `Visibility::Internal` messages — system-injected context that
/// gets re-injected each turn should not be included in the summary.
pub fn render_transcript(messages: &[std::sync::Arc<Message>]) -> String {
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
pub fn extract_previous_summary(messages: &[std::sync::Arc<Message>]) -> Option<String> {
    for msg in messages {
        if msg.role != Role::System || msg.visibility != Visibility::Internal {
            continue;
        }
        let text = msg.text();
        if let Some(start) = text.find("<conversation-summary>") {
            if let Some(end) = text.find("</conversation-summary>") {
                let inner = &text[start + "<conversation-summary>".len()..end];
                let trimmed = inner.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Load-time trim
// ---------------------------------------------------------------------------

/// Trim loaded messages to the latest compaction boundary.
///
/// If the message list contains a `<conversation-summary>` internal_system message,
/// all messages before it are dropped. The summary message becomes the first message.
/// This avoids loading already-summarized history into the context window.
///
/// Idempotent: if no summary exists or messages are already trimmed, this is a no-op.
pub fn trim_to_compaction_boundary(messages: &mut Vec<std::sync::Arc<Message>>) {
    // Find the last summary message (in case of multiple compactions)
    let last_summary_idx = messages.iter().rposition(|m| {
        m.role == Role::System
            && m.visibility == Visibility::Internal
            && m.text().contains("<conversation-summary>")
    });

    if let Some(idx) = last_summary_idx {
        if idx > 0 {
            messages.drain(..idx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::message::ToolCall;
    use serde_json::json;

    fn make_policy(max_tokens: usize, min_recent: usize) -> ContextWindowPolicy {
        ContextWindowPolicy {
            max_context_tokens: max_tokens,
            max_output_tokens: 0,
            min_recent_messages: min_recent,
            enable_prompt_cache: false,
            autocompact_threshold: None,
            compaction_mode: Default::default(),
            compaction_raw_suffix_messages: 2,
        }
    }

    #[test]
    fn truncation_preserves_all_when_under_budget() {
        let transform = ContextTransform::new(make_policy(100_000, 2));
        let messages = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant("hi"),
        ];
        let output = transform.transform(messages.clone(), &[]);
        assert_eq!(output.messages.len(), 3);
    }

    #[test]
    fn truncation_keeps_system_and_recent() {
        // Very tight budget: system + ~2 recent messages
        let transform = ContextTransform::new(make_policy(50, 2));
        let mut messages = vec![Message::system("sys")];
        // Add many user/assistant turns
        for i in 0..20 {
            messages.push(Message::user(format!("msg {i}")));
            messages.push(Message::assistant(format!("reply {i}")));
        }

        let output = transform.transform(messages, &[]);
        // Should have system + at least 2 recent messages
        assert!(output.messages.len() >= 3);
        assert_eq!(output.messages[0].role, Role::System);
    }

    #[test]
    fn truncation_repairs_dangling_tool_calls() {
        let transform = ContextTransform::new(make_policy(30, 1));
        let messages = vec![
            Message::system("sys"),
            Message::user("old msg 1"),
            Message::assistant_with_tool_calls(
                "calling",
                vec![ToolCall::new("c1", "search", json!({}))],
            ),
            Message::tool("c1", "result"),
            Message::user("old msg 2"),
            Message::assistant("old reply"),
            // many more to force truncation...
            Message::user("recent"),
            Message::assistant("recent reply"),
        ];

        let output = transform.transform(messages, &[]);
        // Should not have orphaned tool calls
        for (i, msg) in output.messages.iter().enumerate() {
            if msg.role == Role::Assistant {
                if let Some(ref calls) = msg.tool_calls {
                    for call in calls {
                        assert!(
                            output.messages[i + 1..]
                                .iter()
                                .any(|m| m.tool_call_id.as_deref() == Some(&call.id)),
                            "tool call {} should have a matching result",
                            call.id
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn find_compaction_boundary_respects_tool_pairs() {
        use std::sync::Arc;

        let messages: Vec<Arc<Message>> = vec![
            Arc::new(Message::user("start")),
            Arc::new(Message::assistant_with_tool_calls(
                "",
                vec![ToolCall::new("c1", "search", json!({}))],
            )),
            Arc::new(Message::tool("c1", "found")),
            Arc::new(Message::user("next")), // safe boundary here (idx 3)
            Arc::new(Message::assistant("reply")),
        ];

        let boundary = find_compaction_boundary(&messages, 0, messages.len());
        // Should be at idx 3 or 4 (after tool pair is complete)
        assert!(boundary.is_some());
        let b = boundary.unwrap();
        assert!(b >= 3);
    }

    #[test]
    fn render_transcript_formats_correctly() {
        use std::sync::Arc;

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
        use std::sync::Arc;

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
    fn trim_to_compaction_boundary_drops_pre_summary() {
        use std::sync::Arc;

        let mut messages = vec![
            Arc::new(Message::user("old msg 1")),
            Arc::new(Message::assistant("old reply")),
            Arc::new(Message::internal_system(
                "<conversation-summary>\nSummary of old messages\n</conversation-summary>",
            )),
            Arc::new(Message::user("new msg")),
            Arc::new(Message::assistant("new reply")),
        ];

        trim_to_compaction_boundary(&mut messages);
        assert_eq!(messages.len(), 3);
        assert!(messages[0].text().contains("conversation-summary"));
        assert_eq!(messages[1].text(), "new msg");
    }

    #[test]
    fn trim_to_compaction_boundary_noop_without_summary() {
        use std::sync::Arc;

        let mut messages = vec![
            Arc::new(Message::user("hello")),
            Arc::new(Message::assistant("hi")),
        ];
        let len_before = messages.len();
        trim_to_compaction_boundary(&mut messages);
        assert_eq!(messages.len(), len_before);
    }

    #[test]
    fn extract_previous_summary_finds_summary() {
        use std::sync::Arc;

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
    fn truncation_tool_pair_not_broken() {
        // Tight budget — truncation should not split an assistant+tool pair
        let transform = ContextTransform::new(make_policy(60, 1));
        let messages = vec![
            Message::system("sys"),
            Message::user("old"),
            Message::assistant_with_tool_calls(
                "calling",
                vec![ToolCall::new("c1", "search", json!({}))],
            ),
            Message::tool("c1", "found"),
            Message::user("recent"),
            Message::assistant("reply"),
        ];

        let output = transform.transform(messages, &[]);
        // If the assistant with tool_calls is kept, its tool result must also be kept
        for (i, msg) in output.messages.iter().enumerate() {
            if msg.role == Role::Assistant {
                if let Some(ref calls) = msg.tool_calls {
                    for call in calls {
                        let has_result = output.messages[i + 1..]
                            .iter()
                            .any(|m| m.tool_call_id.as_deref() == Some(&call.id));
                        assert!(
                            has_result,
                            "tool call {} should have matching result",
                            call.id
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn truncation_with_tool_descriptors_reduces_budget() {
        use crate::contract::tool::ToolDescriptor;

        let transform = ContextTransform::new(make_policy(100, 2));
        let messages = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant("world"),
        ];

        // Without tools: all fit
        let output_no_tools = transform.transform(messages.clone(), &[]);
        let count_no_tools = output_no_tools.messages.len();

        // With large tool schemas: might truncate
        let big_tool = ToolDescriptor {
            id: "t".into(),
            name: "t".into(),
            description: "x".repeat(200),
            parameters: json!({"type": "object", "properties": {
                "a": {"type": "string"}, "b": {"type": "string"},
                "c": {"type": "string"}, "d": {"type": "string"},
            }}),
            category: None,
        };

        let output_with_tools = transform.transform(messages, &[big_tool]);
        // With tools consuming budget, we may have fewer messages
        assert!(output_with_tools.messages.len() <= count_no_tools);
    }

    #[test]
    fn find_compaction_boundary_does_not_cut_open_tool_round() {
        use std::sync::Arc;

        let messages: Vec<Arc<Message>> = vec![
            Arc::new(Message::user("start")),
            Arc::new(Message::assistant("reply")),
            Arc::new(Message::user("next")),
            Arc::new(Message::assistant_with_tool_calls(
                "",
                vec![ToolCall::new("c1", "search", json!({}))],
            )),
            // c1 has no result yet — open tool round
        ];

        let boundary = find_compaction_boundary(&messages, 0, messages.len());
        // Boundary should be before the open tool round (idx 2 at latest)
        if let Some(b) = boundary {
            assert!(b <= 2, "boundary should not include open tool round");
        }
    }

    #[test]
    fn trim_to_compaction_boundary_idempotent() {
        use std::sync::Arc;

        let mut messages = vec![
            Arc::new(Message::user("old")),
            Arc::new(Message::internal_system(
                "<conversation-summary>\nSummary\n</conversation-summary>",
            )),
            Arc::new(Message::user("new")),
        ];

        trim_to_compaction_boundary(&mut messages);
        let len_after_first = messages.len();

        trim_to_compaction_boundary(&mut messages);
        assert_eq!(
            messages.len(),
            len_after_first,
            "second trim should be noop"
        );
    }

    #[test]
    fn extract_previous_summary_none_without_summary() {
        use std::sync::Arc;

        let messages = vec![Arc::new(Message::user("hello"))];
        assert!(extract_previous_summary(&messages).is_none());
    }

    // -----------------------------------------------------------------------
    // Truncation tests (ContextTransform)
    // -----------------------------------------------------------------------

    #[test]
    fn no_truncation_when_within_budget() {
        let transform = ContextTransform::new(make_policy(100_000, 2));
        let messages = vec![
            Message::system("system prompt"),
            Message::user("hello"),
            Message::assistant("hi there"),
            Message::user("how are you?"),
            Message::assistant("doing great"),
        ];
        let output = transform.transform(messages.clone(), &[]);
        assert_eq!(output.messages.len(), messages.len());
        for (a, b) in output.messages.iter().zip(messages.iter()) {
            assert_eq!(a.text(), b.text());
        }
    }

    #[test]
    fn truncation_drops_oldest_history() {
        // Use longer messages so token estimation is meaningful.
        // estimate_message_tokens: 4 + len/4.
        // "sys" → 4, each 40-char msg → 14 tokens. 6 history msgs → 84 tokens.
        // Budget 60 means system(4) + history budget 56 → fits ~4 msgs, drops oldest 2.
        let transform = ContextTransform::new(make_policy(60, 2));
        let filler = |tag: &str| format!("{tag}:{}", "x".repeat(40));
        let messages = vec![
            Message::system("sys"),
            Message::user(filler("old1")),
            Message::assistant(filler("old_reply1")),
            Message::user(filler("old2")),
            Message::assistant(filler("old_reply2")),
            Message::user(filler("recent1")),
            Message::assistant(filler("recent_reply1")),
        ];

        let output = transform.transform(messages, &[]);
        // System must be preserved
        assert_eq!(output.messages[0].role, Role::System);
        assert_eq!(output.messages[0].text(), "sys");
        // Oldest history should be dropped
        let texts: Vec<String> = output.messages.iter().map(|m| m.text()).collect();
        assert!(
            !texts.iter().any(|t| t.starts_with("old1:")),
            "oldest message should be dropped"
        );
        // Recent messages should be preserved
        assert!(
            texts.iter().any(|t| t.starts_with("recent_reply1:")),
            "most recent message should be preserved"
        );
    }

    #[test]
    fn min_recent_always_preserved() {
        // Very tight budget but min_recent = 4; should keep at least 4 history messages
        let transform = ContextTransform::new(make_policy(20, 4));
        let messages = vec![
            Message::system("s"),
            Message::user("a"),
            Message::assistant("b"),
            Message::user("c"),
            Message::assistant("d"),
            Message::user("e"),
            Message::assistant("f"),
        ];

        let output = transform.transform(messages, &[]);
        // System is always kept; history portion should have at least min_recent messages
        let history_count = output
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .count();
        assert!(
            history_count >= 4,
            "min_recent_messages=4 but only {history_count} history messages kept"
        );
    }

    #[test]
    fn system_messages_never_truncated() {
        // Multiple system messages at the start — all must survive truncation
        let transform = ContextTransform::new(make_policy(60, 1));
        let messages = vec![
            Message::system("system prompt 1"),
            Message::system("system prompt 2"),
            Message::system("system prompt 3"),
            Message::user("old1"),
            Message::assistant("old_reply1"),
            Message::user("old2"),
            Message::assistant("old_reply2"),
            Message::user("recent"),
            Message::assistant("recent_reply"),
        ];

        let output = transform.transform(messages, &[]);
        let system_msgs: Vec<&Message> = output
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .collect();
        assert_eq!(
            system_msgs.len(),
            3,
            "all system messages must be preserved"
        );
        assert_eq!(system_msgs[0].text(), "system prompt 1");
        assert_eq!(system_msgs[1].text(), "system prompt 2");
        assert_eq!(system_msgs[2].text(), "system prompt 3");
    }

    // -----------------------------------------------------------------------
    // Compaction boundary tests
    // -----------------------------------------------------------------------

    #[test]
    fn find_boundary_skips_open_tool_rounds() {
        use std::sync::Arc;

        let messages: Vec<Arc<Message>> = vec![
            Arc::new(Message::user("start")),
            Arc::new(Message::assistant("ok")),
            Arc::new(Message::user("do something")),
            Arc::new(Message::assistant_with_tool_calls(
                "",
                vec![ToolCall::new("c1", "search", json!({}))],
            )),
            // c1 result is missing — open tool round
        ];

        let boundary = find_compaction_boundary(&messages, 0, messages.len());
        // Must not place boundary at or after the open tool call (idx 3)
        match boundary {
            Some(b) => assert!(b < 3, "boundary {b} must be before open tool call at idx 3"),
            None => {} // also acceptable if no safe boundary exists
        }
    }

    #[test]
    fn find_boundary_respects_suffix_messages() {
        use std::sync::Arc;

        // Search only within a sub-range, leaving suffix messages untouched
        let messages: Vec<Arc<Message>> = vec![
            Arc::new(Message::user("old1")),
            Arc::new(Message::assistant("reply1")),
            Arc::new(Message::user("old2")),
            Arc::new(Message::assistant("reply2")),
            // suffix: last 2 messages are "raw suffix"
            Arc::new(Message::user("recent")),
            Arc::new(Message::assistant("recent_reply")),
        ];

        let suffix_count = 2;
        let search_end = messages.len().saturating_sub(suffix_count);
        let boundary = find_compaction_boundary(&messages, 0, search_end);
        // Boundary must be within the searched range, not touching suffix
        if let Some(b) = boundary {
            assert!(
                b < search_end,
                "boundary {b} must be before suffix start {search_end}"
            );
        }
    }

    #[test]
    fn find_boundary_returns_none_when_too_few_messages() {
        use std::sync::Arc;

        // Single message — no safe compaction point
        let messages: Vec<Arc<Message>> = vec![Arc::new(Message::user("only message"))];
        // Search range is empty (start == end)
        let boundary = find_compaction_boundary(&messages, 0, 0);
        assert!(boundary.is_none(), "empty range should yield no boundary");

        // Range with only an open tool call — no safe boundary
        let messages2: Vec<Arc<Message>> = vec![Arc::new(Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new("c1", "fn", json!({}))],
        ))];
        let boundary2 = find_compaction_boundary(&messages2, 0, messages2.len());
        assert!(
            boundary2.is_none(),
            "single open tool call should yield no boundary"
        );
    }

    // -----------------------------------------------------------------------
    // Render transcript tests
    // -----------------------------------------------------------------------

    #[test]
    fn render_transcript_filters_internal_messages() {
        use std::sync::Arc;

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
        use std::sync::Arc;

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

    // -----------------------------------------------------------------------
    // Artifact compaction tests
    // -----------------------------------------------------------------------

    #[test]
    fn small_tool_result_not_compacted() {
        let small_content = "x".repeat(100);
        let mut messages = vec![
            Message::user("go"),
            Message::assistant_with_tool_calls("", vec![ToolCall::new("c1", "search", json!({}))]),
            Message::tool("c1", &small_content),
        ];
        compact_tool_results(&mut messages);
        assert_eq!(messages[2].text(), small_content);
    }

    #[test]
    fn large_tool_result_compacted_to_preview() {
        // 2048 tokens * 4 chars/token = 8192 chars needed to exceed threshold
        let large_content = "a".repeat(10_000);
        let mut messages = vec![
            Message::user("go"),
            Message::assistant_with_tool_calls(
                "",
                vec![ToolCall::new("c1", "list_files", json!({}))],
            ),
            Message::tool("c1", &large_content),
        ];
        compact_tool_results(&mut messages);

        let result = messages[2].text();
        assert!(
            result.len() < large_content.len(),
            "content should be shorter after compaction"
        );
        assert!(
            result.contains("[Content compacted:"),
            "should contain compaction indicator"
        );
        assert!(result.contains("tokens"), "indicator should mention tokens");
        assert!(result.contains("chars"), "indicator should mention chars");
    }

    #[test]
    fn compact_preserves_non_tool_messages() {
        let large_text = "b".repeat(10_000);
        let mut messages = vec![
            Message::system(&large_text),
            Message::user(&large_text),
            Message::assistant(&large_text),
        ];
        let texts_before: Vec<String> = messages.iter().map(|m| m.text()).collect();
        compact_tool_results(&mut messages);
        let texts_after: Vec<String> = messages.iter().map(|m| m.text()).collect();
        assert_eq!(
            texts_before, texts_after,
            "non-tool messages should be unchanged"
        );
    }
}
