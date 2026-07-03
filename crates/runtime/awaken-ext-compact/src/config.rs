//! The `compact` config section: when to compact and how much tail to keep.

use serde::{Deserialize, Serialize};

/// Compaction configuration (the `compact` plugin section). `#[serde(default)]`
/// so a partial section is valid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactConfig {
    /// Compact once the conversation exceeds this many messages.
    pub threshold: usize,
    /// Keep this many most-recent messages verbatim; the summary covers the rest.
    /// The main agent's `ContextPolicy::KeepLast` must mirror this.
    pub keep_last: usize,
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            threshold: 40,
            keep_last: 8,
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
                "description": "Compact once the conversation exceeds this many messages."
            },
            "keep_last": {
                "type": "integer", "minimum": 0,
                "description": "Keep this many most-recent messages verbatim; the summary covers the rest."
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
