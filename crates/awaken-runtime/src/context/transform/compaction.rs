//! Artifact and tool-result compaction: shrink oversized content blocks to previews.

use awaken_runtime_contract::contract::content::ContentBlock;
use awaken_runtime_contract::contract::message::{Message, Role};
use serde::{Deserialize, Serialize};

/// Configuration for compacting oversized tool-result artifacts.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(default)]
pub struct ArtifactCompactionConfig {
    /// Token threshold at or above which a tool result is compacted to a preview.
    #[schemars(range(min = 1))]
    pub threshold_tokens: usize,
    /// Maximum characters retained in a compacted artifact preview.
    #[schemars(range(min = 1))]
    pub preview_max_chars: usize,
    /// Maximum lines retained in a compacted artifact preview.
    #[schemars(range(min = 1))]
    pub preview_max_lines: usize,
}

impl Default for ArtifactCompactionConfig {
    fn default() -> Self {
        Self {
            threshold_tokens: 2048,
            preview_max_chars: 1600,
            preview_max_lines: 24,
        }
    }
}

impl<'de> Deserialize<'de> for ArtifactCompactionConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(default)]
        struct RawArtifactCompactionConfig {
            threshold_tokens: usize,
            preview_max_chars: usize,
            preview_max_lines: usize,
        }

        impl Default for RawArtifactCompactionConfig {
            fn default() -> Self {
                let defaults = ArtifactCompactionConfig::default();
                Self {
                    threshold_tokens: defaults.threshold_tokens,
                    preview_max_chars: defaults.preview_max_chars,
                    preview_max_lines: defaults.preview_max_lines,
                }
            }
        }

        let raw = RawArtifactCompactionConfig::deserialize(deserializer)?;
        let config = Self {
            threshold_tokens: raw.threshold_tokens,
            preview_max_chars: raw.preview_max_chars,
            preview_max_lines: raw.preview_max_lines,
        };
        config.validate().map_err(serde::de::Error::custom)?;
        Ok(config)
    }
}

impl ArtifactCompactionConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.threshold_tokens == 0 {
            return Err("artifact_compaction.threshold_tokens must be >= 1".into());
        }
        if self.preview_max_chars == 0 {
            return Err("artifact_compaction.preview_max_chars must be >= 1".into());
        }
        if self.preview_max_lines == 0 {
            return Err("artifact_compaction.preview_max_lines must be >= 1".into());
        }
        Ok(())
    }
}

/// Compact a single artifact string if it is at or above the token threshold.
///
/// Returns the original content unchanged when estimated tokens are within budget.
/// Otherwise truncates to the default preview character / line limits
/// (whichever is shorter) and appends a compaction indicator.
pub fn compact_artifact(content: &str) -> String {
    compact_artifact_with_config(content, &ArtifactCompactionConfig::default())
}

pub fn compact_artifact_with_config(content: &str, config: &ArtifactCompactionConfig) -> String {
    let estimated_tokens = content.len() / 4;
    if estimated_tokens < config.threshold_tokens {
        return content.to_string();
    }

    // Truncate by line limit first, then by char limit
    let mut char_count = 0usize;
    let mut line_count = 0usize;
    let mut end_byte = 0usize;

    for (idx, ch) in content.char_indices() {
        if char_count >= config.preview_max_chars || line_count >= config.preview_max_lines {
            break;
        }
        if ch == '\n' {
            line_count += 1;
        }
        char_count += 1;
        end_byte = idx + ch.len_utf8();
    }

    let preview = &content[..end_byte];
    format!(
        "{preview}\n\n[Content compacted: original ~{estimated_tokens} tokens, showing first {char_count} chars]"
    )
}

/// Compact tool result messages at or above the artifact token threshold.
///
/// Iterates over all `Role::Tool` messages and replaces oversized text content
/// blocks with a truncated preview plus compaction indicator.
pub fn compact_tool_results(messages: &mut [Message]) {
    compact_tool_results_with_config(messages, &ArtifactCompactionConfig::default());
}

