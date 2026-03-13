use std::collections::{HashMap, HashSet};

use tirea_contract::runtime::inference::{
    ContextWindowPolicy, InferenceRequestTransform, InferenceTransformOutput,
};
use tirea_contract::runtime::tool_call::ToolDescriptor;
use tirea_contract::thread::{Message, Role};

use crate::engine::context_window::truncate_to_budget;
use crate::engine::token_estimator::estimate_tool_tokens;

use super::state::{ArtifactRef, ContextState};
use super::{SUMMARY_MESSAGE_OPEN, SUMMARY_MESSAGE_CLOSE};

const PENDING_APPROVAL_NOTICE_PREFIX: &str = "Tool '";
const PENDING_APPROVAL_NOTICE_SUFFIX: &str = "' is awaiting approval. Execution paused.";
pub(super) const INTERRUPTED_TOOL_RESULT_NOTICE: &str =
    "[Tool execution was interrupted before producing a result.]";

/// Inference request transform that applies logical compression then hard truncation.
///
/// Constructed each step in `before_inference` with a snapshot of the current
/// [`ContextState`] and the active [`ContextWindowPolicy`]. The transform:
/// 1. Replaces messages before the latest boundary with a summary.
/// 2. Repairs tool/result pairing after replacement.
/// 3. Substitutes large artifact content with compact views.
/// 4. Truncates history to fit the token budget.
pub(super) struct ContextTransform {
    state: ContextState,
    policy: ContextWindowPolicy,
    artifact_by_message_id: HashMap<String, ArtifactRef>,
    artifact_by_tool_call_id: HashMap<String, ArtifactRef>,
}

impl ContextTransform {
    pub(super) fn new(state: ContextState, policy: ContextWindowPolicy) -> Self {
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
            policy,
            artifact_by_message_id,
            artifact_by_tool_call_id,
        }
    }

    /// Replace messages before the latest boundary with a summary message.
    pub(super) fn apply_boundaries(&self, messages: Vec<Message>) -> Vec<Message> {
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
    pub(super) fn apply_artifact_refs(&self, messages: &mut [Message]) {
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

impl InferenceRequestTransform for ContextTransform {
    fn transform(
        &self,
        messages: Vec<Message>,
        tool_descriptors: &[ToolDescriptor],
    ) -> InferenceTransformOutput {
        // Phase 1: Logical compression (boundary replacement + artifact substitution).
        let mut result = self.apply_boundaries(messages);
        self.apply_artifact_refs(&mut result);

        // Phase 2: Hard truncation to token budget.
        let tool_tokens = estimate_tool_tokens(tool_descriptors);
        let system_end = result
            .iter()
            .position(|m| m.role != Role::System)
            .unwrap_or(result.len());
        let (system_msgs, history_msgs) = result.split_at(system_end);
        let truncated = truncate_to_budget(system_msgs, history_msgs, tool_tokens, &self.policy);

        InferenceTransformOutput {
            messages: truncated.messages.into_iter().cloned().collect(),
            enable_prompt_cache: self.policy.enable_prompt_cache,
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

pub(super) fn is_pending_approval_notice(msg: &Message) -> bool {
    msg.role == Role::Tool && is_pending_approval_notice_content(&msg.content)
}
