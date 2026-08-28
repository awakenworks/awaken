//! The `compact` config section: when to compact and how much tail to keep.

use serde::{Deserialize, Serialize};

/// Compaction configuration (the `compact` plugin section). A partial section is
/// valid (missing fields fall back to [`CompactConfig::default`]).
///
/// `max_tokens` is the sole executable trigger. Config publication derives and
/// freezes that effective window from typed Agent strategy and model capability;
/// `None` means there is no basis to compact. `keep_last` most-recent messages
/// stay verbatim and a successful fold activates the matching Run-scoped window.
///
/// Deserialization is **bounds-checked** against [`config_schema`]: an
/// out-of-range value is rejected at load (fail-closed) rather than silently
/// producing a degenerate trigger. A zero token window is rejected fail-closed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CompactConfig {
    /// Ordinary published auxiliary Agent. Publishing this id through the normal
    /// Agent API replaces the built-in default; Compact owns no Agent registry.
    pub agent_id: String,
    /// Optional per-main-Agent override for the selected compactor's system
    /// instructions. Absent keeps the selected Agent publication unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_instructions: Option<String>,
    /// Keep this many most-recent messages verbatim; the summary covers the rest.
    /// Applied by the Run-scoped request window only after prefix coverage exists.
    pub keep_last: usize,
    /// The model's max context window in tokens. `Some` ⇒ token-aware trigger;
    /// `None` means publication had no safe trigger basis, so compaction is off.
    pub max_tokens: Option<u32>,
    /// Begin non-blocking background precomputation at this fraction of the hard
    /// frozen window. Must be in `(0, 1)`; cache misses never affect correctness.
    pub prefetch_ratio: f64,
    /// Optional per-agent compaction prompt: the instruction appended to the older
    /// slice that tells the compactor what to preserve. `None` falls back to the
    /// built-in [`SUMMARIZE_PROMPT`](crate::SUMMARIZE_PROMPT). This is the one knob
    /// that shapes *what* the summary keeps (the frozen window shapes *when* it fires).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            agent_id: crate::COMPACT_AGENT_ID.to_string(),
            agent_instructions: None,
            keep_last: 8,
            max_tokens: None,
            prefetch_ratio: 0.75,
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
        #[serde(default, deny_unknown_fields)]
        struct Shadow {
            agent_id: String,
            agent_instructions: Option<String>,
            keep_last: usize,
            max_tokens: Option<u32>,
            prefetch_ratio: f64,
            instructions: Option<String>,
        }

        impl Default for Shadow {
            fn default() -> Self {
                let d = CompactConfig::default();
                Self {
                    agent_id: d.agent_id,
                    agent_instructions: d.agent_instructions,
                    keep_last: d.keep_last,
                    max_tokens: d.max_tokens,
                    prefetch_ratio: d.prefetch_ratio,
                    instructions: d.instructions,
                }
            }
        }

        let s = Shadow::deserialize(deserializer)?;

        if s.agent_id.trim().is_empty() {
            return Err(serde::de::Error::custom(
                "compact.agent_id must not be empty",
            ));
        }

        if !s.prefetch_ratio.is_finite() || s.prefetch_ratio <= 0.0 || s.prefetch_ratio >= 1.0 {
            return Err(serde::de::Error::custom(
                "compact.prefetch_ratio must be in (0, 1)",
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
            agent_id: s.agent_id,
            agent_instructions: s.agent_instructions,
            keep_last: s.keep_last,
            max_tokens: s.max_tokens,
            prefetch_ratio: s.prefetch_ratio,
            instructions: s.instructions,
        })
    }
}

/// The rules the field-level schema can't convey: where the trigger comes from, and that
/// `instructions` is a PROMPT (not a size), so an author — human form or LLM — otherwise
/// mis-models it. Mirrors the state-machine / permission `description`+`examples` pattern.
const COMPACT_AUTHORING_GUIDE: &str = "\
When and how the conversation is compacted (summarized). Authoring rules:\n\
- Trigger: `max_tokens` is the publication-derived effective token window. It is not a \
second authoring control; null means Config had no safe trigger basis and compaction is off.\n\
- `keep_last` most-recent messages always stay verbatim; the summary covers everything \
older. Keep it small (a handful) so compaction actually reclaims context.\n\
- `prefetch_ratio` starts best-effort background summarization before the hard trigger; \
the hard trigger still recovers or awaits the exact stable compactor Run on a miss.\n\
- `instructions` is the COMPACTION PROMPT — free-form English telling the summarizer WHAT \
to preserve (open tasks, decisions, file paths, identifiers), NOT a size or token count. \
Blank uses the built-in default. The frozen `max_tokens` window shapes WHEN it fires; \
`instructions` shapes WHAT survives.";

