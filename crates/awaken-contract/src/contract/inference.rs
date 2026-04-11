//! Inference response types and override configuration.

use super::content::{ContentBlock, extract_text};
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
    /// Content blocks from the LLM response.
    pub content: Vec<ContentBlock>,
    /// Collected tool calls.
    pub tool_calls: Vec<ToolCall>,
    /// Token usage from the LLM response.
    pub usage: Option<TokenUsage>,
    /// Why the model stopped generating.
    pub stop_reason: Option<StopReason>,
    /// True when tool calls were started but their argument JSON was truncated
    /// (e.g. the response hit `MaxTokens` mid-generation).
    pub has_incomplete_tool_calls: bool,
}

impl StreamResult {
    /// Check if tool execution is needed.
    pub fn needs_tools(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    /// Extract concatenated text from content blocks.
    pub fn text(&self) -> String {
        extract_text(&self.content)
    }

    /// Whether this result was truncated mid-generation and should be recovered
    /// via a continuation prompt.
    pub fn needs_truncation_recovery(&self) -> bool {
        self.stop_reason == Some(StopReason::MaxTokens)
            && (self.has_incomplete_tool_calls
                || (self.tool_calls.is_empty() && self.has_text_output()))
    }

    fn has_text_output(&self) -> bool {
        self.content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text } if !text.is_empty()))
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// Returns true if all fields are `None` (no override set).
    pub fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.fallback_models.is_none()
            && self.temperature.is_none()
            && self.max_tokens.is_none()
            && self.top_p.is_none()
            && self.reasoning_effort.is_none()
    }

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
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "search", json!({}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        };
        assert!(with_tools.needs_tools());

        let without = StreamResult {
            content: vec![ContentBlock::text("hello")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        };
        assert!(!without.needs_tools());
    }

    #[test]
    fn llm_response_success_and_error() {
        let success = LLMResponse::success(StreamResult {
            content: vec![ContentBlock::text("hi")],
            tool_calls: vec![],
            usage: Some(TokenUsage {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                total_tokens: Some(15),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
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

    // ── StreamResult tests (migrated from uncarve) ──

    #[test]
    fn stream_result_text_concatenates() {
        let result = StreamResult {
            content: vec![
                ContentBlock::text("Hello "),
                ContentBlock::image_url("img.png"),
                ContentBlock::text("World"),
            ],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        };
        assert_eq!(result.text(), "Hello World");
    }

    #[test]
    fn stream_result_text_empty_no_text_blocks() {
        let result = StreamResult {
            content: vec![ContentBlock::image_url("img.png")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        };
        assert_eq!(result.text(), "");
    }

    #[test]
    fn stream_result_needs_truncation_recovery_true() {
        let result = StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "search", json!({}))],
            usage: None,
            stop_reason: Some(StopReason::MaxTokens),
            has_incomplete_tool_calls: true,
        };
        assert!(result.needs_truncation_recovery());
    }

    #[test]
    fn stream_result_needs_truncation_recovery_for_text_max_tokens() {
        let result = StreamResult {
            content: vec![ContentBlock::text("partial answer")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::MaxTokens),
            has_incomplete_tool_calls: false,
        };
        assert!(result.needs_truncation_recovery());
    }

    #[test]
    fn stream_result_no_truncation_recovery_end_turn() {
        let result = StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "search", json!({}))],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: true,
        };
        assert!(!result.needs_truncation_recovery());
    }

    #[test]
    fn stream_result_no_truncation_recovery_complete_tools() {
        let result = StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "search", json!({}))],
            usage: None,
            stop_reason: Some(StopReason::MaxTokens),
            has_incomplete_tool_calls: false,
        };
        assert!(!result.needs_truncation_recovery());
    }

    #[test]
    fn stream_result_no_truncation_recovery_empty_max_tokens() {
        let result = StreamResult {
            content: vec![],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::MaxTokens),
            has_incomplete_tool_calls: false,
        };
        assert!(!result.needs_truncation_recovery());
    }

    // ── StopReason tests (migrated from uncarve) ──

    #[test]
    fn stop_reason_serde_roundtrip_all_variants() {
        for reason in [
            StopReason::EndTurn,
            StopReason::MaxTokens,
            StopReason::ToolUse,
            StopReason::StopSequence,
        ] {
            let json = serde_json::to_string(&reason).unwrap();
            let parsed: StopReason = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, reason);
        }
    }

    #[test]
    fn stop_reason_debug_output() {
        assert_eq!(format!("{:?}", StopReason::EndTurn), "EndTurn");
        assert_eq!(format!("{:?}", StopReason::MaxTokens), "MaxTokens");
        assert_eq!(format!("{:?}", StopReason::ToolUse), "ToolUse");
        assert_eq!(format!("{:?}", StopReason::StopSequence), "StopSequence");
    }

    // ── InferenceOverride.is_empty tests (migrated from uncarve) ──

    #[test]
    fn inference_override_is_empty_default() {
        assert!(InferenceOverride::default().is_empty());
    }

    #[test]
    fn inference_override_not_empty_with_model() {
        let ovr = InferenceOverride {
            model: Some("model-a".into()),
            ..Default::default()
        };
        assert!(!ovr.is_empty());
    }

    #[test]
    fn inference_override_not_empty_with_temperature() {
        let ovr = InferenceOverride {
            temperature: Some(0.5),
            ..Default::default()
        };
        assert!(!ovr.is_empty());
    }

    #[test]
    fn inference_override_not_empty_with_max_tokens() {
        let ovr = InferenceOverride {
            max_tokens: Some(1024),
            ..Default::default()
        };
        assert!(!ovr.is_empty());
    }

    #[test]
    fn inference_override_not_empty_with_reasoning_effort() {
        let ovr = InferenceOverride {
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        assert!(!ovr.is_empty());
    }

    #[test]
    fn inference_override_not_empty_with_top_p() {
        let ovr = InferenceOverride {
            top_p: Some(0.9),
            ..Default::default()
        };
        assert!(!ovr.is_empty());
    }

    #[test]
    fn inference_override_not_empty_with_fallback_models() {
        let ovr = InferenceOverride {
            fallback_models: Some(vec!["m1".into()]),
            ..Default::default()
        };
        assert!(!ovr.is_empty());
    }

    // ── TokenUsage tests (migrated from uncarve) ──

    #[test]
    fn token_usage_full_serde_roundtrip() {
        let usage = TokenUsage {
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            total_tokens: Some(150),
            cache_read_tokens: Some(30),
            cache_creation_tokens: Some(10),
            thinking_tokens: Some(5),
        };
        let json = serde_json::to_string(&usage).unwrap();
        let parsed: TokenUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, usage);
    }

    #[test]
    fn token_usage_default_is_all_none() {
        let usage = TokenUsage::default();
        assert!(usage.prompt_tokens.is_none());
        assert!(usage.completion_tokens.is_none());
        assert!(usage.total_tokens.is_none());
        assert!(usage.cache_read_tokens.is_none());
        assert!(usage.cache_creation_tokens.is_none());
        assert!(usage.thinking_tokens.is_none());
    }

    #[test]
    fn token_usage_default_serializes_to_empty_object() {
        let usage = TokenUsage::default();
        let json = serde_json::to_string(&usage).unwrap();
        assert_eq!(json, "{}");
    }

    // ── ReasoningEffort tests (migrated from uncarve) ──

    #[test]
    fn reasoning_effort_all_variants_serde_roundtrip() {
        for effort in [
            ReasoningEffort::None,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Max,
            ReasoningEffort::Budget(2048),
        ] {
            let json = serde_json::to_string(&effort).unwrap();
            let parsed: ReasoningEffort = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, effort);
        }
    }

    // ── ContextCompactionMode tests (migrated from uncarve) ──

    #[test]
    fn context_compaction_mode_serde_roundtrip() {
        for mode in [
            ContextCompactionMode::KeepRecentRawSuffix,
            ContextCompactionMode::CompactToSafeFrontier,
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            let parsed: ContextCompactionMode = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, mode);
        }
    }

    #[test]
    fn context_compaction_mode_default_is_keep_recent() {
        assert_eq!(
            ContextCompactionMode::default(),
            ContextCompactionMode::KeepRecentRawSuffix
        );
    }

    #[test]
    fn context_compaction_mode_serialization_values() {
        assert_eq!(
            serde_json::to_string(&ContextCompactionMode::KeepRecentRawSuffix).unwrap(),
            "\"keep_recent_raw_suffix\""
        );
        assert_eq!(
            serde_json::to_string(&ContextCompactionMode::CompactToSafeFrontier).unwrap(),
            "\"compact_to_safe_frontier\""
        );
    }

    // ── InferenceError tests (migrated from uncarve) ──

    #[test]
    fn inference_error_omits_none_error_class() {
        let err = InferenceError {
            error_type: "timeout".into(),
            message: "timed out".into(),
            error_class: None,
        };
        let json = serde_json::to_string(&err).unwrap();
        assert!(!json.contains("error_class"));
    }

    #[test]
    fn inference_error_type_field_serializes_as_type() {
        let err = InferenceError {
            error_type: "rate_limit".into(),
            message: "too many requests".into(),
            error_class: None,
        };
        let json_val: serde_json::Value = serde_json::to_value(&err).unwrap();
        assert_eq!(json_val["type"], "rate_limit");
        assert!(json_val.get("error_type").is_none());
    }

    // ── ContextWindowPolicy tests (migrated from uncarve) ──

    #[test]
    fn context_window_policy_default_values() {
        let policy = ContextWindowPolicy::default();
        assert_eq!(policy.max_context_tokens, 200_000);
        assert_eq!(policy.max_output_tokens, 16_384);
        assert_eq!(policy.min_recent_messages, 10);
        assert!(policy.enable_prompt_cache);
        assert!(policy.autocompact_threshold.is_none());
    }

    #[test]
    fn context_window_policy_full_roundtrip() {
        let policy = ContextWindowPolicy {
            max_context_tokens: 4096,
            max_output_tokens: 512,
            min_recent_messages: 4,
            enable_prompt_cache: false,
            autocompact_threshold: Some(2048),
            compaction_mode: ContextCompactionMode::CompactToSafeFrontier,
            compaction_raw_suffix_messages: 3,
        };
        let json = serde_json::to_value(&policy).unwrap();
        let restored: ContextWindowPolicy = serde_json::from_value(json).unwrap();
        assert_eq!(restored.max_context_tokens, 4096);
        assert_eq!(
            restored.compaction_mode,
            ContextCompactionMode::CompactToSafeFrontier
        );
        assert_eq!(restored.compaction_raw_suffix_messages, 3);
    }

    // ── InferenceOverride merge tests (migrated from uncarve) ──

    #[test]
    fn inference_override_merge_fallback_models() {
        let mut base = InferenceOverride::default();
        base.merge(InferenceOverride {
            fallback_models: Some(vec!["model-a".into(), "model-b".into()]),
            ..Default::default()
        });
        assert_eq!(
            base.fallback_models,
            Some(vec!["model-a".into(), "model-b".into()])
        );
    }

    #[test]
    fn inference_override_merge_top_p() {
        let mut base = InferenceOverride {
            top_p: Some(0.9),
            ..Default::default()
        };
        base.merge(InferenceOverride {
            top_p: Some(0.7),
            ..Default::default()
        });
        assert_eq!(base.top_p, Some(0.7));
    }
}