pub fn compact_tool_results_with_config(
    messages: &mut [Message],
    config: &ArtifactCompactionConfig,
) {
    for msg in messages.iter_mut() {
        if msg.role != Role::Tool {
            continue;
        }
        let mut modified = false;
        let new_content: Vec<ContentBlock> = msg
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => {
                    let compacted = compact_artifact_with_config(text, config);
                    if compacted.len() != text.len() {
                        modified = true;
                    }
                    ContentBlock::Text { text: compacted }
                }
                other => other.clone(),
            })
            .collect();
        if modified {
            msg.content = new_content;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::contract::message::ToolCall;
    use serde_json::json;

    #[test]
    fn small_tool_result_not_compacted() {
        let small_content = "x".repeat(100);
        let mut messages = vec![
            Message::user("go"),
            Message::assistant_with_tool_calls("", vec![ToolCall::new("c1", "search", json!({}))]),
            Message::tool("c1", &small_content),
        ];
        compact_tool_results(&mut messages);
        assert_eq!(messages[2].text(), small_content);
    }

    #[test]
    fn large_tool_result_compacted_to_preview() {
        // 2048 tokens * 4 chars/token = 8192 chars reaches the threshold.
        let large_content = "a".repeat(10_000);
        let mut messages = vec![
            Message::user("go"),
            Message::assistant_with_tool_calls(
                "",
                vec![ToolCall::new("c1", "list_files", json!({}))],
            ),
            Message::tool("c1", &large_content),
        ];
        compact_tool_results(&mut messages);

        let result = messages[2].text();
        assert!(
            result.len() < large_content.len(),
            "content should be shorter after compaction"
        );
        assert!(
            result.contains("[Content compacted:"),
            "should contain compaction indicator"
        );
        assert!(result.contains("tokens"), "indicator should mention tokens");
        assert!(result.contains("chars"), "indicator should mention chars");
    }

    #[test]
    fn compact_preserves_non_tool_messages() {
        let large_text = "b".repeat(10_000);
        let mut messages = vec![
            Message::system(&large_text),
            Message::user(&large_text),
            Message::assistant(&large_text),
        ];
        let texts_before: Vec<String> = messages.iter().map(|m| m.text()).collect();
        compact_tool_results(&mut messages);
        let texts_after: Vec<String> = messages.iter().map(|m| m.text()).collect();
        assert_eq!(
            texts_before, texts_after,
            "non-tool messages should be unchanged"
        );
    }

    #[test]
    fn compact_artifact_below_threshold_unchanged() {
        let content = "short content";
        let result = compact_artifact(content);
        assert_eq!(result, content);
    }

    #[test]
    fn compact_artifact_above_threshold_truncates() {
        let content = "x".repeat(10_000);
        let result = compact_artifact(&content);
        assert!(result.len() < content.len());
        assert!(result.contains("[Content compacted:"));
    }

    #[test]
    fn compact_artifact_respects_line_limit() {
        // Create content with many lines above the threshold.
        let content: String = (0..100)
            .map(|i| format!("line {}: {}", i, "x".repeat(200)))
            .collect::<Vec<_>>()
            .join("\n");
        let result = compact_artifact(&content);
        let default_limit = ArtifactCompactionConfig::default().preview_max_lines;
        // Count lines in the preview part (before the compaction indicator)
        let lines_before_indicator = result
            .split("[Content compacted:")
            .next()
            .unwrap_or("")
            .lines()
            .count();
        assert!(
            lines_before_indicator <= default_limit + 1,
            "should respect line limit, got {} lines",
            lines_before_indicator
        );
    }

    #[test]
    fn compact_artifact_uses_custom_config() {
        let config = ArtifactCompactionConfig {
            threshold_tokens: 1,
            preview_max_chars: 8,
            preview_max_lines: 1,
        };
        let result = compact_artifact_with_config("abcdefghijklmnop", &config);
        assert!(result.starts_with("abcdefgh"));
        assert!(result.contains("showing first 8 chars"));
    }

    #[test]
    fn artifact_compaction_config_validate_rejects_zero_values() {
        assert!(
            ArtifactCompactionConfig {
                threshold_tokens: 0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            ArtifactCompactionConfig {
                preview_max_chars: 0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            ArtifactCompactionConfig {
                preview_max_lines: 0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn artifact_compaction_config_serde_rejects_zero_values() {
        let err = serde_json::from_value::<ArtifactCompactionConfig>(json!({
            "threshold_tokens": 0,
            "preview_max_chars": 1600,
            "preview_max_lines": 24
        }))
        .expect_err("serde must enforce runtime minimums");

        assert!(err.to_string().contains("threshold_tokens"));
    }

    #[test]
    fn compact_tool_results_uses_custom_config() {
        let config = ArtifactCompactionConfig {
            threshold_tokens: 1,
            preview_max_chars: 4,
            preview_max_lines: 1,
        };
        let mut messages = vec![Message::tool("c1", "abcdefghijklmnop")];
        compact_tool_results_with_config(&mut messages, &config);
        assert!(messages[0].text().starts_with("abcd"));
        assert!(messages[0].text().contains("[Content compacted:"));
    }

    #[test]
    fn compact_tool_results_multiple_tool_messages() {
        let small = "x".repeat(100);
        let large = "y".repeat(10_000);
        let mut messages = vec![
            Message::user("go"),
            Message::assistant_with_tool_calls(
                "",
                vec![
                    ToolCall::new("c1", "small", json!({})),
                    ToolCall::new("c2", "large", json!({})),
                ],
            ),
            Message::tool("c1", &small),
            Message::tool("c2", &large),
        ];
        compact_tool_results(&mut messages);

        // Small tool result unchanged
        assert_eq!(messages[2].text(), small);
        // Large tool result compacted
        assert!(messages[3].text().len() < large.len());
        assert!(messages[3].text().contains("[Content compacted:"));
    }

    #[test]
    fn compact_artifact_boundary_just_under_threshold() {
        // Exactly at threshold: 2048 * 4 = 8192 chars
        let content = "a".repeat(8191);
        let result = compact_artifact(&content);
        // 8191 / 4 = 2047, which is < 2048 threshold
        assert_eq!(result, content, "just under threshold should not compact");
    }

    #[test]
    fn compact_artifact_boundary_at_threshold() {
        // At threshold: 2048 * 4 = 8192 chars
        let content = "a".repeat(8192);
        let result = compact_artifact(&content);
        // 8192 / 4 = 2048, which is NOT < 2048
        assert!(result.len() < content.len(), "at threshold should compact");
    }
}
