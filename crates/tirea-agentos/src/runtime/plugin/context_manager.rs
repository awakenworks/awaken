//! Context manager: logical compression of conversation history.
//!
//! Maintains [`ContextManagerState`] with compact boundaries and artifact
//! references. Registers a [`ContextAssemblyTransform`] that replaces
//! pre-boundary messages with a summary and swaps large artifact content
//! with compact views — without modifying the persisted thread.

use async_trait::async_trait;
use genai::chat::{ChatMessage, ChatOptions, ChatRequest};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tirea_contract::runtime::behavior::ReadOnlyContext;
use tirea_contract::runtime::inference::{
    ContextCompactionMode, ContextWindowPolicy, InferenceRequestTransform, InferenceTransformOutput,
};
use tirea_contract::runtime::phase::{ActionSet, AfterToolExecuteAction, BeforeInferenceAction};
use tirea_contract::runtime::state::{AnyStateAction, StateScope};
use tirea_contract::runtime::tool_call::{
    suspended_calls_from_state, tool_call_states_from_state, SuspendedCall, ToolCallState,
    ToolDescriptor, ToolResult,
};
use tirea_contract::thread::{Message, Role, Thread};
use tirea_state::State;

use crate::engine::token_estimator::{
    estimate_message_tokens, estimate_messages_tokens, estimate_tokens,
};
use crate::runtime::loop_runner::LlmExecutor;

/// Behavior ID used for registration.
pub const CONTEXT_MANAGER_PLUGIN_ID: &str = "context_manager";

const SUMMARY_MESSAGE_OPEN: &str = "<conversation-summary>";
const SUMMARY_MESSAGE_CLOSE: &str = "</conversation-summary>";
const SUMMARY_RESPONSE_MAX_TOKENS: u32 = 1024;
const MIN_COMPACTION_GAIN_TOKENS: usize = 1024;
const DEFAULT_ARTIFACT_COMPACT_THRESHOLD_TOKENS: usize = 2048;
const ARTIFACT_PREVIEW_MAX_CHARS: usize = 1600;
const ARTIFACT_PREVIEW_MAX_LINES: usize = 24;

const PENDING_APPROVAL_NOTICE_PREFIX: &str = "Tool '";
const PENDING_APPROVAL_NOTICE_SUFFIX: &str = "' is awaiting approval. Execution paused.";
const INTERRUPTED_TOOL_RESULT_NOTICE: &str =
    "[Tool execution was interrupted before producing a result.]";

const SUMMARY_SYSTEM_PROMPT: &str = "You maintain a durable conversation summary for an agent runtime. Produce a concise but lossless working summary for future turns. Preserve user goals, constraints, preferences, decisions, completed work, important tool findings, file paths, identifiers, and unresolved follow-ups. Output plain text only; do not mention the summarization process.";

// ---------------------------------------------------------------------------
// State types
// ---------------------------------------------------------------------------

/// A compact boundary marking that all messages up through a given message ID
/// have been logically replaced by a summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactBoundary {
    /// ID of the last message covered by this boundary.
    pub covers_through_message_id: String,
    /// Pre-computed summary text that replaces the covered messages.
    pub summary: String,
    /// Estimated token count of the original messages that were summarized.
    pub original_token_count: usize,
    /// Timestamp (ms since epoch) when this boundary was created.
    pub created_at_ms: u64,
}

/// A reference to a large artifact whose content is replaced with a
/// lightweight compact view during inference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// Message ID containing the artifact, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Tool call ID if the artifact is a tool result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Human-readable label for the compact view.
    pub label: String,
    /// Compact preview shown to the model during inference.
    pub summary: String,
    /// Original content size in characters.
    pub original_size: usize,
    /// Estimated original token count.
    pub original_token_count: usize,
}

/// Thread-scoped state persisting compact boundaries and artifact references.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(path = "__context", action = "ContextManagerAction", scope = "thread")]
pub struct ContextManagerState {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub boundaries: Vec<CompactBoundary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_refs: Vec<ArtifactRef>,
}

/// Actions that modify [`ContextManagerState`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContextManagerAction {
    /// Add a new compact boundary. Boundaries are cumulative; the latest one
    /// supersedes any earlier boundary.
    AddBoundary(CompactBoundary),
    /// Register an artifact reference for compact-view substitution.
    AddArtifact(ArtifactRef),
    /// Remove artifact refs that are fully covered by compaction.
    PruneArtifacts {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        message_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_call_ids: Vec<String>,
    },
}

impl ContextManagerState {
    fn reduce(&mut self, action: ContextManagerAction) {
        match action {
            ContextManagerAction::AddBoundary(boundary) => {
                self.boundaries.clear();
                self.boundaries.push(boundary);
            }
            ContextManagerAction::AddArtifact(artifact) => {
                if let Some(existing) = self
                    .artifact_refs
                    .iter_mut()
                    .find(|existing| artifact_identity_matches(existing, &artifact))
                {
                    *existing = artifact;
                } else {
                    self.artifact_refs.push(artifact);
                }
            }
            ContextManagerAction::PruneArtifacts {
                message_ids,
                tool_call_ids,
            } => {
                if message_ids.is_empty() && tool_call_ids.is_empty() {
                    return;
                }
                let message_ids: HashSet<String> = message_ids.into_iter().collect();
                let tool_call_ids: HashSet<String> = tool_call_ids.into_iter().collect();
                self.artifact_refs.retain(|artifact| {
                    let message_match = artifact
                        .message_id
                        .as_ref()
                        .is_some_and(|id| message_ids.contains(id));
                    let tool_match = artifact
                        .tool_call_id
                        .as_ref()
                        .is_some_and(|id| tool_call_ids.contains(id));
                    !(message_match || tool_match)
                });
            }
        }
    }

    fn latest_boundary(&self) -> Option<&CompactBoundary> {
        self.boundaries.last()
    }
}

fn artifact_identity_matches(existing: &ArtifactRef, candidate: &ArtifactRef) -> bool {
    existing.message_id.is_some() && existing.message_id == candidate.message_id
        || existing.tool_call_id.is_some() && existing.tool_call_id == candidate.tool_call_id
}

// ---------------------------------------------------------------------------
// Transform
// ---------------------------------------------------------------------------

/// Inference request transform that applies logical compression.
///
/// Constructed each step in `before_inference` with a snapshot of the current
/// [`ContextManagerState`]. The transform:
/// 1. Replaces messages before the latest boundary with a summary.
/// 2. Repairs tool/result pairing after replacement.
/// 3. Substitutes large artifact content with compact views.
struct ContextAssemblyTransform {
    state: ContextManagerState,
    artifact_by_message_id: HashMap<String, ArtifactRef>,
    artifact_by_tool_call_id: HashMap<String, ArtifactRef>,
}

impl ContextAssemblyTransform {
    fn new(state: ContextManagerState) -> Self {
        let mut artifact_by_message_id = HashMap::new();
        let mut artifact_by_tool_call_id = HashMap::new();
        for artifact in &state.artifact_refs {
            if let Some(message_id) = &artifact.message_id {
                artifact_by_message_id.insert(message_id.clone(), artifact.clone());
            }
            if let Some(tool_call_id) = &artifact.tool_call_id {
                artifact_by_tool_call_id.insert(tool_call_id.clone(), artifact.clone());
            }
        }
        Self {
            state,
            artifact_by_message_id,
            artifact_by_tool_call_id,
        }
    }

