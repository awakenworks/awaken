//! Context management: logical compression + hard truncation.
//!
//! Combines two concerns into a single plugin:
//! 1. **Compaction** — LLM-based summarization of old messages and artifact
//!    compaction via [`ContextState`].
//! 2. **Truncation** — hard token-budget enforcement via [`truncate_to_budget`].
//!
//! Registers a single [`ContextTransform`](transform::ContextTransform) that
//! first replaces pre-boundary messages with a summary and swaps large artifact
//! content with compact views, then truncates history to fit the token budget.

mod compaction;
mod plugin;
mod state;
mod transform;

#[cfg(test)]
mod tests;

use tirea_contract::runtime::inference::{ContextCompactionMode, ContextWindowPolicy};

pub use plugin::ContextPlugin;
// State types re-exported for sibling modules (tests, etc.) within the crate.
#[allow(unused_imports)]
pub(crate) use state::{ArtifactRef, CompactBoundary, ContextAction, ContextState};
pub(crate) use compaction::trim_thread_to_latest_boundary;

/// Behavior ID used for registration.
pub const CONTEXT_PLUGIN_ID: &str = "context";

pub(super) const SUMMARY_MESSAGE_OPEN: &str = "<conversation-summary>";
pub(super) const SUMMARY_MESSAGE_CLOSE: &str = "</conversation-summary>";

fn auto_compact_threshold(max_context_tokens: usize, max_output_tokens: usize) -> usize {
    let available = max_context_tokens.saturating_sub(max_output_tokens);
    available.saturating_mul(7) / 10
}

pub(crate) fn policy_for_model(model: &str) -> ContextWindowPolicy {
    match model {
        m if m.contains("claude") => ContextWindowPolicy {
            max_context_tokens: 200_000,
            max_output_tokens: 16_384,
            enable_prompt_cache: true,
            autocompact_threshold: Some(auto_compact_threshold(200_000, 16_384)),
            compaction_mode: ContextCompactionMode::KeepRecentRawSuffix,
            ..ContextWindowPolicy::default()
        },
        m if m.contains("gpt-4o") => ContextWindowPolicy {
            max_context_tokens: 128_000,
            max_output_tokens: 16_384,
            enable_prompt_cache: false,
            autocompact_threshold: Some(auto_compact_threshold(128_000, 16_384)),
            compaction_mode: ContextCompactionMode::KeepRecentRawSuffix,
            ..ContextWindowPolicy::default()
        },
        m if m.contains("gpt-4") => ContextWindowPolicy {
            max_context_tokens: 128_000,
            max_output_tokens: 4_096,
            enable_prompt_cache: false,
            autocompact_threshold: Some(auto_compact_threshold(128_000, 4_096)),
            compaction_mode: ContextCompactionMode::KeepRecentRawSuffix,
            ..ContextWindowPolicy::default()
        },
        _ => ContextWindowPolicy::default(),
    }
}
