//! Compaction boundary discovery, plan/apply helpers, and load-time trimming.

use std::collections::HashSet;
use std::sync::Arc;

use awaken_contract::contract::inference::{ContextCompactionMode, ContextWindowPolicy};
use awaken_contract::contract::message::{Message, Role, Visibility};
use awaken_contract::contract::transform::estimate_message_tokens;

use super::plugin::{
    CompactionAction, CompactionBoundary, CompactionFailure, CompactionInFlight, CompactionSkipped,
    CompactionStateKey,
};
use super::summarizer::{MIN_COMPACTION_GAIN_TOKENS, extract_previous_summary, render_transcript};
use crate::state::{MutationBatch, StateStore};

/// Custom event type emitted by a successful background compaction task.
pub const COMPACTION_COMPLETED_EVENT: &str = "context.compacted";
/// Custom event type emitted when a background compaction task fails.
pub const COMPACTION_FAILED_EVENT: &str = "context.compaction_failed";
/// Custom event type emitted when a background compaction task starts.
pub const COMPACTION_STARTED_EVENT: &str = "compaction.started";
/// Custom event type emitted when a background compaction task is not applied.
pub const COMPACTION_SKIPPED_EVENT: &str = "compaction.skipped";

pub const COMPACTION_SKIP_REASON_MIN_SAVINGS_RATIO: &str = "min_savings_ratio";
const RATIO_PPM: f64 = 1_000_000.0;