    /// Replace messages before the latest boundary with a summary message.
    fn apply_boundaries(&self, messages: Vec<Message>) -> Vec<Message> {
        let Some(boundary) = self.state.latest_boundary() else {
            return messages;
        };

        let system_end = messages
            .iter()
            .position(|m| m.role != Role::System)
            .unwrap_or(messages.len());
        let (leading_system, history) = messages.split_at(system_end);

        let Some(boundary_idx) = history
            .iter()
            .position(|m| m.id.as_deref() == Some(boundary.covers_through_message_id.as_str()))
        else {
            // Boundary message not found (unexpected external mutation). Pass
            // through unchanged instead of corrupting history.
            return messages;
        };

        let mut out = Vec::with_capacity(
            leading_system.len() + 1 + history.len().saturating_sub(boundary_idx + 1),
        );
        out.extend(leading_system.iter().cloned());
        out.push(Message::internal_system(format!(
            "{SUMMARY_MESSAGE_OPEN}\n{}\n{SUMMARY_MESSAGE_CLOSE}",
            boundary.summary
        )));
        out.extend(history[boundary_idx + 1..].iter().cloned());

        repair_tool_pairing(out)
    }

    /// Replace content of messages referenced by artifact refs.
    fn apply_artifact_refs(&self, messages: &mut [Message]) {
        if self.state.artifact_refs.is_empty() {
            return;
        }

        for msg in messages.iter_mut() {
            let artifact = msg
                .id
                .as_ref()
                .and_then(|id| self.artifact_by_message_id.get(id))
                .or_else(|| {
                    msg.tool_call_id
                        .as_ref()
                        .and_then(|id| self.artifact_by_tool_call_id.get(id))
                });

            if let Some(artifact) = artifact {
                msg.content = render_artifact_compact_view(artifact);
            }
        }
    }
}

impl InferenceRequestTransform for ContextAssemblyTransform {
    fn transform(
        &self,
        messages: Vec<Message>,
        _tool_descriptors: &[ToolDescriptor],
    ) -> InferenceTransformOutput {
        let mut result = self.apply_boundaries(messages);
        self.apply_artifact_refs(&mut result);
        InferenceTransformOutput {
            messages: result,
            enable_prompt_cache: false,
        }
    }
}

fn render_artifact_compact_view(artifact: &ArtifactRef) -> String {
    let summary = artifact.summary.trim();
    if summary.is_empty() {
        format!(
            "[Artifact compacted: {}]\n[Original size: {} chars / ~{} tokens. Re-run the producing tool or inspect the persisted thread for full content.]",
            artifact.label, artifact.original_size, artifact.original_token_count,
        )
    } else {
        format!(
            "[Artifact compacted: {}]\n{}\n[Original size: {} chars / ~{} tokens. Re-run the producing tool or inspect the persisted thread for full content.]",
            artifact.label, summary, artifact.original_size, artifact.original_token_count,
        )
    }
}

fn repair_tool_pairing(messages: Vec<Message>) -> Vec<Message> {
    let system_end = messages
        .iter()
        .position(|m| m.role != Role::System)
        .unwrap_or(messages.len());
    let mut repaired = messages[..system_end].to_vec();
    let mut history = filter_orphaned_tool_results(&messages[system_end..]);
    patch_dangling_tool_calls(&mut history);
    repaired.extend(history);
    repaired
}

fn filter_orphaned_tool_results(history: &[Message]) -> Vec<Message> {
    let known_tool_call_ids: HashSet<&str> = history
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
        .map(|tc| tc.id.as_str())
        .collect();

    let mut pending_notice_ids = HashSet::new();
    let mut resolved_result_ids = HashSet::new();
    for msg in history {
        let Some(tc_id) = msg.tool_call_id.as_deref() else {
            continue;
        };
        if !known_tool_call_ids.contains(tc_id) {
            continue;
        }
        if is_pending_approval_notice(msg) {
            pending_notice_ids.insert(tc_id.to_string());
        } else if msg.role == Role::Tool {
            resolved_result_ids.insert(tc_id.to_string());
        }
    }
    let superseded_pending_notice_ids: HashSet<String> = pending_notice_ids
        .intersection(&resolved_result_ids)
        .cloned()
        .collect();

    history
        .iter()
        .filter(|msg| {
            if msg.role != Role::Tool {
                return true;
            }
            let Some(tc_id) = msg.tool_call_id.as_deref() else {
                return false;
            };
            if !known_tool_call_ids.contains(tc_id) {
                return false;
            }
            !(superseded_pending_notice_ids.contains(tc_id) && is_pending_approval_notice(msg))
        })
        .cloned()
        .collect()
}

fn patch_dangling_tool_calls(messages: &mut Vec<Message>) {
    let result_ids: HashSet<String> = messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .collect();

    let mut insertions: Vec<(usize, Vec<Message>)> = Vec::new();
    let mut i = 0usize;
    while i < messages.len() {
        let msg = &messages[i];
        if msg.role == Role::Assistant {
            if let Some(ref calls) = msg.tool_calls {
                let missing: Vec<&str> = calls
                    .iter()
                    .map(|tc| tc.id.as_str())
                    .filter(|id| !result_ids.contains(*id))
                    .collect();

                if !missing.is_empty() {
                    let mut insert_at = i + 1;
                    while insert_at < messages.len() && messages[insert_at].role == Role::Tool {
                        insert_at += 1;
                    }
                    let synthetic = missing
                        .into_iter()
                        .map(|id| Message::tool(id, INTERRUPTED_TOOL_RESULT_NOTICE))
                        .collect();
                    insertions.push((insert_at, synthetic));
                }
            }
        }
        i += 1;
    }

    for (idx, msgs) in insertions.into_iter().rev() {
        let idx = idx.min(messages.len());
        for (offset, msg) in msgs.into_iter().enumerate() {
            messages.insert(idx + offset, msg);
        }
    }
}

fn is_pending_approval_notice_content(content: &str) -> bool {
    content
        .strip_prefix(PENDING_APPROVAL_NOTICE_PREFIX)
        .and_then(|rest| rest.strip_suffix(PENDING_APPROVAL_NOTICE_SUFFIX))
        .is_some_and(|tool_name| !tool_name.is_empty())
}

fn is_pending_approval_notice(msg: &Message) -> bool {
    msg.role == Role::Tool && is_pending_approval_notice_content(&msg.content)
}

// ---------------------------------------------------------------------------
// Compaction planner + summarizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompactionPlan {
    start_index: usize,
    boundary_index: usize,
    boundary_message_id: String,
    covered_token_count: usize,
    covered_message_ids: Vec<String>,
    covered_tool_call_ids: Vec<String>,
}

#[derive(Debug, Clone)]
struct SummaryPayload {
    previous_summary: Option<String>,
    transcript: String,
}

