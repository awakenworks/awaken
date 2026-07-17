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

    // --- config-bounds fail-open: the schema declares bounds that deserialization
    //     does NOT enforce. These pin the CURRENT (unvalidated) trigger behavior. ---

    #[test]
    fn trigger_ratio_zero_deserializes_and_folds_immediately() {
        // KNOWN BUG (adjudicate): the schema declares `trigger_ratio` exclusiveMinimum 0,
        // but serde enforces no lower bound — `0.0` deserializes clean. With a token
        // budget of `0.0 * max_tokens == 0`, `est_tokens >= 0` is always true, so the
        // token trigger fires on the very first turn (even ~0 estimated tokens).
        let cfg: CompactConfig =
            serde_json::from_value(serde_json::json!({ "trigger_ratio": 0.0, "max_tokens": 1000 }))
                .unwrap();
        assert_eq!(
            cfg.trigger_ratio, 0.0,
            "no lower-bound validation on deserialize"
        );
        // Pin: a zero-token conversation still folds (all but keep_last).
        assert_eq!(
            crate::fold::token_fold_point(0, 1000, cfg.trigger_ratio, 10, 2),
            Some(8),
            "trigger_ratio 0 folds immediately — fail-open"
        );
    }

    #[test]
    fn trigger_ratio_above_one_deserializes_and_disables_the_trigger() {
        // KNOWN BUG (adjudicate): the schema declares `trigger_ratio` maximum 1, but
        // `2.0` deserializes clean. The budget becomes `2.0 * max_tokens`, so the
        // trigger never fires within the real window — compaction is silently disabled.
        let cfg: CompactConfig =
            serde_json::from_value(serde_json::json!({ "trigger_ratio": 2.0, "max_tokens": 1000 }))
                .unwrap();
        assert_eq!(
            cfg.trigger_ratio, 2.0,
            "no upper-bound validation on deserialize"
        );
        // Pin: even a context at the full window (1000) does not fold, because the
        // budget is 2000. A valid ratio (<= 1) would have folded here.
        assert_eq!(
            crate::fold::token_fold_point(1000, 1000, cfg.trigger_ratio, 10, 2),
            None,
            "trigger_ratio > 1 never fires — fail-open"
        );
    }

    #[test]
    fn threshold_zero_deserializes_and_folds_every_nonempty_conversation() {
        // KNOWN BUG (adjudicate): the schema declares `threshold` minimum 1, but `0`
        // deserializes clean. `fold_point` triggers on `committed_len > threshold`, so
        // a threshold of 0 folds every conversation of even one message.
        let cfg: CompactConfig =
            serde_json::from_value(serde_json::json!({ "threshold": 0 })).unwrap();
        assert_eq!(cfg.threshold, 0, "no minimum validation on deserialize");
        // Pin: a single-message conversation with keep_last 0 folds its only message.
        assert_eq!(
            crate::fold::fold_point(1, cfg.threshold, 0),
            Some(1),
            "threshold 0 folds a one-message conversation — fail-open"
        );
    }

    // --- Serialize round-trip incl. `skip_serializing_if` on `instructions` ---

    #[test]
    fn serialize_round_trip_skips_none_instructions_and_emits_some() {
        // Default: `instructions` is None → the field is omitted entirely.
        let cfg = CompactConfig::default();
        let value = serde_json::to_value(&cfg).unwrap();
        assert!(
            value.get("instructions").is_none(),
            "None instructions must be skipped, not serialized as null: {value}"
        );
        assert_eq!(value["threshold"], 40);
        assert_eq!(value["trigger_ratio"], 0.8);
        // `max_tokens` has no skip, so it round-trips as an explicit null.
        assert!(value["max_tokens"].is_null());
        let back: CompactConfig = serde_json::from_value(value).unwrap();
        assert_eq!(back, cfg);

        // Some: the field is present verbatim and round-trips.
        let tuned = CompactConfig {
            instructions: Some("Keep only API endpoints.".to_string()),
            max_tokens: Some(2000),
            ..Default::default()
        };
        let value = serde_json::to_value(&tuned).unwrap();
        assert_eq!(value["instructions"], "Keep only API endpoints.");
        assert_eq!(value["max_tokens"], 2000);
        let back: CompactConfig = serde_json::from_value(value).unwrap();
        assert_eq!(back, tuned);
    }
}
