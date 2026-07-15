//! The `compact` config section: when to compact and how much tail to keep.

use serde::{Deserialize, Serialize};

/// Compaction configuration (the `compact` plugin section). `#[serde(default)]`
/// so a partial section is valid.
///
/// Two trigger modes. When `max_tokens` is set (the model's context window),
/// compaction is **token-aware**: it folds once the estimated context reaches
/// `trigger_ratio` of that window — the "auto-compact at N% of the window"
/// behavior. When `max_tokens` is `None`, it falls back to the message-count
/// `threshold`. Either way, `keep_last` most-recent messages stay verbatim and
/// the main agent's `ContextPolicy::KeepLast` must mirror it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactConfig {
    /// Message-count trigger (fallback when `max_tokens` is unset): compact once
    /// the conversation exceeds this many messages.
    pub threshold: usize,
    /// Keep this many most-recent messages verbatim; the summary covers the rest.
    /// The main agent's `ContextPolicy::KeepLast` must mirror this.
    pub keep_last: usize,
    /// The model's max context window in tokens. `Some` ⇒ token-aware trigger;
    /// `None` ⇒ message-count `threshold`.
    pub max_tokens: Option<u32>,
    /// Fold once the estimated context reaches this fraction of `max_tokens`
    /// (e.g. `0.8` = compact at 80% of the window). Ignored without `max_tokens`.
    pub trigger_ratio: f64,
    /// Optional per-agent compaction prompt: the instruction appended to the older
    /// slice that tells the compactor what to preserve. `None` falls back to the
    /// built-in [`SUMMARIZE_PROMPT`](crate::SUMMARIZE_PROMPT). This is the one knob
    /// that shapes *what* the summary keeps (the thresholds shape *when* it fires).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            threshold: 40,
            keep_last: 8,
            max_tokens: None,
            trigger_ratio: 0.8,
            instructions: None,
        }
    }
}

impl CompactConfig {
    /// The token window that drives the auto-compact trigger, sourced from the model's
    /// intrinsic attributes when the agent didn't pin one: an explicit `max_tokens`
    /// override wins, else the model's `context_window` (its published context window).
    /// So an agent that leaves `max_tokens` unset inherits token-aware compaction from
    /// whatever model it resolves to — the same attribute the ACP CLIs read as their
    /// `CLAUDE_CODE_AUTO_COMPACT_WINDOW`-style window. Returns `None` only when neither
    /// is known (the plugin then falls back to the message-count `threshold`).
    #[must_use]
    pub fn effective_max_tokens(&self, model_context_window: Option<u32>) -> Option<u32> {
        self.max_tokens.or(model_context_window)
    }

    /// Fill `max_tokens` from the model's context window when the agent didn't pin one,
    /// so a config compiled against a resolved model is already token-aware. Idempotent:
    /// an explicit override is never clobbered.
    #[must_use]
    pub fn with_model_context_window(mut self, model_context_window: Option<u32>) -> Self {
        self.max_tokens = self.effective_max_tokens(model_context_window);
        self
    }
}

/// The JSON Schema for the `compact` config section.
pub fn config_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "threshold": {
                "type": "integer", "minimum": 1,
                "description": "Message-count trigger (fallback when max_tokens is unset)."
            },
            "keep_last": {
                "type": "integer", "minimum": 0,
                "description": "Keep this many most-recent messages verbatim; the summary covers the rest."
            },
            "max_tokens": {
                "type": ["integer", "null"], "minimum": 1,
                "description": "The model's context window in tokens; enables the token-aware trigger."
            },
            "trigger_ratio": {
                "type": "number", "exclusiveMinimum": 0, "maximum": 1,
                "description": "Fold once estimated context reaches this fraction of max_tokens (e.g. 0.8)."
            },
            "instructions": {
                "type": ["string", "null"], "format": "textarea",
                "description": "Compaction prompt: what the summary should preserve. Blank uses the built-in default."
            }
        },
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_section_fills_defaults() {
        let cfg: CompactConfig =
            serde_json::from_value(serde_json::json!({ "keep_last": 3 })).unwrap();
        assert_eq!(cfg.keep_last, 3);
        assert_eq!(cfg.threshold, CompactConfig::default().threshold);
    }

    #[test]
    fn config_schema_is_an_object() {
        assert_eq!(config_schema()["type"], "object");
    }

    #[test]
    fn effective_max_tokens_prefers_an_explicit_override_then_the_model_window() {
        // Agent pinned a window → the model attribute never overrides it.
        let pinned = CompactConfig {
            max_tokens: Some(100),
            ..Default::default()
        };
        assert_eq!(pinned.effective_max_tokens(Some(200_000)), Some(100));
        // Agent left it unset → inherit the model's context window.
        let unset = CompactConfig {
            max_tokens: None,
            ..Default::default()
        };
        assert_eq!(unset.effective_max_tokens(Some(200_000)), Some(200_000));
        // Neither known → None (plugin falls back to the message-count threshold).
        assert_eq!(unset.effective_max_tokens(None), None);
    }

    #[test]
    fn with_model_context_window_fills_only_an_unset_window_and_is_idempotent() {
        // Unset → filled from the model; token-aware trigger now active.
        let filled = CompactConfig::default().with_model_context_window(Some(200_000));
        assert_eq!(filled.max_tokens, Some(200_000));
        // Applying again is a no-op (the explicit value now wins).
        assert_eq!(
            filled.clone().with_model_context_window(Some(9)).max_tokens,
            Some(200_000)
        );
        // An agent override survives the model default.
        let pinned = CompactConfig {
            max_tokens: Some(50),
            ..Default::default()
        }
        .with_model_context_window(Some(200_000));
        assert_eq!(pinned.max_tokens, Some(50));
        // No model window and no override → stays count-mode.
        assert_eq!(
            CompactConfig::default()
                .with_model_context_window(None)
                .max_tokens,
            None
        );
    }
}
