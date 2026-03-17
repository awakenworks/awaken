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

/// Reasoning effort hint, independent of any LLM provider SDK.
///
/// Maps to provider-specific parameters at the genai adapter layer
/// (e.g. Anthropic `thinking.budget_tokens`, OpenAI reasoning effort).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    None,
    Low,
    Medium,
    High,
    Max,
    /// Explicit token budget for reasoning/thinking.
    Budget(u32),
}

/// Unified per-inference override for model selection and inference parameters.
///
/// All fields are `Option` — `None` means "use the agent-level default".
/// Multiple plugins can emit `BeforeInferenceAction::OverrideInference`;
/// fields are merged with last-wins semantics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InferenceOverride {
    /// Primary model identifier.
    pub model: Option<String>,
    /// Fallback model identifiers.
    pub fallback_models: Option<Vec<String>>,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Maximum output tokens.
    pub max_tokens: Option<u32>,
    /// Nucleus sampling (top-p).
    pub top_p: Option<f64>,
    /// Reasoning effort hint.
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl InferenceOverride {
    /// Merge `other` into `self` with last-wins semantics per field.
    pub fn merge(&mut self, other: InferenceOverride) {
        if other.model.is_some() {
            self.model = other.model;
        }
        if other.fallback_models.is_some() {
            self.fallback_models = other.fallback_models;
        }
        if other.temperature.is_some() {
            self.temperature = other.temperature;
        }
        if other.max_tokens.is_some() {
            self.max_tokens = other.max_tokens;
        }
        if other.top_p.is_some() {
            self.top_p = other.top_p;
        }
        if other.reasoning_effort.is_some() {
            self.reasoning_effort = other.reasoning_effort;
        }
    }
}

impl From<InferenceModelOverride> for InferenceOverride {
    fn from(m: InferenceModelOverride) -> Self {
        Self {
            model: Some(m.model),
            fallback_models: if m.fallback_models.is_empty() {
                None
            } else {
                Some(m.fallback_models)
            },
            ..Default::default()
        }
    }
}

/// Inference-phase extension: system/session context and tool descriptors.
///
/// Populated by `AddSystemContext`, `AddSessionContext`, `ExcludeTool`,
/// `IncludeOnlyTools`, `AddRequestTransform`, `OverrideModel`,
/// `OverrideInference` actions during `BeforeInference`.
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
    /// Unified inference override set by plugins via `OverrideModel` or
    /// `OverrideInference`. When `Some`, the loop runner uses these values
    /// instead of the agent's defaults.
    pub inference_override: Option<InferenceOverride>,
}

impl std::fmt::Debug for InferenceContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferenceContext")
            .field("system_context", &self.system_context)
            .field("session_context", &self.session_context)
            .field("tools", &self.tools)
            .field("request_transforms", &self.request_transforms.len())
            .field("inference_override", &self.inference_override)
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

    #[test]
    fn inference_override_merge_last_wins() {
        let mut base = InferenceOverride {
            model: Some("model-a".into()),
            temperature: Some(0.5),
            ..Default::default()
        };
        base.merge(InferenceOverride {
            model: Some("model-b".into()),
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        });
        assert_eq!(base.model.as_deref(), Some("model-b"));
        assert_eq!(base.temperature, Some(0.5)); // not overwritten
        assert_eq!(base.reasoning_effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn inference_override_merge_none_preserves_existing() {
        let mut base = InferenceOverride {
            max_tokens: Some(1024),
            top_p: Some(0.9),
            ..Default::default()
        };
        base.merge(InferenceOverride::default());
        assert_eq!(base.max_tokens, Some(1024));
        assert_eq!(base.top_p, Some(0.9));
    }

    #[test]
    fn from_model_override_converts_correctly() {
        let model_ovr = InferenceModelOverride {
            model: "claude-sonnet".into(),
            fallback_models: vec!["claude-haiku".into()],
        };
        let ovr: InferenceOverride = model_ovr.into();
        assert_eq!(ovr.model.as_deref(), Some("claude-sonnet"));
        assert_eq!(ovr.fallback_models, Some(vec!["claude-haiku".into()]));
        assert!(ovr.temperature.is_none());
    }

    #[test]
    fn from_model_override_empty_fallbacks() {
        let model_ovr = InferenceModelOverride {
            model: "gpt-4o".into(),
            fallback_models: vec![],
        };
        let ovr: InferenceOverride = model_ovr.into();
        assert!(ovr.fallback_models.is_none());
    }

    #[test]
    fn reasoning_effort_serde_roundtrip() {
        let effort = ReasoningEffort::Budget(4096);
        let json = serde_json::to_string(&effort).unwrap();
        let restored: ReasoningEffort = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, effort);
    }
}
