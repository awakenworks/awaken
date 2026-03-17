use crate::runtime::inference::transform::InferenceRequestTransform;
use crate::runtime::tool_call::ToolDescriptor;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Auto-compaction strategy used by [`ContextPlugin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ContextCompactionMode {
    /// Replace an older prefix with a summary while preserving a recent raw suffix.
    #[default]
    KeepRecentRawSuffix,
    /// Replace all messages through the latest safe frontier with a summary.
    CompactToSafeFrontier,
}

const fn default_compaction_raw_suffix_messages() -> usize {
    2
}

/// Context window management policy.
///
/// Pure data struct used by [`ContextPlugin`] to configure context
/// management. Lives in the contract layer so plugin crates can construct
/// it without depending on `tirea-agentos`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextWindowPolicy {
    /// Model's total context window size in tokens.
    pub max_context_tokens: usize,
    /// Tokens reserved for model output.
    pub max_output_tokens: usize,
    /// Minimum number of recent messages to always preserve (never truncated).
    pub min_recent_messages: usize,
    /// Whether to enable prompt caching (Anthropic `cache_control: ephemeral`).
    pub enable_prompt_cache: bool,
    /// Token count threshold that triggers auto-compaction. `None` disables.
    /// Used by ContextPlugin to decide when to compact history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autocompact_threshold: Option<usize>,
    /// Auto-compaction strategy to use when `autocompact_threshold` is reached.
    #[serde(default)]
    pub compaction_mode: ContextCompactionMode,
    /// Number of recent raw messages to preserve in
    /// [`ContextCompactionMode::KeepRecentRawSuffix`] mode.
    #[serde(default = "default_compaction_raw_suffix_messages")]
    pub compaction_raw_suffix_messages: usize,
}

impl Default for ContextWindowPolicy {
    fn default() -> Self {
        Self {
            max_context_tokens: 200_000,
            max_output_tokens: 16_384,
            min_recent_messages: 10,
            enable_prompt_cache: true,
            autocompact_threshold: None,
            compaction_mode: ContextCompactionMode::KeepRecentRawSuffix,
            compaction_raw_suffix_messages: default_compaction_raw_suffix_messages(),
        }
    }
}

/// Override for model selection during inference.
///
/// When set via `BeforeInferenceAction::OverrideModel`, the loop runner
/// uses these values instead of the base agent's model and fallback models.
#[derive(Debug, Clone)]
pub struct InferenceModelOverride {
    /// Primary model identifier to use for this inference call.
    pub model: String,
    /// Fallback model identifiers (tried in order if the primary fails).
    pub fallback_models: Vec<String>,
}

/// Inference-phase extension: system/session context and tool descriptors.
///
/// Populated by `AddSystemContext`, `AddSessionContext`, `ExcludeTool`,
/// `IncludeOnlyTools`, `AddRequestTransform`, `OverrideModel` actions
/// during `BeforeInference`.
#[derive(Default, Clone)]
pub struct InferenceContext {
    /// System context lines appended to the system prompt.
    pub system_context: Vec<String>,
    /// Session context messages injected before user messages.
    pub session_context: Vec<String>,
    /// Available tool descriptors (can be filtered by actions).
    pub tools: Vec<ToolDescriptor>,
    /// Request transforms registered by plugins. Applied in order after
    /// messages are assembled, before the request is sent to the LLM.
    pub request_transforms: Vec<Arc<dyn InferenceRequestTransform>>,
    /// Model override set by a `BeforeInferenceAction::OverrideModel` action.
    /// When `Some`, the loop runner uses this instead of `agent.model()`.
    pub model_override: Option<InferenceModelOverride>,
}

impl std::fmt::Debug for InferenceContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferenceContext")
            .field("system_context", &self.system_context)
            .field("session_context", &self.session_context)
            .field("tools", &self.tools)
            .field("request_transforms", &self.request_transforms.len())
            .field("model_override", &self.model_override)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_policy_uses_suffix_compaction_defaults() {
        let policy = ContextWindowPolicy::default();
        assert_eq!(
            policy.compaction_mode,
            ContextCompactionMode::KeepRecentRawSuffix
        );
        assert_eq!(policy.compaction_raw_suffix_messages, 2);
    }

    #[test]
    fn policy_deserialization_backfills_new_compaction_fields() {
        let value = json!({
            "max_context_tokens": 4096,
            "max_output_tokens": 512,
            "min_recent_messages": 4,
            "enable_prompt_cache": false,
            "autocompact_threshold": 2048
        });

        let policy: ContextWindowPolicy = serde_json::from_value(value).unwrap();
        assert_eq!(
            policy.compaction_mode,
            ContextCompactionMode::KeepRecentRawSuffix
        );
        assert_eq!(policy.compaction_raw_suffix_messages, 2);
    }

    #[test]
    fn policy_serialization_roundtrip_preserves_frontier_mode() {
        let policy = ContextWindowPolicy {
            max_context_tokens: 8192,
            max_output_tokens: 1024,
            min_recent_messages: 6,
            enable_prompt_cache: false,
            autocompact_threshold: Some(4096),
            compaction_mode: ContextCompactionMode::CompactToSafeFrontier,
            compaction_raw_suffix_messages: 5,
        };

        let encoded = serde_json::to_value(&policy).unwrap();
        assert_eq!(encoded["compaction_mode"], "compact_to_safe_frontier");
        assert_eq!(encoded["compaction_raw_suffix_messages"], 5);

        let restored: ContextWindowPolicy = serde_json::from_value(encoded).unwrap();
        assert_eq!(
            restored.compaction_mode,
            ContextCompactionMode::CompactToSafeFrontier
        );
        assert_eq!(restored.compaction_raw_suffix_messages, 5);
    }
}