#[derive(Debug, Error)]
enum ContextManagerError {
    #[error("summary model returned no text")]
    EmptySummary,
    #[error("failed to execute summary request: {0}")]
    SummaryExecution(#[from] genai::Error),
}

#[async_trait]
trait ContextSummarizer: Send + Sync {
    async fn summarize(&self, payload: SummaryPayload) -> Result<String, ContextManagerError>;
}

#[derive(Clone)]
struct LlmContextSummarizer {
    model: String,
    executor: Arc<dyn LlmExecutor>,
    chat_options: Option<ChatOptions>,
}

impl LlmContextSummarizer {
    fn new(
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
    async fn summarize(&self, payload: SummaryPayload) -> Result<String, ContextManagerError> {
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
            .ok_or(ContextManagerError::EmptySummary)?;
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

fn estimate_arc_messages_tokens(messages: &[Arc<Message>]) -> usize {
    messages
        .iter()
        .map(|message| estimate_message_tokens(message))
        .sum()
}

fn unsummarized_start_index(
    messages: &[Arc<Message>],
    state: &ContextManagerState,
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

fn find_compaction_plan(
    messages: &[Arc<Message>],
    state: &ContextManagerState,
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

fn render_messages_for_summary(messages: &[Message]) -> String {
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

fn now_ms() -> u64 {
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

fn build_artifact_preview(result: &ToolResult) -> String {
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
/// Reads [`ContextManagerState`] from the thread's persisted state. If a
/// compaction boundary exists, replaces all messages up to (and including) the
/// boundary message with a single summary message. This avoids carrying
/// thousands of `Arc<Message>` references through `RunContext` that would be
/// replaced by `ContextAssemblyTransform` at inference time anyway.
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

    let ctx_value = match state.get(<ContextManagerState as State>::PATH) {
        Some(v) => v,
        None => return,
    };

    let cm_state: ContextManagerState = match serde_json::from_value(ctx_value.clone()) {
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

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

/// Behavior that registers context assembly transforms and performs auto-compaction.
#[derive(Clone)]
pub struct ContextManagerPlugin {
    policy: ContextWindowPolicy,
    artifact_compact_threshold_tokens: usize,
    summarizer: Option<Arc<dyn ContextSummarizer>>,
}

impl std::fmt::Debug for ContextManagerPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextManagerPlugin")
            .field("policy", &self.policy)
            .field(
                "artifact_compact_threshold_tokens",
                &self.artifact_compact_threshold_tokens,
            )
            .field("has_summarizer", &self.summarizer.is_some())
            .finish()
    }
}

impl Default for ContextManagerPlugin {
    fn default() -> Self {
        Self::new(ContextWindowPolicy::default())
    }
}

impl ContextManagerPlugin {
    pub fn new(policy: ContextWindowPolicy) -> Self {
        Self {
            policy,
            artifact_compact_threshold_tokens: DEFAULT_ARTIFACT_COMPACT_THRESHOLD_TOKENS,
            summarizer: None,
        }
    }

    pub fn with_artifact_compact_threshold_tokens(mut self, threshold: usize) -> Self {
        self.artifact_compact_threshold_tokens = threshold;
        self
    }

    #[cfg(test)]
    fn with_summarizer(mut self, summarizer: Arc<dyn ContextSummarizer>) -> Self {
        self.summarizer = Some(summarizer);
        self
    }

    pub(crate) fn with_llm_summarizer(
        mut self,
        model: String,
        executor: Arc<dyn LlmExecutor>,
        chat_options: Option<ChatOptions>,
    ) -> Self {
        self.summarizer = Some(Arc::new(LlmContextSummarizer::new(
            model,
            executor,
            chat_options,
        )));
        self
    }

    async fn maybe_compact(
        &self,
        ctx: &ReadOnlyContext<'_>,
        state: &ContextManagerState,
    ) -> Option<(CompactBoundary, Option<ContextManagerAction>)> {
        let threshold = self.policy.autocompact_threshold?;
        let raw_messages: Vec<Message> = ctx
            .messages()
            .iter()
            .map(|message| (**message).clone())
            .collect();
        let effective_messages = ContextAssemblyTransform::new(state.clone())
            .transform(raw_messages, &[])
            .messages;
        let effective_tokens = estimate_messages_tokens(&effective_messages);
        if effective_tokens < threshold {
            return None;
        }

        let snapshot = ctx.snapshot();
        let tool_states = tool_call_states_from_state(&snapshot);
        let suspended_calls = suspended_calls_from_state(&snapshot);
        let plan = find_compaction_plan(
            ctx.messages(),
            state,
            &tool_states,
            &suspended_calls,
            self.policy.compaction_mode,
            self.policy.compaction_raw_suffix_messages,
        )?;
        if plan.covered_token_count < MIN_COMPACTION_GAIN_TOKENS {
            return None;
        }
        let summarizer = self.summarizer.as_ref()?;

        let mut delta_messages: Vec<Message> = ctx.messages()
            [plan.start_index..=plan.boundary_index]
            .iter()
            .map(|message| (**message).clone())
            .collect();
        ContextAssemblyTransform::new(state.clone()).apply_artifact_refs(&mut delta_messages);

        let payload = SummaryPayload {
            previous_summary: state
                .latest_boundary()
                .map(|boundary| boundary.summary.clone()),
            transcript: render_messages_for_summary(&delta_messages),
        };
        let summary = match summarizer.summarize(payload).await {
            Ok(summary) => summary,
            Err(error) => {
                tracing::warn!(
                    thread_id = %ctx.thread_id(),
                    error = %error,
                    "context compaction skipped after summary generation failed"
                );
                return None;
            }
        };

        let boundary = CompactBoundary {
            covers_through_message_id: plan.boundary_message_id.clone(),
            summary,
            original_token_count: state
                .latest_boundary()
                .map(|boundary| boundary.original_token_count)
                .unwrap_or(0)
                + plan.covered_token_count,
            created_at_ms: now_ms(),
        };

        let prune_action =
            if plan.covered_message_ids.is_empty() && plan.covered_tool_call_ids.is_empty() {
                None
            } else {
                Some(ContextManagerAction::PruneArtifacts {
                    message_ids: plan.covered_message_ids,
                    tool_call_ids: plan.covered_tool_call_ids,
                })
            };

        Some((boundary, prune_action))
    }

    fn maybe_build_artifact_ref(&self, call_id: &str, result: &ToolResult) -> Option<ArtifactRef> {
        let raw_content = serde_json::to_string(result).unwrap_or_else(|_| {
            result
                .message
                .clone()
                .unwrap_or_else(|| result.data.to_string())
        });
        let token_count = estimate_tokens(&raw_content);
        if token_count < self.artifact_compact_threshold_tokens {
            return None;
        }

        Some(ArtifactRef {
            message_id: None,
            tool_call_id: Some(call_id.to_string()),
            label: result.tool_name.clone(),
            summary: build_artifact_preview(result),
            original_size: raw_content.len(),
            original_token_count: token_count,
        })
    }
}

#[async_trait]
impl tirea_contract::runtime::AgentBehavior for ContextManagerPlugin {
    fn id(&self) -> &str {
        CONTEXT_MANAGER_PLUGIN_ID
    }

    tirea_contract::declare_plugin_states!(ContextManagerState);

    async fn before_inference(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        let state = ctx
            .scoped_state_of::<ContextManagerState>(StateScope::Thread)
            .ok()
            .unwrap_or_default();

        let mut effective_state = state.clone();
        let mut actions = ActionSet::empty();

        if let Some((boundary, prune_action)) = self.maybe_compact(ctx, &state).await {
            let boundary_action = ContextManagerAction::AddBoundary(boundary.clone());
            effective_state.reduce(boundary_action.clone());
            actions = actions.and(BeforeInferenceAction::State(AnyStateAction::new::<
                ContextManagerState,
            >(boundary_action)));
            if let Some(prune_action) = prune_action {
                effective_state.reduce(prune_action.clone());
                actions = actions.and(BeforeInferenceAction::State(AnyStateAction::new::<
                    ContextManagerState,
                >(prune_action)));
            }
        }

        if effective_state.boundaries.is_empty() && effective_state.artifact_refs.is_empty() {
            return actions;
        }

        actions.and(BeforeInferenceAction::AddRequestTransform(Arc::new(
            ContextAssemblyTransform::new(effective_state),
        )))
    }

    async fn after_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<AfterToolExecuteAction> {
        let Some(result) = ctx.tool_result() else {
            return ActionSet::empty();
        };
        let Some(call_id) = ctx.tool_call_id() else {
            return ActionSet::empty();
        };

        let Some(artifact) = self.maybe_build_artifact_ref(call_id, result) else {
            return ActionSet::empty();
        };

        ActionSet::single(AfterToolExecuteAction::State(AnyStateAction::new::<
            ContextManagerState,
        >(
            ContextManagerAction::AddArtifact(artifact),
        )))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;
    use tirea_contract::runtime::phase::Phase;
    use tirea_contract::RunPolicy;
    use tirea_state::DocCell;

    fn make_msg_with_id(role: Role, content: &str, id: &str) -> Message {
        match role {
            Role::System => Message::system(content).with_id(id.to_string()),
            Role::User => Message::user(content).with_id(id.to_string()),
            Role::Assistant => Message::assistant(content).with_id(id.to_string()),
            Role::Tool => Message::tool("call_0", content).with_id(id.to_string()),
        }
    }

    fn assistant_with_tool_calls(
        id: &str,
        calls: Vec<tirea_contract::thread::ToolCall>,
    ) -> Message {
        Message::assistant_with_tool_calls("tool call", calls).with_id(id.to_string())
    }

    fn tool_result_with_call(id: &str, call_id: &str, content: &str) -> Message {
        Message::tool(call_id, content).with_id(id.to_string())
    }

    #[derive(Debug)]
    struct TestSummarizer {
        calls: Mutex<Vec<SummaryPayload>>,
        response: String,
    }

    impl TestSummarizer {
        fn new(response: impl Into<String>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                response: response.into(),
            }
        }

        fn calls(&self) -> Vec<SummaryPayload> {
            self.calls.lock().expect("lock poisoned").clone()
        }
    }

    #[async_trait]
    impl ContextSummarizer for TestSummarizer {
        async fn summarize(&self, payload: SummaryPayload) -> Result<String, ContextManagerError> {
            self.calls.lock().expect("lock poisoned").push(payload);
            Ok(self.response.clone())
        }
    }

    // -- State tests --

    #[test]
    fn state_default() {
        let state = ContextManagerState::default();
        assert!(state.boundaries.is_empty());
        assert!(state.artifact_refs.is_empty());
    }

    #[test]
    fn reducer_add_boundary_replaces_latest() {
        let mut state = ContextManagerState::default();
        state.reduce(ContextManagerAction::AddBoundary(CompactBoundary {
            covers_through_message_id: "msg-1".into(),
            summary: "First summary".into(),
            original_token_count: 100,
            created_at_ms: 1000,
        }));
        state.reduce(ContextManagerAction::AddBoundary(CompactBoundary {
            covers_through_message_id: "msg-5".into(),
            summary: "Second summary".into(),
            original_token_count: 200,
            created_at_ms: 2000,
        }));

        assert_eq!(state.boundaries.len(), 1);
        assert_eq!(state.boundaries[0].summary, "Second summary");
    }

    #[test]
    fn reducer_add_artifact_deduplicates_by_tool_call_id() {
        let mut state = ContextManagerState::default();
        state.reduce(ContextManagerAction::AddArtifact(ArtifactRef {
            message_id: None,
            tool_call_id: Some("call-1".into()),
            label: "first".into(),
            summary: "preview".into(),
            original_size: 10,
            original_token_count: 3,
        }));
        state.reduce(ContextManagerAction::AddArtifact(ArtifactRef {
            message_id: None,
            tool_call_id: Some("call-1".into()),
            label: "updated".into(),
            summary: "preview-2".into(),
            original_size: 20,
            original_token_count: 4,
        }));

        assert_eq!(state.artifact_refs.len(), 1);
        assert_eq!(state.artifact_refs[0].label, "updated");
    }

    #[test]
    fn reducer_prune_artifacts_by_exact_ids() {
        let mut state = ContextManagerState {
            boundaries: vec![],
            artifact_refs: vec![
                ArtifactRef {
                    message_id: Some("msg-1".into()),
                    tool_call_id: Some("call-1".into()),
                    label: "old".into(),
                    summary: "old preview".into(),
                    original_size: 100,
                    original_token_count: 25,
                },
                ArtifactRef {
                    message_id: Some("msg-2".into()),
                    tool_call_id: Some("call-2".into()),
                    label: "keep".into(),
                    summary: "keep preview".into(),
                    original_size: 200,
                    original_token_count: 50,
                },
            ],
        };

        state.reduce(ContextManagerAction::PruneArtifacts {
            message_ids: vec!["msg-1".into()],
            tool_call_ids: vec![],
        });

        assert_eq!(state.artifact_refs.len(), 1);
        assert_eq!(state.artifact_refs[0].label, "keep");
    }

    // -- Transform tests --

    #[test]
    fn transform_no_boundaries_passthrough() {
        let transform = ContextAssemblyTransform::new(ContextManagerState::default());
        let messages = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant("hi"),
        ];
        let output = transform.transform(messages.clone(), &[]);
        assert_eq!(output.messages.len(), 3);
        assert_eq!(output.messages[1].content, "hello");
    }

    #[test]
    fn transform_replaces_pre_boundary_messages() {
        let state = ContextManagerState {
            boundaries: vec![CompactBoundary {
                covers_through_message_id: "msg-2".into(),
                summary: "User asked about weather, assistant replied sunny.".into(),
                original_token_count: 50,
                created_at_ms: 1000,
            }],
            artifact_refs: vec![],
        };
        let transform = ContextAssemblyTransform::new(state);

        let messages = vec![
            make_msg_with_id(Role::System, "You are helpful.", "sys-1"),
            make_msg_with_id(Role::User, "What is the weather?", "msg-1"),
            make_msg_with_id(Role::Assistant, "It is sunny today.", "msg-2"),
            make_msg_with_id(Role::User, "Thanks!", "msg-3"),
            make_msg_with_id(Role::Assistant, "You're welcome!", "msg-4"),
        ];

        let output = transform.transform(messages, &[]);
        assert_eq!(output.messages.len(), 4);
        assert_eq!(output.messages[0].content, "You are helpful.");
        assert!(output.messages[1].content.contains(SUMMARY_MESSAGE_OPEN));
        assert!(output.messages[1].content.contains("sunny"));
        assert_eq!(output.messages[2].content, "Thanks!");
        assert_eq!(output.messages[3].content, "You're welcome!");
    }

    #[test]
    fn transform_frontier_boundary_can_replace_all_history() {
        let state = ContextManagerState {
            boundaries: vec![CompactBoundary {
                covers_through_message_id: "msg-2".into(),
                summary: "User asked about weather, assistant replied sunny.".into(),
                original_token_count: 50,
                created_at_ms: 1000,
            }],
            artifact_refs: vec![],
        };
        let transform = ContextAssemblyTransform::new(state);

        let messages = vec![
            make_msg_with_id(Role::System, "You are helpful.", "sys-1"),
            make_msg_with_id(Role::User, "What is the weather?", "msg-1"),
            make_msg_with_id(Role::Assistant, "It is sunny today.", "msg-2"),
        ];

        let output = transform.transform(messages, &[]);
        assert_eq!(output.messages.len(), 2);
        assert_eq!(output.messages[0].content, "You are helpful.");
        assert!(output.messages[1].content.contains(SUMMARY_MESSAGE_OPEN));
        assert!(output.messages[1].content.contains("sunny"));
    }

    #[test]
    fn transform_only_preserves_leading_system_messages() {
        let state = ContextManagerState {
            boundaries: vec![CompactBoundary {
                covers_through_message_id: "msg-3".into(),
                summary: "summary".into(),
                original_token_count: 10,
                created_at_ms: 1,
            }],
            artifact_refs: vec![],
        };
        let transform = ContextAssemblyTransform::new(state);
        let messages = vec![
            make_msg_with_id(Role::System, "system prompt", "sys-1"),
            make_msg_with_id(Role::User, "hello", "msg-1"),
            Message::internal_system("historical reminder").with_id("msg-2".into()),
            make_msg_with_id(Role::Assistant, "before boundary", "msg-3"),
            make_msg_with_id(Role::Assistant, "after boundary", "msg-4"),
        ];

        let output = transform.transform(messages, &[]);
        assert_eq!(output.messages.len(), 3);
        assert_eq!(output.messages[0].content, "system prompt");
        assert!(output.messages[1].content.contains(SUMMARY_MESSAGE_OPEN));
        assert_eq!(output.messages[2].content, "after boundary");
    }

    #[test]
    fn transform_boundary_message_not_found_passthrough() {
        let state = ContextManagerState {
            boundaries: vec![CompactBoundary {
                covers_through_message_id: "nonexistent".into(),
                summary: "Should not appear".into(),
                original_token_count: 0,
                created_at_ms: 1000,
            }],
            artifact_refs: vec![],
        };
        let transform = ContextAssemblyTransform::new(state);

        let messages = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant("hi"),
        ];
        let output = transform.transform(messages.clone(), &[]);
        assert_eq!(output.messages.len(), 3);
        assert_eq!(output.messages[1].content, "hello");
    }

    #[test]
    fn transform_artifact_ref_replaces_content_by_tool_call_id() {
        let state = ContextManagerState {
            boundaries: vec![],
            artifact_refs: vec![ArtifactRef {
                message_id: None,
                tool_call_id: Some("call-1".into()),
                label: "file.rs".into(),
                summary: "tool: read_file\nstatus: success\ndata preview:\nfn main() {}".into(),
                original_size: 25_000,
                original_token_count: 6_000,
            }],
        };
        let transform = ContextAssemblyTransform::new(state);

        let messages = vec![
            Message::system("sys"),
            make_msg_with_id(Role::User, "Read the file", "msg-1"),
            tool_result_with_call(
                "tool-msg-1",
                "call-1",
                "fn main() { /* very long content */ }",
            ),
            make_msg_with_id(Role::Assistant, "Here is the file content.", "msg-2"),
        ];

        let output = transform.transform(messages, &[]);
        assert_eq!(output.messages.len(), 4);
        assert!(output.messages[2]
            .content
            .contains("[Artifact compacted: file.rs]"));
        assert!(output.messages[2].content.contains("6000 tokens"));
    }

    #[test]
    fn transform_repairs_orphaned_tool_result_after_manual_boundary() {
        let state = ContextManagerState {
            boundaries: vec![CompactBoundary {
                covers_through_message_id: "msg-2".into(),
                summary: "summary".into(),
                original_token_count: 40,
                created_at_ms: 1,
            }],
            artifact_refs: vec![],
        };
        let transform = ContextAssemblyTransform::new(state);
        let messages = vec![
            make_msg_with_id(Role::System, "sys", "sys-1"),
            assistant_with_tool_calls(
                "msg-2",
                vec![tirea_contract::thread::ToolCall::new(
                    "call-1",
                    "search",
                    json!({"q": "rust"}),
                )],
            ),
            tool_result_with_call("tool-1", "call-1", "result"),
            make_msg_with_id(Role::User, "next", "msg-3"),
        ];

        let output = transform.transform(messages, &[]);
        assert_eq!(output.messages.len(), 3);
        assert!(output.messages[1].content.contains(SUMMARY_MESSAGE_OPEN));
        assert_eq!(output.messages[2].content, "next");
    }

    #[test]
    fn transform_patches_dangling_tool_call_after_boundary() {
        let state = ContextManagerState {
            boundaries: vec![CompactBoundary {
                covers_through_message_id: "msg-1".into(),
                summary: "summary".into(),
                original_token_count: 10,
                created_at_ms: 1,
            }],
            artifact_refs: vec![],
        };
        let transform = ContextAssemblyTransform::new(state);
        let messages = vec![
            make_msg_with_id(Role::System, "sys", "sys-1"),
            make_msg_with_id(Role::User, "earlier", "msg-1"),
            assistant_with_tool_calls(
                "msg-2",
                vec![tirea_contract::thread::ToolCall::new(
                    "call-1",
                    "search",
                    json!({"q": "rust"}),
                )],
            ),
            make_msg_with_id(Role::User, "after", "msg-3"),
        ];

        let output = transform.transform(messages, &[]);
        assert_eq!(output.messages.len(), 5);
        assert!(output.messages[1].content.contains(SUMMARY_MESSAGE_OPEN));
        assert_eq!(output.messages[2].role, Role::Assistant);
        assert_eq!(output.messages[3].role, Role::Tool);
        assert_eq!(output.messages[3].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(output.messages[3].content, INTERRUPTED_TOOL_RESULT_NOTICE);
        assert_eq!(output.messages[4].content, "after");
    }

    // -- Planner tests --

    #[test]
    fn planner_keeps_latest_user_turn_raw() {
        let messages = vec![
            Arc::new(make_msg_with_id(Role::User, "old", "msg-1")),
            Arc::new(make_msg_with_id(Role::Assistant, "old reply", "msg-2")),
            Arc::new(make_msg_with_id(Role::User, "current request", "msg-3")),
        ];

        let plan = find_compaction_plan(
            &messages,
            &ContextManagerState::default(),
            &HashMap::new(),
            &HashMap::new(),
            ContextCompactionMode::KeepRecentRawSuffix,
            2,
        )
        .expect("should find boundary before current user");

        assert_eq!(plan.boundary_message_id, "msg-1");
    }

    #[test]
    fn planner_compacts_through_latest_safe_frontier() {
        let messages = vec![
            Arc::new(make_msg_with_id(Role::User, "old", "msg-1")),
            Arc::new(make_msg_with_id(Role::Assistant, "old reply", "msg-2")),
            Arc::new(make_msg_with_id(Role::User, "current request", "msg-3")),
        ];

        let plan = find_compaction_plan(
            &messages,
            &ContextManagerState::default(),
            &HashMap::new(),
            &HashMap::new(),
            ContextCompactionMode::CompactToSafeFrontier,
            2,
        )
        .expect("frontier mode should compact through the latest safe message");

        assert_eq!(plan.boundary_message_id, "msg-3");
    }

    #[test]
    fn planner_does_not_cut_open_tool_round() {
        let messages = vec![
            Arc::new(make_msg_with_id(Role::User, "start", "msg-1")),
            Arc::new(assistant_with_tool_calls(
                "msg-2",
                vec![tirea_contract::thread::ToolCall::new(
                    "call-1",
                    "search",
                    json!({"q": "rust"}),
                )],
            )),
            Arc::new(make_msg_with_id(Role::User, "later user", "msg-3")),
        ];

        let plan = find_compaction_plan(
            &messages,
            &ContextManagerState::default(),
            &HashMap::new(),
            &HashMap::new(),
            ContextCompactionMode::KeepRecentRawSuffix,
            2,
        )
        .expect("older closed prefix should still be compactable");
        assert_eq!(plan.boundary_message_id, "msg-1");
    }

    #[test]
    fn planner_frontier_mode_stops_before_open_tool_round() {
        let messages = vec![
            Arc::new(make_msg_with_id(Role::User, "start", "msg-1")),
            Arc::new(assistant_with_tool_calls(
                "msg-2",
                vec![tirea_contract::thread::ToolCall::new(
                    "call-1",
                    "search",
                    json!({"q": "rust"}),
                )],
            )),
            Arc::new(make_msg_with_id(Role::User, "later user", "msg-3")),
        ];

        let plan = find_compaction_plan(
            &messages,
            &ContextManagerState::default(),
            &HashMap::new(),
            &HashMap::new(),
            ContextCompactionMode::CompactToSafeFrontier,
            2,
        )
        .expect("frontier mode should stop at the latest safe boundary");
        assert_eq!(plan.boundary_message_id, "msg-1");
    }

    #[test]
    fn planner_suffix_mode_respects_custom_raw_suffix_size() {
        let messages = vec![
            Arc::new(make_msg_with_id(Role::User, "older request", "msg-1")),
            Arc::new(make_msg_with_id(Role::Assistant, "older reply", "msg-2")),
            Arc::new(make_msg_with_id(Role::User, "recent request", "msg-3")),
            Arc::new(make_msg_with_id(Role::Assistant, "recent reply", "msg-4")),
        ];

        let plan = find_compaction_plan(
            &messages,
            &ContextManagerState::default(),
            &HashMap::new(),
            &HashMap::new(),
            ContextCompactionMode::KeepRecentRawSuffix,
            3,
        )
        .expect("custom raw suffix should still leave an older prefix to compact");

        assert_eq!(plan.boundary_message_id, "msg-1");
        assert_eq!(plan.covered_message_ids, vec!["msg-1".to_string()]);
    }

    #[test]
    fn planner_skips_messages_already_covered_by_existing_boundary() {
        let messages = vec![
            Arc::new(make_msg_with_id(Role::User, "first", "msg-1")),
            Arc::new(make_msg_with_id(Role::Assistant, "second", "msg-2")),
            Arc::new(make_msg_with_id(Role::User, "third", "msg-3")),
            Arc::new(make_msg_with_id(Role::Assistant, "fourth", "msg-4")),
        ];
        let state = ContextManagerState {
            boundaries: vec![CompactBoundary {
                covers_through_message_id: "msg-2".into(),
                summary: "prior summary".into(),
                original_token_count: 20,
                created_at_ms: 1,
            }],
            artifact_refs: vec![],
        };

        let plan = find_compaction_plan(
            &messages,
            &state,
            &HashMap::new(),
            &HashMap::new(),
            ContextCompactionMode::CompactToSafeFrontier,
            2,
        )
        .expect("unsummarized suffix should remain compactable");

        assert_eq!(plan.start_index, 2);
        assert_eq!(plan.boundary_message_id, "msg-4");
        assert_eq!(
            plan.covered_message_ids,
            vec!["msg-3".to_string(), "msg-4".to_string()]
        );
    }

    #[test]
    fn planner_frontier_mode_advances_to_latest_closed_round_before_open_call() {
        let messages = vec![
            Arc::new(make_msg_with_id(Role::User, "start", "msg-1")),
            Arc::new(assistant_with_tool_calls(
                "msg-2",
                vec![tirea_contract::thread::ToolCall::new(
                    "call-1",
                    "search",
                    json!({"q": "rust"}),
                )],
            )),
            Arc::new(tool_result_with_call("tool-1", "call-1", "done")),
            Arc::new(assistant_with_tool_calls(
                "msg-3",
                vec![tirea_contract::thread::ToolCall::new(
                    "call-2",
                    "grep",
                    json!({"pattern": "fn"}),
                )],
            )),
            Arc::new(make_msg_with_id(Role::User, "later", "msg-4")),
        ];

        let plan = find_compaction_plan(
            &messages,
            &ContextManagerState::default(),
            &HashMap::new(),
            &HashMap::new(),
            ContextCompactionMode::CompactToSafeFrontier,
            2,
        )
        .expect("frontier mode should stop at the most recent closed round");

        assert_eq!(plan.boundary_message_id, "tool-1");
    }

    // -- Plugin tests --

    #[test]
    fn plugin_id() {
        let plugin = ContextManagerPlugin::new(ContextWindowPolicy::default());
        use tirea_contract::runtime::AgentBehavior;
        assert_eq!(plugin.id(), "context_manager");
    }

    #[tokio::test]
    async fn plugin_before_inference_no_state_no_threshold() {
        let plugin = ContextManagerPlugin::new(ContextWindowPolicy::default());
        use tirea_contract::runtime::AgentBehavior;

        let config = RunPolicy::new();
        let doc = DocCell::new(json!({}));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions = plugin.before_inference(&ctx).await;
        assert!(
            actions.is_empty(),
            "no state and no threshold = no transform"
        );
    }

    #[tokio::test]
    async fn plugin_before_inference_with_boundary_registers_transform() {
        let plugin = ContextManagerPlugin::new(ContextWindowPolicy::default());
        use tirea_contract::runtime::AgentBehavior;

        let state = ContextManagerState {
            boundaries: vec![CompactBoundary {
                covers_through_message_id: "msg-5".into(),
                summary: "A summary".into(),
                original_token_count: 200,
                created_at_ms: 1000,
            }],
            artifact_refs: vec![],
        };
        let state_value = serde_json::to_value(&state).unwrap();
        let doc = DocCell::new(json!({ "__context": state_value }));
        let config = RunPolicy::new();
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions = plugin.before_inference(&ctx).await;
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            actions.into_vec().pop().unwrap(),
            BeforeInferenceAction::AddRequestTransform(_)
        ));
    }

    #[tokio::test]
    async fn plugin_before_inference_autocompacts_and_registers_transform() {
        use tirea_contract::runtime::AgentBehavior;

        let summarizer = Arc::new(TestSummarizer::new("compacted summary"));
        let plugin = ContextManagerPlugin::new(ContextWindowPolicy {
            max_context_tokens: 4_000,
            max_output_tokens: 512,
            min_recent_messages: 4,
            enable_prompt_cache: false,
            autocompact_threshold: Some(30),
            ..ContextWindowPolicy::default()
        })
        .with_summarizer(summarizer.clone());

        let messages: Vec<Arc<Message>> = vec![
            Arc::new(make_msg_with_id(
                Role::User,
                &"old request with enough content to exceed the threshold ".repeat(120),
                "msg-1",
            )),
            Arc::new(make_msg_with_id(
                Role::Assistant,
                &"old reply with enough content to exceed the threshold ".repeat(120),
                "msg-2",
            )),
            Arc::new(make_msg_with_id(Role::User, "current request", "msg-3")),
        ];
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({}));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &messages, &config, &doc);

        let actions = plugin.before_inference(&ctx).await.into_vec();
        assert_eq!(actions.len(), 3);
        assert!(matches!(actions[0], BeforeInferenceAction::State(_)));
        assert!(matches!(actions[1], BeforeInferenceAction::State(_)));
        assert!(matches!(
            actions[2],
            BeforeInferenceAction::AddRequestTransform(_)
        ));

        let calls = summarizer.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].transcript.contains("old request"));
        assert!(!calls[0].transcript.contains("current request"));
    }

