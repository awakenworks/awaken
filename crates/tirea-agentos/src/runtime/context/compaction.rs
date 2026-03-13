use async_trait::async_trait;
use genai::chat::{ChatMessage, ChatOptions, ChatRequest};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

use tirea_contract::runtime::inference::ContextCompactionMode;
use tirea_contract::runtime::tool_call::{SuspendedCall, ToolCallState, ToolResult};
use tirea_contract::thread::{Message, Role, Thread};
use tirea_state::State;

use crate::engine::token_estimator::estimate_message_tokens;
use crate::runtime::loop_runner::LlmExecutor;

use super::state::ContextState;
use super::transform::is_pending_approval_notice;
use super::{SUMMARY_MESSAGE_OPEN, SUMMARY_MESSAGE_CLOSE};

const SUMMARY_SYSTEM_PROMPT: &str = "You maintain a durable conversation summary for an agent runtime. Produce a concise but lossless working summary for future turns. Preserve user goals, constraints, preferences, decisions, completed work, important tool findings, file paths, identifiers, and unresolved follow-ups. Output plain text only; do not mention the summarization process.";
const SUMMARY_RESPONSE_MAX_TOKENS: u32 = 1024;
pub(super) const MIN_COMPACTION_GAIN_TOKENS: usize = 1024;
pub(super) const DEFAULT_ARTIFACT_COMPACT_THRESHOLD_TOKENS: usize = 2048;
const ARTIFACT_PREVIEW_MAX_CHARS: usize = 1600;
const ARTIFACT_PREVIEW_MAX_LINES: usize = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CompactionPlan {
    pub(super) start_index: usize,
    pub(super) boundary_index: usize,
    pub(super) boundary_message_id: String,
    pub(super) covered_token_count: usize,
    pub(super) covered_message_ids: Vec<String>,
    pub(super) covered_tool_call_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub(super) struct SummaryPayload {
    pub(super) previous_summary: Option<String>,
    pub(super) transcript: String,
}

#[derive(Debug, Error)]
pub(super) enum ContextError {
    #[error("summary model returned no text")]
    EmptySummary,
    #[error("failed to execute summary request: {0}")]
    SummaryExecution(#[from] genai::Error),
}

#[async_trait]
pub(super) trait ContextSummarizer: Send + Sync {
    async fn summarize(&self, payload: SummaryPayload) -> Result<String, ContextError>;
}

#[derive(Clone)]
pub(super) struct LlmContextSummarizer {
    model: String,
    executor: Arc<dyn LlmExecutor>,
    chat_options: Option<ChatOptions>,
}

impl LlmContextSummarizer {
    pub(super) fn new(
        model: String,
        executor: Arc<dyn LlmExecutor>,
        chat_options: Option<ChatOptions>,
    ) -> Self {
        Self {
            model,
            executor,
            chat_options,
        }
    }
}

#[async_trait]
impl ContextSummarizer for LlmContextSummarizer {
    async fn summarize(&self, payload: SummaryPayload) -> Result<String, ContextError> {
        let prompt = render_summary_prompt(&payload);
        let request = ChatRequest::new(vec![
            ChatMessage::system(SUMMARY_SYSTEM_PROMPT),
            ChatMessage::user(prompt),
        ]);

        let mut options = self.chat_options.clone().unwrap_or_default();
        options = options
            .with_capture_usage(true)
            .with_max_tokens(SUMMARY_RESPONSE_MAX_TOKENS);

        let response = self
            .executor
            .exec_chat_response(&self.model, request, Some(&options))
            .await?;
        let summary = response
            .first_text()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or(ContextError::EmptySummary)?;
        Ok(summary.to_string())
    }
}

fn render_summary_prompt(payload: &SummaryPayload) -> String {
    match payload.previous_summary.as_deref() {
        Some(previous) if !previous.trim().is_empty() => format!(
            "Update the cumulative summary with the new conversation span. Preserve all still-relevant facts from the existing summary, then merge in the new span.\n\n<existing-summary>\n{}\n</existing-summary>\n\n<new-conversation>\n{}\n</new-conversation>",
            previous.trim(),
            payload.transcript.trim(),
        ),
        _ => format!(
            "Summarize the following conversation span for continued execution.\n\n<conversation>\n{}\n</conversation>",
            payload.transcript.trim(),
        ),
    }
}

pub(super) fn estimate_arc_messages_tokens(messages: &[Arc<Message>]) -> usize {
    messages
        .iter()
        .map(|message| estimate_message_tokens(message))
        .sum()
}

pub(super) fn unsummarized_start_index(
    messages: &[Arc<Message>],
    state: &ContextState,
) -> Option<usize> {
    let Some(boundary) = state.latest_boundary() else {
        return Some(0);
    };

    Some(
        messages
            .iter()
            .position(|message| {
                message.id.as_deref() == Some(boundary.covers_through_message_id.as_str())
            })
            .map(|idx| idx + 1)
            .unwrap_or(0),
    )
}

fn protected_tail_start(
    messages: &[Arc<Message>],
    start_index: usize,
    raw_suffix_messages: usize,
) -> usize {
    if start_index >= messages.len() {
        return messages.len();
    }

    let min_tail_start = messages.len().saturating_sub(raw_suffix_messages);
    let last_user_index = messages
        .iter()
        .enumerate()
        .skip(start_index)
        .filter(|(_, message)| message.role == Role::User)
        .map(|(idx, _)| idx)
        .last();

    let candidate = last_user_index.map_or(min_tail_start, |idx| idx.min(min_tail_start));
    candidate.max(start_index)
}

fn compaction_search_end(
    messages: &[Arc<Message>],
    start_index: usize,
    mode: ContextCompactionMode,
    raw_suffix_messages: usize,
) -> usize {
    match mode {
        ContextCompactionMode::KeepRecentRawSuffix => {
            protected_tail_start(messages, start_index, raw_suffix_messages)
        }
        ContextCompactionMode::CompactToSafeFrontier => messages.len(),
    }
}

pub(super) fn find_compaction_plan(
    messages: &[Arc<Message>],
    state: &ContextState,
    tool_states: &HashMap<String, ToolCallState>,
    suspended_calls: &HashMap<String, SuspendedCall>,
    mode: ContextCompactionMode,
    raw_suffix_messages: usize,
) -> Option<CompactionPlan> {
    let start_index = unsummarized_start_index(messages, state)?;
    let search_end = compaction_search_end(messages, start_index, mode, raw_suffix_messages);
    if search_end <= start_index {
        return None;
    }

    let mut open_calls = HashSet::<String>::new();
    let mut best_boundary = None;

    for (idx, message) in messages
        .iter()
        .enumerate()
        .skip(start_index)
        .take(search_end - start_index)
    {
        if let Some(calls) = &message.tool_calls {
            for call in calls {
                open_calls.insert(call.id.clone());
            }
        }

        if message.role == Role::Tool {
            if let Some(call_id) = message.tool_call_id.as_deref() {
                if open_calls.contains(call_id)
                    && is_terminal_tool_result(call_id, message, tool_states, suspended_calls)
                {
                    open_calls.remove(call_id);
                }
            }
        }

        let next_is_tool = messages
            .get(idx + 1)
            .is_some_and(|next| next.role == Role::Tool);
        if !open_calls.is_empty() || next_is_tool {
            continue;
        }

        if let Some(boundary_message_id) = message.id.clone() {
            best_boundary = Some((idx, boundary_message_id));
        }
    }

    let (boundary_index, boundary_message_id) = best_boundary?;
    let covered_slice = &messages[start_index..=boundary_index];
    let covered_token_count = estimate_arc_messages_tokens(covered_slice);

    let covered_message_ids = covered_slice
        .iter()
        .filter_map(|message| message.id.clone())
        .collect();
    let covered_tool_call_ids = covered_slice
        .iter()
        .filter_map(|message| message.tool_call_id.clone())
        .collect();

    Some(CompactionPlan {
        start_index,
        boundary_index,
        boundary_message_id,
        covered_token_count,
        covered_message_ids,
        covered_tool_call_ids,
    })
}

fn is_terminal_tool_result(
    call_id: &str,
    message: &Message,
    tool_states: &HashMap<String, ToolCallState>,
    suspended_calls: &HashMap<String, SuspendedCall>,
) -> bool {
    if is_pending_approval_notice(message) || suspended_calls.contains_key(call_id) {
        return false;
    }

    tool_states
        .get(call_id)
        .map(|state| state.status.is_terminal())
        .unwrap_or(true)
}

pub(super) fn render_messages_for_summary(messages: &[Message]) -> String {
    messages
        .iter()
        .map(render_message_for_summary)
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_message_for_summary(message: &Message) -> String {
    match message.role {
        Role::System => format!("[system]\n{}", message.content.trim()),
        Role::User => format!("[user]\n{}", message.content.trim()),
        Role::Assistant => {
            let mut body = String::new();
            if !message.content.trim().is_empty() {
                body.push_str(message.content.trim());
            }
            if let Some(tool_calls) = &message.tool_calls {
                if !body.is_empty() {
                    body.push_str("\n\n");
                }
                body.push_str("tool_calls:\n");
                for call in tool_calls {
                    body.push_str("- ");
                    body.push_str(&call.name);
                    body.push_str(" ");
                    body.push_str(&call.arguments.to_string());
                    body.push('\n');
                }
                body.truncate(body.trim_end_matches('\n').len());
            }
            format!("[assistant]\n{}", body.trim())
        }
        Role::Tool => format!(
            "[tool:{}]\n{}",
            message.tool_call_id.as_deref().unwrap_or("unknown"),
            message.content.trim()
        ),
    }
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn truncate_preview(text: &str, max_chars: usize, max_lines: usize) -> String {
    let mut out = String::new();
    for (line_idx, line) in text.lines().enumerate() {
        if line_idx >= max_lines {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        if out.chars().count() >= max_chars {
            break;
        }
    }

    let mut chars = out.chars();
    let mut truncated = String::new();
    for _ in 0..max_chars {
        let Some(ch) = chars.next() else {
            return out.trim().to_string();
        };
        truncated.push(ch);
    }
    if chars.next().is_some() || text.lines().count() > max_lines {
        truncated.push_str("\n…");
    }
    truncated.trim().to_string()
}

pub(super) fn build_artifact_preview(result: &ToolResult) -> String {
    let mut sections = Vec::new();
    sections.push(format!("tool: {}", result.tool_name));
    sections.push(format!("status: {:?}", result.status).to_lowercase());

    if let Some(message) = result
        .message
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        sections.push(format!("message: {message}"));
    }

    if !result.data.is_null() {
        let data_text =
            serde_json::to_string_pretty(&result.data).unwrap_or_else(|_| result.data.to_string());
        let preview = truncate_preview(
            &data_text,
            ARTIFACT_PREVIEW_MAX_CHARS,
            ARTIFACT_PREVIEW_MAX_LINES,
        );
        if !preview.is_empty() {
            sections.push(format!("data preview:\n{preview}"));
        }
    }

    sections.join("\n")
}

// ---------------------------------------------------------------------------
// Lazy context loading
// ---------------------------------------------------------------------------

/// Trim pre-boundary messages from a thread at load time.
///
/// Reads [`ContextState`] from the thread's persisted state. If a
/// compaction boundary exists, replaces all messages up to (and including) the
/// boundary message with a single summary message. This avoids carrying
/// thousands of `Arc<Message>` references through `RunContext` that would be
/// replaced by `ContextTransform` at inference time anyway.
///
/// Safe to call multiple times (idempotent): the second call finds no boundary
/// message and returns without changes.
pub(crate) fn trim_thread_to_latest_boundary(thread: &mut Thread) {
    let state = match thread.rebuild_state() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "skipping lazy trim: state rebuild failed");
            return;
        }
    };

    let ctx_value = match state.get(<ContextState as State>::PATH) {
        Some(v) => v,
        None => return,
    };

    let cm_state: ContextState = match serde_json::from_value(ctx_value.clone()) {
        Ok(s) => s,
        Err(_) => return,
    };

    let Some(boundary) = cm_state.latest_boundary() else {
        return;
    };

    let Some(idx) = thread.messages.iter().position(|m| {
        m.id.as_deref() == Some(boundary.covers_through_message_id.as_str())
    }) else {
        return;
    };

    let summary = Message::internal_system(format!(
        "{SUMMARY_MESSAGE_OPEN}\n{}\n{SUMMARY_MESSAGE_CLOSE}",
        boundary.summary
    ));
    thread.messages.drain(..=idx);
    thread.messages.insert(0, Arc::new(summary));
}
