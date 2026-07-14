//! Free helpers shared by the [`ManagedState`] event path: rubric normalization,
//! usage projection, and content-block text extraction.

use super::*;

/// Normalize a Managed rubric (a bare string or `{type:"text",content}`) to text.
pub(crate) fn rubric_text(rubric: &serde_json::Value) -> String {
    match rubric {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) => map
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// The session's `usage` object (`BetaManagedAgentsSessionUsage`): cumulative input +
/// output (+ prompt-cache) token counts across all turns. Emitted whenever a turn ran.
pub(crate) fn session_usage_value(usage: SessionUsage) -> Usage {
    Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage.cache_read_tokens,
        cache_creation_input_tokens: usage.cache_creation_tokens,
    }
}

/// Concatenate the text of a content-block list.
pub(crate) fn content_text(
    content: &[awaken_agent_contract::agent::content::ContentBlock],
) -> String {
    use awaken_agent_contract::agent::content::ContentBlock;
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}