    #[tokio::test]
    async fn plugin_before_inference_frontier_mode_compacts_current_frontier() {
        use tirea_contract::runtime::AgentBehavior;

        let summarizer = Arc::new(TestSummarizer::new("frontier summary"));
        let plugin = ContextManagerPlugin::new(ContextWindowPolicy {
            max_context_tokens: 4_000,
            max_output_tokens: 512,
            min_recent_messages: 4,
            enable_prompt_cache: false,
            autocompact_threshold: Some(30),
            compaction_mode: ContextCompactionMode::CompactToSafeFrontier,
            ..ContextWindowPolicy::default()
        })
        .with_summarizer(summarizer.clone());

        let messages: Vec<Arc<Message>> = vec![
            Arc::new(make_msg_with_id(
                Role::User,
                &"old request with enough content to exceed the threshold ".repeat(120),
                "msg-1",
            )),
            Arc::new(make_msg_with_id(
                Role::Assistant,
                &"old reply with enough content to exceed the threshold ".repeat(120),
                "msg-2",
            )),
            Arc::new(make_msg_with_id(Role::User, "current request", "msg-3")),
        ];
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({}));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &messages, &config, &doc);

        let actions = plugin.before_inference(&ctx).await.into_vec();
        assert_eq!(actions.len(), 3);
        assert!(matches!(actions[0], BeforeInferenceAction::State(_)));
        assert!(matches!(actions[1], BeforeInferenceAction::State(_)));
        assert!(matches!(
            actions[2],
            BeforeInferenceAction::AddRequestTransform(_)
        ));

