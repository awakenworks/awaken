//! The `compact` config section: when to compact and how much tail to keep.

use serde::{Deserialize, Serialize};

/// Compaction configuration (the `compact` plugin section). A partial section is
/// valid (missing fields fall back to [`CompactConfig::default`]).
///
/// Two trigger modes. When `max_tokens` is set (the model's context window),
/// compaction is **token-aware**: it folds once the estimated context reaches
/// `trigger_ratio` of that window — the "auto-compact at N% of the window"
/// behavior. When `max_tokens` is `None`, it falls back to the message-count
/// `threshold`. Either way, `keep_last` most-recent messages stay verbatim and
/// the main agent's `ContextPolicy::KeepLast` must mirror it.
///
/// Deserialization is **bounds-checked** against [`config_schema`]: an
/// out-of-range value is rejected at load (fail-closed) rather than silently
/// producing a degenerate trigger — a `trigger_ratio` of `0` folds every turn,
/// `> 1` disables the token trigger, and a `threshold` of `0` folds one-message
/// conversations, none of which the schema permits.
#[derive(Debug, Clone, PartialEq, Serialize)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
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

impl<'de> Deserialize<'de> for CompactConfig {
    /// Deserialize with the schema's bounds enforced (fail-closed): missing
    /// fields fall back to [`CompactConfig::default`], then every value is checked
    /// against the same bounds [`config_schema`] declares. An out-of-range knob is
    /// rejected here rather than deserializing clean and producing a degenerate
    /// trigger downstream.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Shadow with identical fields but the derived (unchecked) deserialize,
        // seeded from `CompactConfig::default` for any missing field. Validation
        // runs once on the fully-populated value.
        #[derive(Deserialize)]
        #[serde(default)]
        struct Shadow {
            threshold: usize,
            keep_last: usize,
            max_tokens: Option<u32>,
            trigger_ratio: f64,
            instructions: Option<String>,
        }

        impl Default for Shadow {
            fn default() -> Self {
                let d = CompactConfig::default();
                Self {
                    threshold: d.threshold,
                    keep_last: d.keep_last,
                    max_tokens: d.max_tokens,
                    trigger_ratio: d.trigger_ratio,
                    instructions: d.instructions,
                }
            }
        }

        let s = Shadow::deserialize(deserializer)?;

        // `threshold` minimum 1 (a 0 folds every non-empty conversation).
        if s.threshold < 1 {
            return Err(serde::de::Error::custom(
                "compact.threshold must be >= 1 (0 folds every conversation)",
            ));
        }
        // `trigger_ratio` in (0, 1]: exclusiveMinimum 0 (a 0 budget folds every
        // turn), maximum 1 (a > 1 budget never fires within the real window). Also
        // reject non-finite values, which no fraction-of-window can be.
        if !s.trigger_ratio.is_finite() || s.trigger_ratio <= 0.0 || s.trigger_ratio > 1.0 {
            return Err(serde::de::Error::custom(
                "compact.trigger_ratio must be in (0, 1]",
            ));
        }
        // `max_tokens` minimum 1 when present (a 0-token window is a degenerate
        // budget); `u32` already excludes negatives.
        if matches!(s.max_tokens, Some(0)) {
            return Err(serde::de::Error::custom(
                "compact.max_tokens must be >= 1 when set",
            ));
        }

        Ok(Self {
            threshold: s.threshold,
            keep_last: s.keep_last,
            max_tokens: s.max_tokens,
            trigger_ratio: s.trigger_ratio,
            instructions: s.instructions,
        })
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

    // --- config-bounds fail-closed: deserialization enforces the bounds the
    //     schema declares, rejecting out-of-range knobs at load. ---

    #[test]
    fn trigger_ratio_zero_is_rejected_at_load() {
        // The schema declares `trigger_ratio` exclusiveMinimum 0. A `0.0` budget
        // (`0.0 * max_tokens == 0`) would fold on the very first turn, so it must
        // be rejected at deserialize rather than deserializing clean.
        let err =
            serde_json::from_value::<CompactConfig>(serde_json::json!({ "trigger_ratio": 0.0 }));
        assert!(
            err.is_err(),
            "trigger_ratio 0 must be rejected (exclusiveMinimum 0): {err:?}"
        );
    }

    #[test]
    fn trigger_ratio_above_one_is_rejected_at_load() {
        // The schema declares `trigger_ratio` maximum 1. A `2.0` budget never fires
        // within the real window (compaction silently disabled), so it is rejected.
        let err =
            serde_json::from_value::<CompactConfig>(serde_json::json!({ "trigger_ratio": 2.0 }));
        assert!(
            err.is_err(),
            "trigger_ratio > 1 must be rejected (maximum 1): {err:?}"
        );
    }

    #[test]
    fn threshold_zero_is_rejected_at_load() {
        // The schema declares `threshold` minimum 1. A `0` folds every conversation
        // of even one message, so it is rejected at deserialize.
        let err = serde_json::from_value::<CompactConfig>(serde_json::json!({ "threshold": 0 }));
        assert!(
            err.is_err(),
            "threshold 0 must be rejected (minimum 1): {err:?}"
        );
    }

    #[test]
    fn in_range_bounds_still_deserialize() {
        // The boundary-valid values the schema permits still load: trigger_ratio at
        // the inclusive max, threshold at its minimum, max_tokens at its minimum.
        let cfg: CompactConfig = serde_json::from_value(serde_json::json!({
            "trigger_ratio": 1.0,
            "threshold": 1,
            "max_tokens": 1
        }))
        .expect("boundary-valid config deserializes");
        assert_eq!(cfg.trigger_ratio, 1.0);
        assert_eq!(cfg.threshold, 1);
        assert_eq!(cfg.max_tokens, Some(1));
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
