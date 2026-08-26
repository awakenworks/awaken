//! Typed Agent-owned Web execution policy and its runtime projection.

use awaken_runtime_contract::agent_bindings::{ToolsetPolicy, ToolsetSource};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Extension-owned execution settings decoded from the neutral policy's one
/// opaque configuration value. The serde shape is the durable shape previously
/// stored by the neutral contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum WebExecutionConfiguration {
    WebFetch(WebFetchExecutionConfiguration),
    WebSearch(WebSearchExecutionConfiguration),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "domains",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum WebDomainFilter {
    Allow(Vec<String>),
    Block(Vec<String>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebFetchExecutionConfiguration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domains: Option<WebDomainFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_content_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchExecutionConfiguration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domains: Option<WebDomainFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_location: Option<WebSearchUserLocation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchUserLocation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

fn execution_configuration<'a>(toolsets: &'a [ToolsetPolicy], name: &str) -> Option<&'a Value> {
    toolsets
        .iter()
        .find(|policy| matches!(policy.source, ToolsetSource::Agent))
        .and_then(|policy| policy.configuration_for(name))
}

/// Read the normalized `web_fetch` settings from the executable toolset.
/// Authoring and runtime never maintain a parallel settings map.
pub fn web_fetch_execution_configuration(
    toolsets: &[ToolsetPolicy],
) -> Result<Option<WebFetchExecutionConfiguration>, String> {
    let Some(value) = execution_configuration(toolsets, "web_fetch") else {
        return Ok(None);
    };
    match serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid web_fetch execution configuration: {error}"))?
    {
        WebExecutionConfiguration::WebFetch(configuration) => Ok(Some(configuration)),
        WebExecutionConfiguration::WebSearch(_) => {
            Err("web_fetch policy carries web_search execution configuration".to_string())
        }
    }
}

/// Read the normalized `web_search` settings from the executable toolset. The
/// same value configures Native dispatch and ACP export.
pub fn web_search_execution_configuration(
    toolsets: &[ToolsetPolicy],
) -> Result<Option<WebSearchExecutionConfiguration>, String> {
    let Some(value) = execution_configuration(toolsets, "web_search") else {
        return Ok(None);
    };
    match serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid web_search execution configuration: {error}"))?
    {
        WebExecutionConfiguration::WebSearch(configuration) => Ok(Some(configuration)),
        WebExecutionConfiguration::WebFetch(_) => {
            Err("web_search policy carries web_fetch execution configuration".to_string())
        }
    }
}

pub(super) fn url_matches_filter(url: &url::Url, filter: &WebDomainFilter) -> bool {
    let matches = |configured: &str| {
        let (domain, path) = configured.split_once('/').unwrap_or((configured, ""));
        let host_matches = url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case(domain)
                || host
                    .to_ascii_lowercase()
                    .ends_with(&format!(".{}", domain.to_ascii_lowercase()))
        });
        host_matches
            && (path.is_empty()
                || url.path() == format!("/{path}")
                || url.path().starts_with(&format!("/{path}/")))
    };
    match filter {
        WebDomainFilter::Allow(domains) => domains.iter().any(|domain| matches(domain)),
        WebDomainFilter::Block(domains) => !domains.iter().any(|domain| matches(domain)),
    }
}