/// A canonical config: token-aware at 80%, keep a short tail, task-preserving prompt.
fn compact_example() -> serde_json::Value {
    serde_json::json!({
        "agent_id": "team-compactor",
        "keep_last": 8,
        "prefetch_ratio": 0.75,
        "instructions": "Summarize the older messages into a compact briefing. Preserve open \
    tasks, decisions made, and any file paths, identifiers, and commands referenced. Drop \
    resolved chatter and duplicated tool output."
    })
}

/// The JSON Schema for the `compact` config section.
pub fn config_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": COMPACT_AUTHORING_GUIDE,
        "examples": [compact_example()],
        "properties": {
            "agent_id": {
                "type": "string", "minLength": 1,
                "title": "Compaction Agent",
                "description": "Published auxiliary Agent id. Defaults to compactor."
            },
            "agent_instructions": {
                "type": ["string", "null"], "format": "textarea",
                "title": "Compactor system instructions",
                "description": "Per-main-Agent override for the selected compactor's system instructions."
            },
            "keep_last": {
                "type": "integer", "minimum": 0,
                "description": "Keep this many most-recent messages verbatim; the summary covers the rest."
            },
            "max_tokens": {
                "type": ["integer", "null"], "minimum": 1,
                "description": "The model's context window in tokens; enables the token-aware trigger."
            },
            "prefetch_ratio": {
                "type": "number", "exclusiveMinimum": 0, "exclusiveMaximum": 1,
                "description": "Start non-blocking background compaction at this fraction of the frozen token window."
            },
            "instructions": {
                "type": ["string", "null"], "format": "textarea",
                "title": "Compaction instructions",
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
        // Cause/effect rule: C1 a partial parent-Agent compact section -> all
        // missing auxiliary-Agent fields resolve to the stable ordinary default.
        let cfg: CompactConfig =
            serde_json::from_value(serde_json::json!({ "keep_last": 3 })).unwrap();
        assert_eq!(cfg.keep_last, 3);
        assert_eq!(cfg.max_tokens, None);
        assert_eq!(cfg.agent_id, crate::COMPACT_AGENT_ID);
    }

    #[test]
    fn auxiliary_agent_selection_and_prompt_are_validated_and_round_trip() {
        // Cause/effect decision table: C1 nonblank ordinary Agent id + online
        // system prompt -> E1 preserve both; C2 blank id -> E2 reject before a
        // Compact request can select an ambiguous fallback.
        let config: CompactConfig = serde_json::from_value(serde_json::json!({
            "agent_id": "team-compactor",
            "agent_instructions": "Preserve open decisions and exact paths."
        }))
        .expect("R1");
        assert_eq!(config.agent_id, "team-compactor", "R1");
        assert_eq!(
            config.agent_instructions.as_deref(),
            Some("Preserve open decisions and exact paths."),
            "R1"
        );
        assert!(
            serde_json::from_value::<CompactConfig>(serde_json::json!({"agent_id": " "})).is_err(),
            "R2"
        );
    }

    #[test]
    fn config_schema_is_an_object() {
        assert_eq!(config_schema()["type"], "object");
    }

    // --- config-bounds fail-closed: deserialization enforces the bounds the
    //     schema declares, rejecting out-of-range knobs at load. ---

    #[test]
    fn prefetch_ratio_must_be_strictly_between_zero_and_one() {
        for invalid in [0.0, 1.0, -0.1] {
            assert!(
                serde_json::from_value::<CompactConfig>(
                    serde_json::json!({ "prefetch_ratio": invalid })
                )
                .is_err(),
                "{invalid} must be rejected"
            );
        }
        assert!(
            serde_json::from_value::<CompactConfig>(serde_json::json!({ "prefetch_ratio": 0.5 }))
                .is_ok()
        );
    }

    #[test]
    fn minimum_token_window_still_deserializes() {
        // Cause/effect boundary: C1 the sole token trigger is its minimum valid
        // value; E1 it loads exactly rather than being mistaken for disabled.
        let cfg: CompactConfig = serde_json::from_value(serde_json::json!({
            "max_tokens": 1
        }))
        .expect("boundary-valid config deserializes");
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

    #[test]
    fn config_schema_carries_authoring_guidance() {
        let schema = config_schema();
        // The when-vs-what distinction (instructions is a prompt, not a size) must ride on
        // the schema so an author reading it doesn't mis-model `instructions`.
        let desc = schema["description"].as_str().unwrap();
        assert!(
            desc.contains("COMPACTION PROMPT"),
            "guide frames instructions as a prompt"
        );
        // The worked example parses back into a valid config (instructions is a string prompt).
        let example = &schema["examples"][0];
        let cfg: CompactConfig = serde_json::from_value(example.clone()).unwrap();
        assert!(cfg.instructions.is_some_and(|s| s.len() > 20));
        assert_eq!(cfg.keep_last, 8);
    }
}
