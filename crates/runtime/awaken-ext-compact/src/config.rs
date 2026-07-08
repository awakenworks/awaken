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
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            threshold: 40,
            keep_last: 8,
            max_tokens: None,
            trigger_ratio: 0.8,
        }
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
}