        let calls = summarizer.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].transcript.contains("old request"));
        assert!(calls[0].transcript.contains("current request"));
    }

    #[tokio::test]
    async fn plugin_before_inference_suffix_mode_respects_custom_raw_suffix() {
        use tirea_contract::runtime::AgentBehavior;

        let summarizer = Arc::new(TestSummarizer::new("suffix summary"));
        let plugin = ContextManagerPlugin::new(ContextWindowPolicy {
            max_context_tokens: 4_000,
            max_output_tokens: 512,
            min_recent_messages: 4,
            enable_prompt_cache: false,
            autocompact_threshold: Some(30),
            compaction_mode: ContextCompactionMode::KeepRecentRawSuffix,
            compaction_raw_suffix_messages: 2,
        })
        .with_summarizer(summarizer.clone());

        let messages: Vec<Arc<Message>> = vec![
            Arc::new(make_msg_with_id(
                Role::User,
                &"old request with enough content to exceed the threshold ".repeat(120),
                "msg-1",
            )),
            Arc::new(make_msg_with_id(
                Role::Assistant,
                &"old reply with enough content to exceed the threshold ".repeat(120),
                "msg-2",
            )),
            Arc::new(make_msg_with_id(
                Role::User,
                &"recent request that should remain raw ".repeat(40),
                "msg-3",
            )),
            Arc::new(make_msg_with_id(
                Role::Assistant,
                &"recent reply that should remain raw ".repeat(40),
                "msg-4",
            )),
        ];
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({}));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &messages, &config, &doc);

        let actions = plugin.before_inference(&ctx).await.into_vec();
        assert_eq!(actions.len(), 3);

        let calls = summarizer.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].transcript.contains("old request"));
        assert!(calls[0].transcript.contains("old reply"));
        assert!(!calls[0].transcript.contains("recent request"));
        assert!(!calls[0].transcript.contains("recent reply"));
    }

    #[tokio::test]
    async fn plugin_before_inference_frontier_mode_uses_previous_summary_for_delta_only() {
        use tirea_contract::runtime::AgentBehavior;

        let summarizer = Arc::new(TestSummarizer::new("updated frontier summary"));
        let plugin = ContextManagerPlugin::new(ContextWindowPolicy {
            max_context_tokens: 4_000,
            max_output_tokens: 512,
            min_recent_messages: 4,
            enable_prompt_cache: false,
            autocompact_threshold: Some(30),
            compaction_mode: ContextCompactionMode::CompactToSafeFrontier,
            ..ContextWindowPolicy::default()
        })
        .with_summarizer(summarizer.clone());

        let state = ContextManagerState {
            boundaries: vec![CompactBoundary {
                covers_through_message_id: "msg-2".into(),
                summary: "previous summary".into(),
                original_token_count: 100,
                created_at_ms: 1,
            }],
            artifact_refs: vec![],
        };
        let state_value = serde_json::to_value(&state).unwrap();
        let doc = DocCell::new(json!({ "__context": state_value }));
        let messages: Vec<Arc<Message>> = vec![
            Arc::new(make_msg_with_id(
                Role::User,
                &"old request already summarized ".repeat(80),
                "msg-1",
            )),
            Arc::new(make_msg_with_id(
                Role::Assistant,
                &"old reply already summarized ".repeat(80),
                "msg-2",
            )),
            Arc::new(make_msg_with_id(
                Role::User,
                &"new request to merge into summary ".repeat(120),
                "msg-3",
            )),
            Arc::new(make_msg_with_id(
                Role::Assistant,
                &"new reply to merge into summary ".repeat(120),
                "msg-4",
            )),
        ];
        let config = RunPolicy::new();
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &messages, &config, &doc);

        let actions = plugin.before_inference(&ctx).await.into_vec();
        assert_eq!(actions.len(), 3);

        let calls = summarizer.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].previous_summary.as_deref(),
            Some("previous summary")
        );
        assert!(!calls[0].transcript.contains("already summarized"));
        assert!(calls[0].transcript.contains("new request"));
        assert!(calls[0].transcript.contains("new reply"));
    }

    #[tokio::test]
    async fn plugin_before_inference_frontier_mode_excludes_open_tool_round_from_summary() {
        use tirea_contract::runtime::AgentBehavior;

        let summarizer = Arc::new(TestSummarizer::new("frontier summary"));
        let plugin = ContextManagerPlugin::new(ContextWindowPolicy {
            max_context_tokens: 4_000,
            max_output_tokens: 512,
            min_recent_messages: 4,
            enable_prompt_cache: false,
            autocompact_threshold: Some(30),
            compaction_mode: ContextCompactionMode::CompactToSafeFrontier,
            ..ContextWindowPolicy::default()
        })
        .with_summarizer(summarizer.clone());

        let messages: Vec<Arc<Message>> = vec![
            Arc::new(make_msg_with_id(
                Role::User,
                &"very old request with enough content to exceed the threshold ".repeat(120),
                "msg-1",
            )),
            Arc::new(assistant_with_tool_calls(
                "msg-2",
                vec![tirea_contract::thread::ToolCall::new(
                    "call-1",
                    "search",
                    json!({"q": "rust"}),
                )],
            )),
            Arc::new(make_msg_with_id(Role::User, "later user", "msg-3")),
        ];
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({}));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &messages, &config, &doc);

        let actions = plugin.before_inference(&ctx).await.into_vec();
        assert_eq!(actions.len(), 3);

        let calls = summarizer.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].transcript.contains("very old request"));
        assert!(!calls[0].transcript.contains("tool_calls:"));
        assert!(!calls[0].transcript.contains("later user"));
    }

    #[tokio::test]
    async fn plugin_after_tool_execute_adds_artifact_ref_for_large_result() {
        use tirea_contract::runtime::AgentBehavior;

        let plugin = ContextManagerPlugin::new(ContextWindowPolicy::default())
            .with_artifact_compact_threshold_tokens(10);
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({}));
        let result = ToolResult::success(
            "read_file",
            json!({
                "path": "src/lib.rs",
                "content": "x".repeat(400)
            }),
        );
        let ctx = ReadOnlyContext::new(Phase::AfterToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("read_file", "call-1", None)
            .with_tool_result(&result);

        let actions = plugin.after_tool_execute(&ctx).await.into_vec();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], AfterToolExecuteAction::State(_)));
    }

    #[test]
    fn latest_boundary_wins() {
        let state = ContextManagerState {
            boundaries: vec![CompactBoundary {
                covers_through_message_id: "msg-4".into(),
                summary: "New summary".into(),
                original_token_count: 100,
                created_at_ms: 2000,
            }],
            artifact_refs: vec![],
        };
        let transform = ContextAssemblyTransform::new(state);

        let messages = vec![
            make_msg_with_id(Role::System, "sys", "sys-1"),
            make_msg_with_id(Role::User, "q1", "msg-1"),
            make_msg_with_id(Role::Assistant, "a1", "msg-2"),
            make_msg_with_id(Role::User, "q2", "msg-3"),
            make_msg_with_id(Role::Assistant, "a2", "msg-4"),
            make_msg_with_id(Role::User, "q3", "msg-5"),
        ];

        let output = transform.transform(messages, &[]);
        assert_eq!(output.messages.len(), 3);
        assert!(output.messages[1].content.contains("New summary"));
        assert_eq!(output.messages[2].content, "q3");
    }

    // -- Lazy trim tests --

    fn thread_with_context_state(
        messages: Vec<Message>,
        cm_state: &ContextManagerState,
    ) -> Thread {
        let state = json!({
            <ContextManagerState as State>::PATH: serde_json::to_value(cm_state).unwrap()
        });
        Thread::with_initial_state("test-thread", state).with_messages(messages)
    }

    #[test]
    fn trim_no_boundary_is_noop() {
        let messages = vec![
            Message::user("hello").with_id("msg-1".into()),
            Message::assistant("hi").with_id("msg-2".into()),
        ];
        let mut thread = thread_with_context_state(messages, &ContextManagerState::default());
        let original_len = thread.messages.len();

        trim_thread_to_latest_boundary(&mut thread);

        assert_eq!(thread.messages.len(), original_len);
        assert_eq!(thread.messages[0].content, "hello");
        assert_eq!(thread.messages[1].content, "hi");
    }

    #[test]
    fn trim_replaces_pre_boundary_messages() {
        let messages = vec![
            Message::user("q1").with_id("msg-1".into()),
            Message::assistant("a1").with_id("msg-2".into()),
            Message::user("q2").with_id("msg-3".into()),
            Message::assistant("a2").with_id("msg-4".into()),
        ];
        let cm_state = ContextManagerState {
            boundaries: vec![CompactBoundary {
                covers_through_message_id: "msg-2".into(),
                summary: "Summary of q1/a1".into(),
                original_token_count: 50,
                created_at_ms: 1000,
            }],
            artifact_refs: vec![],
        };
        let mut thread = thread_with_context_state(messages, &cm_state);

        trim_thread_to_latest_boundary(&mut thread);

        assert_eq!(thread.messages.len(), 3); // summary + msg-3 + msg-4
        assert!(thread.messages[0].content.contains(SUMMARY_MESSAGE_OPEN));
        assert!(thread.messages[0].content.contains("Summary of q1/a1"));
        assert!(thread.messages[0].content.contains(SUMMARY_MESSAGE_CLOSE));
        assert_eq!(thread.messages[0].role, Role::System);
    }

    #[test]
    fn trim_preserves_post_boundary_messages() {
        let messages = vec![
            Message::user("q1").with_id("msg-1".into()),
            Message::assistant("a1").with_id("msg-2".into()),
            Message::user("q2").with_id("msg-3".into()),
            Message::assistant("a2").with_id("msg-4".into()),
        ];
        let cm_state = ContextManagerState {
            boundaries: vec![CompactBoundary {
                covers_through_message_id: "msg-2".into(),
                summary: "Summary".into(),
                original_token_count: 50,
                created_at_ms: 1000,
            }],
            artifact_refs: vec![],
        };
        let mut thread = thread_with_context_state(messages, &cm_state);

        trim_thread_to_latest_boundary(&mut thread);

        assert_eq!(thread.messages[1].content, "q2");
        assert_eq!(thread.messages[1].id.as_deref(), Some("msg-3"));
        assert_eq!(thread.messages[2].content, "a2");
        assert_eq!(thread.messages[2].id.as_deref(), Some("msg-4"));
    }

    #[test]
    fn trim_boundary_message_not_found_is_noop() {
        let messages = vec![
            Message::user("q1").with_id("msg-1".into()),
            Message::assistant("a1").with_id("msg-2".into()),
        ];
        let cm_state = ContextManagerState {
            boundaries: vec![CompactBoundary {
                covers_through_message_id: "nonexistent-id".into(),
                summary: "Summary".into(),
                original_token_count: 50,
                created_at_ms: 1000,
            }],
            artifact_refs: vec![],
        };
        let mut thread = thread_with_context_state(messages, &cm_state);

        trim_thread_to_latest_boundary(&mut thread);

        assert_eq!(thread.messages.len(), 2);
        assert_eq!(thread.messages[0].content, "q1");
    }

    #[test]
    fn trim_state_parse_failure_is_noop() {
        let state = json!({ <ContextManagerState as State>::PATH: "not-valid-json-object" });
        let mut thread = Thread::with_initial_state("test-thread", state)
            .with_message(Message::user("q1").with_id("msg-1".into()));

        trim_thread_to_latest_boundary(&mut thread);

        assert_eq!(thread.messages.len(), 1);
        assert_eq!(thread.messages[0].content, "q1");
    }

    #[test]
    fn trim_idempotent() {
        let messages = vec![
            Message::user("q1").with_id("msg-1".into()),
            Message::assistant("a1").with_id("msg-2".into()),
            Message::user("q2").with_id("msg-3".into()),
        ];
        let cm_state = ContextManagerState {
            boundaries: vec![CompactBoundary {
                covers_through_message_id: "msg-2".into(),
                summary: "Summary".into(),
                original_token_count: 50,
                created_at_ms: 1000,
            }],
            artifact_refs: vec![],
        };
        let mut thread = thread_with_context_state(messages, &cm_state);

        trim_thread_to_latest_boundary(&mut thread);
        assert_eq!(thread.messages.len(), 2); // summary + msg-3

        // Second call: boundary message "msg-2" no longer in messages → noop.
        trim_thread_to_latest_boundary(&mut thread);
        assert_eq!(thread.messages.len(), 2);
        assert!(thread.messages[0].content.contains("Summary"));
        assert_eq!(thread.messages[1].content, "q2");
    }

    #[test]
    fn unsummarized_start_returns_zero_after_trim() {
        let summary = Arc::new(Message::internal_system("summary of earlier"));
        let msg_a = Arc::new(Message::user("q1").with_id("msg-3".into()));
        let msg_b = Arc::new(Message::assistant("a1").with_id("msg-4".into()));
        let messages = vec![summary, msg_a, msg_b];

        // State still has a boundary referencing a message that was trimmed.
        let state = ContextManagerState {
            boundaries: vec![CompactBoundary {
                covers_through_message_id: "msg-2".into(),
                summary: "old summary".into(),
                original_token_count: 50,
                created_at_ms: 1000,
            }],
            artifact_refs: vec![],
        };

        let result = unsummarized_start_index(&messages, &state);
        assert_eq!(result, Some(0));
    }

    #[test]
    fn compaction_plan_after_trim() {
        let summary = Arc::new(Message::internal_system("summary of earlier"));
        let msg_a = Arc::new(Message::user("q2").with_id("msg-3".into()));
        let msg_b = Arc::new(Message::assistant("a2").with_id("msg-4".into()));
        let msg_c = Arc::new(Message::user("q3").with_id("msg-5".into()));
        let msg_d = Arc::new(Message::assistant("a3").with_id("msg-6".into()));
        let messages = vec![summary, msg_a, msg_b, msg_c, msg_d];

        let state = ContextManagerState {
            boundaries: vec![CompactBoundary {
                covers_through_message_id: "msg-2".into(),
                summary: "old summary".into(),
                original_token_count: 50,
                created_at_ms: 1000,
            }],
            artifact_refs: vec![],
        };

        let tool_states = HashMap::new();
        let suspended = HashMap::new();
        let plan = find_compaction_plan(
            &messages,
            &state,
            &tool_states,
            &suspended,
            ContextCompactionMode::CompactToSafeFrontier,
            0,
        );

        assert!(plan.is_some(), "should produce a compaction plan after trim");
        let plan = plan.unwrap();
        assert_eq!(plan.start_index, 0);
        assert!(plan.boundary_index > 0);
    }
}
