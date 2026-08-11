//! Free helpers shared by the [`ManagedState`] event path: rubric normalization,
//! usage projection, and content-block text extraction.

use super::*;
use crate::types::{ServerToolUsage, SessionThreadCacheCreationUsage};

pub(crate) fn lifecycle_fact(
    id: String,
    session_id: &str,
    workspace_id: Option<String>,
    event_type: &str,
) -> ManagedLifecycleFact {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    ManagedLifecycleFact {
        id,
        object_id: session_id.to_string(),
        workspace_id,
        event_type: event_type.to_string(),
        timestamp,
        runtime_interval: None,
    }
}

/// Normalize the typed Managed rubric to the evaluator's text/file reference.
pub(crate) fn rubric_text(rubric: &crate::types::OutcomeRubric) -> String {
    match rubric {
        crate::types::OutcomeRubric::Text { content } => content.clone(),
        crate::types::OutcomeRubric::File { file_id } => file_id.clone(),
    }
}

/// The session's `usage` object (`BetaManagedAgentsSessionUsage`): cumulative input +
/// output (+ prompt-cache) token counts across all turns. Emitted whenever a turn ran.
pub(crate) fn session_usage_value(usage: SessionUsage) -> Usage {
    Usage {
        active_seconds: Some(usage.active_seconds),
        input_tokens: Some(usage.input_tokens),
        output_tokens: Some(usage.output_tokens),
        cache_read_input_tokens: Some(usage.cache_read_tokens),
        cache_creation: Some(SessionThreadCacheCreationUsage {
            ephemeral_1h_input_tokens: None,
            ephemeral_5m_input_tokens: Some(usage.cache_creation_tokens),
        }),
        list_cost: None,
        server_tool_use: Some(ServerToolUsage {
            web_fetch_requests: Some(usage.web_fetch_requests),
            web_search_requests: Some(usage.web_search_requests),
        }),
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
