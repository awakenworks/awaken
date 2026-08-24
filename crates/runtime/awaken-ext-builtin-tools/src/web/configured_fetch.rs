//! Shared WebFetch policy kernel used by the configured plugin.

use awaken_runtime_contract::tool::ToolError;

use super::{WebFetchExecutionConfiguration, url_matches_filter};

pub(super) fn configured_web_fetch_url(
    raw_url: &str,
    configuration: &WebFetchExecutionConfiguration,
) -> Result<url::Url, ToolError> {
    let url = url::Url::parse(raw_url)
        .map_err(|error| ToolError::InvalidArguments(format!("url: {error}")))?;
    if let Some(filter) = &configuration.domains
        && !url_matches_filter(&url, filter)
    {
        return Err(ToolError::Execution(
            "web_fetch URL is outside the configured domain policy".into(),
        ));
    }
    Ok(url)
}

pub(super) fn web_fetch_text_limit(
    url: &url::Url,
    configuration: &WebFetchExecutionConfiguration,
) -> Option<usize> {
    configuration
        .max_content_tokens
        // Binary content is not part of the configured text-context cap.
        // Preserve the legacy PDF URL exception for providers returning lossy
        // text while every provider keeps its independent response-size fence.
        .filter(|_| !url.path().to_ascii_lowercase().ends_with(".pdf"))
        .map(|limit| usize::try_from(limit).unwrap_or(usize::MAX))
}

pub(super) fn truncate_text(text: &mut String, max_bytes: usize) {
    let mut boundary = max_bytes.min(text.len());
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
}