/// Find a safe compaction boundary in the message history.
///
/// Returns the index of the last message that can be safely compacted
/// (all tool call/result pairs are complete before this point).
pub fn find_compaction_boundary(
    messages: &[Arc<Message>],
    start: usize,
    end: usize,
) -> Option<usize> {
    let mut open_calls = HashSet::<String>::new();
    let mut best_boundary = None;

    for (idx, msg) in messages.iter().enumerate().skip(start).take(end - start) {
        if let Some(ref calls) = msg.tool_calls {
            for call in calls {
                open_calls.insert(call.id.clone());
            }
        }

        if msg.role == Role::Tool
            && let Some(ref call_id) = msg.tool_call_id
        {
            open_calls.remove(call_id);
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

/// Trim loaded messages to the latest compaction boundary.
///
/// If the message list contains a `<conversation-summary>` internal_system message,
/// all messages before it are dropped. The summary message becomes the first message.
/// This avoids loading already-summarized history into the context window.
///
/// Idempotent: if no summary exists or messages are already trimmed, this is a no-op.
pub fn trim_to_compaction_boundary(messages: &mut Vec<Arc<Message>>) {
    // Find the last summary message (in case of multiple compactions)
    let last_summary_idx = messages.iter().rposition(|m| {
        m.role == Role::System
            && m.visibility == Visibility::Internal
            && m.text().contains("<conversation-summary>")
    });

    if let Some(idx) = last_summary_idx
        && idx > 0
    {
        messages.drain(..idx);
    }
}

/// Record a compaction boundary in the state store.
pub fn record_compaction_boundary(
    boundary: super::plugin::CompactionBoundary,
) -> super::plugin::CompactionAction {
    super::plugin::CompactionAction::RecordBoundary(boundary)
}

/// Record a failed background compaction attempt in the state store.
pub fn record_compaction_failure(
    failure: super::plugin::CompactionFailure,
) -> super::plugin::CompactionAction {
    super::plugin::CompactionAction::RecordFailure(failure)
}

/// Record a skipped background compaction attempt in the state store.
pub fn record_compaction_skipped(skipped: CompactionSkipped) -> CompactionAction {
    CompactionAction::RecordSkipped(skipped)
}

/// Inputs needed to run a compaction off the main thread. Snapshotted at
/// trigger time so the background task does not race with the live
/// `messages` list (which keeps growing during summarization).
#[derive(Debug, Clone)]
pub struct CompactionPlan {
    /// Pre-rendered transcript to feed the summarizer (Internal messages
    /// already filtered).
    pub transcript: String,
    /// Previous cumulative summary, if any, for incremental updates.
    pub previous_summary: Option<String>,
    /// Stable id of the last message included in the summary. The swap
    /// path locates the cut point against the current message list by
    /// this id, so it survives any new messages appended in the window.
    pub boundary_message_id: String,
    /// Token estimate of the messages that the summary will replace.
    /// Used for the `pre_tokens` field of the recorded boundary.
    pub pre_tokens: usize,
}

/// Result of a successful in-place swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedCompaction {
    /// Index in the original list where the cut happened.
    pub boundary_index: usize,
    /// Tokens that were dropped from the head of the message list.
    pub pre_tokens: usize,
    /// Tokens used by the inserted summary message.
    pub post_tokens: usize,
}

/// Decide whether compaction should run right now and, if so, capture the
/// inputs needed by the background summarization task. Returns `None` if
/// compaction is not feasible (no safe boundary, savings below threshold,
/// boundary message has no stable id, transcript is empty, etc.).
pub fn plan_compaction(
    messages: &[Arc<Message>],
    policy: &ContextWindowPolicy,
) -> Option<CompactionPlan> {
    if messages.len() < 2 {
        return None;
    }
    let keep_suffix = compaction_keep_suffix(messages, policy);
    let search_end = messages.len().saturating_sub(keep_suffix);
    if search_end < 2 {
        return None;
    }
    let boundary = find_compaction_boundary(messages, 0, search_end)?;
    let boundary_message_id = messages[boundary].id.clone()?;
    let pre_tokens: usize = messages[..=boundary]
        .iter()
        .map(|m| estimate_message_tokens(m))
        .sum();
    if pre_tokens < MIN_COMPACTION_GAIN_TOKENS {
        return None;
    }
    let transcript = render_transcript(&messages[..=boundary]);
    if transcript.is_empty() {
        return None;
    }
    let previous_summary = extract_previous_summary(messages);
    Some(CompactionPlan {
        transcript,
        previous_summary,
        boundary_message_id,
        pre_tokens,
    })
}

fn compaction_keep_suffix(messages: &[Arc<Message>], policy: &ContextWindowPolicy) -> usize {
    let configured = match policy.compaction_mode {
        ContextCompactionMode::KeepRecentRawSuffix => policy.compaction_raw_suffix_messages,
        ContextCompactionMode::CompactToSafeFrontier => policy.min_recent_messages,
    };
    let latest_user_suffix = messages
        .iter()
        .rposition(|message| {
            message.role == Role::User && message.visibility != Visibility::Internal
        })
        .map_or(0, |idx| messages.len().saturating_sub(idx));
    configured.max(latest_user_suffix).min(messages.len())
}

/// Mark a background compaction pass as in-flight in the state store.
/// Used by the spawn helper after a task has been queued.
pub fn record_compaction_in_flight(in_flight: CompactionInFlight) -> CompactionAction {
    CompactionAction::SetInFlight(in_flight)
}

/// Clear the in-flight marker. Called from the inbox-event router on both
/// success and failure of the background pass.
pub fn clear_compaction_in_flight() -> CompactionAction {
    CompactionAction::ClearInFlight
}

/// Estimate the token cost of the summary message that would be inserted.
pub fn summary_message_tokens(summary_text: &str) -> usize {
    estimate_message_tokens(&summary_message(summary_text))
}

/// Calculate a deterministic savings ratio in parts per million.
pub fn compaction_savings_ratio_ppm(pre_tokens: usize, post_tokens: usize) -> u32 {
    if pre_tokens == 0 || post_tokens >= pre_tokens {
        return 0;
    }
    let ratio = (pre_tokens - post_tokens) as f64 / pre_tokens as f64;
    ratio_to_ppm(ratio)
}

/// Detect and handle a context-compaction event arriving via the inbox.
///
/// Returns `true` when `payload` was a compaction event — in that case the
/// caller MUST NOT also push it as a regular Internal user message. The
/// success path performs the message swap by stable id and records the
/// boundary; both success and failure paths clear the in-flight marker
/// so future compactions can be triggered.
///
/// Returns `false` for any payload that is not a compaction event; the
/// caller should fall back to the normal inbox handling.
pub fn try_consume_compaction_event(
    messages: &mut Vec<Arc<Message>>,
    payload: &serde_json::Value,
    store: &StateStore,
) -> bool {
    let Some(event_type) = compaction_event_type(payload) else {
        return false;
    };
    let inner = payload.get("payload");

    match event_type {
        e if e == COMPACTION_COMPLETED_EVENT => {
            let boundary_id = inner
                .and_then(|p| p.get("boundary_message_id"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let summary = inner
                .and_then(|p| p.get("summary"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let reported_pre_tokens = inner
                .and_then(|p| p.get("pre_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let prior_in_flight = store
                .read::<CompactionStateKey>()
                .and_then(|state| state.in_flight);
            let event_task_id = compaction_event_task_id(payload, inner);
            let Some(in_flight) =
                matching_in_flight(prior_in_flight, event_task_id.as_deref(), boundary_id)
            else {
                tracing::warn!(
                    task_id = event_task_id.as_deref().unwrap_or_default(),
                    boundary_message_id = boundary_id,
                    "ignoring stale compaction completion event"
                );
                return true;
            };

            let mut batch = MutationBatch::new();
            if !boundary_id.is_empty()
                && !summary.is_empty()
                && let Some(applied) = apply_summary(messages, boundary_id, summary)
            {
                batch.update::<CompactionStateKey>(CompactionAction::RecordBoundary(
                    CompactionBoundary {
                        summary: summary.to_string(),
                        task_id: Some(in_flight.task_id.clone()),
                        boundary_message_id: Some(boundary_id.to_string()),
                        pre_tokens: applied.pre_tokens.max(reported_pre_tokens),
                        post_tokens: applied.post_tokens,
                        timestamp_ms: now_ms(),
                    },
                ));
                tracing::info!(
                    pre_tokens = applied.pre_tokens,
                    post_tokens = applied.post_tokens,
                    boundary_index = applied.boundary_index,
                    "background_compaction_swap_applied"
                );
            } else {
                tracing::warn!(
                    boundary_message_id = boundary_id,
                    "background compaction completed but boundary message no longer present; skipping swap"
                );
            }
            batch.update::<CompactionStateKey>(CompactionAction::ClearInFlight);
            if let Err(error) = store.commit(batch) {
                tracing::warn!(
                    error = %error,
                    "failed to commit compaction completion state"
                );
            }
        }
        e if e == COMPACTION_FAILED_EVENT => {
            let error_text = inner
                .and_then(|p| p.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            let prior_in_flight = store
                .read::<CompactionStateKey>()
                .and_then(|state| state.in_flight);
            let boundary_message_id = inner
                .and_then(|p| p.get("boundary_message_id"))
                .and_then(|v| v.as_str())
                .filter(|value| !value.trim().is_empty())
                .map(ToOwned::to_owned)
                .or_else(|| {
                    prior_in_flight
                        .as_ref()
                        .map(|in_flight| in_flight.boundary_message_id.clone())
                })
                .unwrap_or_default();
            let event_task_id = compaction_event_task_id(payload, inner);
            let Some(in_flight) = matching_in_flight(
                prior_in_flight,
                event_task_id.as_deref(),
                &boundary_message_id,
            ) else {
                tracing::warn!(
                    task_id = event_task_id.as_deref().unwrap_or_default(),
                    boundary_message_id = boundary_message_id,
                    "ignoring stale compaction failure event"
                );
                return true;
            };
            tracing::warn!(
                error = error_text,
                "background compaction failed; clearing in-flight marker"
            );
            let mut batch = MutationBatch::new();
            batch.update::<CompactionStateKey>(CompactionAction::RecordFailure(
                CompactionFailure {
                    task_id: Some(in_flight.task_id),
                    boundary_message_id,
                    error: error_text.to_string(),
                    timestamp_ms: now_ms(),
                },
            ));
            batch.update::<CompactionStateKey>(CompactionAction::ClearInFlight);
            if let Err(error) = store.commit(batch) {
                tracing::warn!(
                    error = %error,
                    "failed to clear in-flight marker after compaction failure"
                );
            }
        }
        e if e == COMPACTION_SKIPPED_EVENT => {
            let prior_in_flight = store
                .read::<CompactionStateKey>()
                .and_then(|state| state.in_flight);
            let boundary_message_id = inner
                .and_then(|p| p.get("boundary_message_id"))
                .and_then(|v| v.as_str())
                .filter(|value| !value.trim().is_empty())
                .map(ToOwned::to_owned)
                .or_else(|| {
                    prior_in_flight
                        .as_ref()
                        .map(|in_flight| in_flight.boundary_message_id.clone())
                })
                .unwrap_or_default();
            let reason = inner
                .and_then(|p| p.get("reason"))
                .and_then(|v| v.as_str())
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("unknown")
                .to_string();
            let pre_tokens = inner
                .and_then(|p| p.get("pre_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let post_tokens = inner
                .and_then(|p| p.get("post_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let savings_ratio_ppm = inner
                .and_then(|p| p.get("savings_ratio_ppm"))
                .and_then(|v| v.as_u64())
                .unwrap_or_else(|| {
                    inner
                        .and_then(|p| p.get("savings_ratio"))
                        .and_then(|v| v.as_f64())
                        .map(ratio_to_ppm)
                        .unwrap_or_else(|| compaction_savings_ratio_ppm(pre_tokens, post_tokens))
                        .into()
                }) as u32;
            let min_savings_ratio_ppm = inner
                .and_then(|p| p.get("min_savings_ratio_ppm"))
                .and_then(|v| v.as_u64())
                .unwrap_or_else(|| {
                    inner
                        .and_then(|p| p.get("min_savings_ratio"))
                        .and_then(|v| v.as_f64())
                        .map(ratio_to_ppm)
                        .unwrap_or(0)
                        .into()
                }) as u32;
            let event_task_id = compaction_event_task_id(payload, inner);
            let Some(in_flight) = matching_in_flight(
                prior_in_flight,
                event_task_id.as_deref(),
                &boundary_message_id,
            ) else {
                tracing::warn!(
                    task_id = event_task_id.as_deref().unwrap_or_default(),
                    boundary_message_id = boundary_message_id,
                    "ignoring stale compaction skipped event"
                );
                return true;
            };
            let mut batch = MutationBatch::new();
            batch.update::<CompactionStateKey>(CompactionAction::RecordSkipped(
                CompactionSkipped {
                    task_id: Some(in_flight.task_id),
                    boundary_message_id,
                    reason,
                    pre_tokens,
                    post_tokens,
                    savings_ratio_ppm,
                    min_savings_ratio_ppm,
                    timestamp_ms: now_ms(),
                },
            ));
            batch.update::<CompactionStateKey>(CompactionAction::ClearInFlight);
            if let Err(error) = store.commit(batch) {
                tracing::warn!(
                    error = %error,
                    "failed to clear in-flight marker after skipped compaction"
                );
            }
        }
        e if e == COMPACTION_STARTED_EVENT => {}
        _ => {}
    }
    true
}

fn compaction_event_task_id(
    payload: &serde_json::Value,
    inner: Option<&serde_json::Value>,
) -> Option<String> {
    payload
        .get("task_id")
        .and_then(|v| v.as_str())
        .or_else(|| {
            inner
                .and_then(|p| p.get("task_id"))
                .and_then(|v| v.as_str())
        })
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn matching_in_flight(
    prior_in_flight: Option<CompactionInFlight>,
    event_task_id: Option<&str>,
    boundary_message_id: &str,
) -> Option<CompactionInFlight> {
    let in_flight = prior_in_flight?;
    if event_task_id != Some(in_flight.task_id.as_str()) {
        return None;
    }
    if !boundary_message_id.is_empty()
        && boundary_message_id != in_flight.boundary_message_id.as_str()
    {
        return None;
    }
    Some(in_flight)
}

fn compaction_event_type(payload: &serde_json::Value) -> Option<&str> {
    if payload.get("kind").and_then(|k| k.as_str()) != Some("custom") {
        return None;
    }
    payload
        .get("event_type")
        .and_then(|t| t.as_str())
        .filter(|t| {
            *t == COMPACTION_COMPLETED_EVENT
                || *t == COMPACTION_FAILED_EVENT
                || *t == COMPACTION_STARTED_EVENT
                || *t == COMPACTION_SKIPPED_EVENT
        })
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Apply a freshly produced summary to the live message list. Locates the
/// boundary message by id (not by index) so it is safe against any
/// messages appended between trigger and completion. Returns `None` when
/// the boundary message is no longer present (already trimmed by an
/// earlier compaction or rewritten by another path); callers should treat
/// that as a benign skip.
pub fn apply_summary(
    messages: &mut Vec<Arc<Message>>,
    boundary_message_id: &str,
    summary_text: &str,
) -> Option<AppliedCompaction> {
    let idx = messages
        .iter()
        .position(|m| m.id.as_deref() == Some(boundary_message_id))?;
    let pre_tokens: usize = messages[..=idx]
        .iter()
        .map(|m| estimate_message_tokens(m))
        .sum();
    messages.drain(..=idx);
    let summary_message = summary_message(summary_text);
    let post_tokens = estimate_message_tokens(&summary_message);
    messages.insert(0, summary_message);
    Some(AppliedCompaction {
        boundary_index: idx,
        pre_tokens,
        post_tokens,
    })
}

fn summary_message(summary_text: &str) -> Arc<Message> {
    Arc::new(Message::internal_system(format!(
        "<conversation-summary>\n{summary_text}\n</conversation-summary>"
    )))
}

fn ratio_to_ppm(ratio: f64) -> u32 {
    if !ratio.is_finite() {
        return 0;
    }
    ratio.clamp(0.0, 1.0).mul_add(RATIO_PPM, 0.0).round() as u32
}

#[cfg(test)]
mod tests;
