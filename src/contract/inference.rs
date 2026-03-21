//! Inference response types and override configuration.

use super::message::ToolCall;
use serde::{Deserialize, Serialize};

/// Why the LLM stopped generating output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StopReason {
    /// Model finished naturally.
    EndTurn,
    /// Output hit the `max_tokens` limit.
    MaxTokens,
    /// Model emitted one or more tool-use calls.
    ToolUse,
    /// A stop sequence was matched.
    StopSequence,
}

/// Provider-neutral token usage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_tokens: Option<i32>,
}

/// Result of stream collection used by runtime and plugin phase contracts.
#[derive(Debug, Clone)]
pub struct StreamResult {
    /// Accumulated text content.
    pub text: String,
    /// Collected tool calls.
    pub tool_calls: Vec<ToolCall>,
    /// Token usage from the LLM response.
    pub usage: Option<TokenUsage>,
    /// Why the model stopped generating.
    pub stop_reason: Option<StopReason>,
}

impl StreamResult {
    /// Check if tool execution is needed.
    pub fn needs_tools(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

/// Inference error emitted by the loop and consumed by telemetry plugins.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InferenceError {
    /// Stable error class used for metrics/telemetry dimensions.
    #[serde(rename = "type")]
    pub error_type: String,
    /// Human-readable error message.
    pub message: String,
    /// Classified error category (e.g. `rate_limit`, `timeout`, `connection`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_class: Option<String>,
}

/// LLM response: success with a [`StreamResult`] or failure with an [`InferenceError`].
#[derive(Debug, Clone)]
pub struct LLMResponse {
    pub outcome: Result<StreamResult, InferenceError>,
}

impl LLMResponse {
    pub fn success(result: StreamResult) -> Self {
        Self {
            outcome: Ok(result),
        }
    }

    pub fn error(error: InferenceError) -> Self {
        Self {
            outcome: Err(error),
        }
    }
}

// ---------------------------------------------------------------------------
// Inference override / context types
// ---------------------------------------------------------------------------

/// Context window management policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextWindowPolicy {
    /// Model's total context window size in tokens.
    pub max_context_tokens: usize,
    /// Tokens reserved for model output.
    pub max_output_tokens: usize,
    /// Minimum number of recent messages to always preserve.
    pub min_recent_messages: usize,
    /// Whether to enable prompt caching.
    pub enable_prompt_cache: bool,
    /// Token count threshold that triggers auto-compaction. `None` disables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autocompact_threshold: Option<usize>,
    /// Auto-compaction strategy.
    #[serde(default)]
    pub compaction_mode: ContextCompactionMode,
    /// Number of recent raw messages to preserve in suffix compaction mode.
    #[serde(default = "default_compaction_raw_suffix_messages")]
    pub compaction_raw_suffix_messages: usize,
}

const fn default_compaction_raw_suffix_messages() -> usize {
    2
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

/// Auto-compaction strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ContextCompactionMode {
    #[default]
    KeepRecentRawSuffix,
    CompactToSafeFrontier,
}

/// Override for model selection during inference.
#[derive(Debug, Clone)]
pub struct InferenceModelOverride {
    /// Primary model identifier.
    pub model: String,
    /// Fallback model identifiers.
    pub fallback_models: Vec<String>,
}

/// Reasoning effort hint, independent of any LLM provider SDK.
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
/// Multiple plugins can emit overrides; fields are merged with last-wins semantics.
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
        assert_eq!(base.temperature, Some(0.5));
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

    #[test]
    fn stream_result_needs_tools() {
        let with_tools = StreamResult {
            text: String::new(),
            tool_calls: vec![ToolCall::new("c1", "search", json!({}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
        };
        assert!(with_tools.needs_tools());

        let without = StreamResult {
            text: "hello".into(),
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
        };
        assert!(!without.needs_tools());
    }

    #[test]
    fn llm_response_success_and_error() {
        let success = LLMResponse::success(StreamResult {
            text: "hi".into(),
            tool_calls: vec![],
            usage: Some(TokenUsage {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                total_tokens: Some(15),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::EndTurn),
        });
        assert!(success.outcome.is_ok());

        let error = LLMResponse::error(InferenceError {
            error_type: "rate_limit".into(),
            message: "too many requests".into(),
            error_class: Some("rate_limit".into()),
        });
        assert!(error.outcome.is_err());
    }

    #[test]
    fn token_usage_serde_omits_none_fields() {
        let usage = TokenUsage {
            prompt_tokens: Some(100),
            ..Default::default()
        };
        let json = serde_json::to_string(&usage).unwrap();
        assert!(json.contains("prompt_tokens"));
        assert!(!json.contains("completion_tokens"));
    }

    #[test]
    fn inference_error_serde_roundtrip() {
        let err = InferenceError {
            error_type: "timeout".into(),
            message: "request timed out".into(),
            error_class: Some("connection".into()),
        };
        let json = serde_json::to_string(&err).unwrap();
        let parsed: InferenceError = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, err);
    }
}
