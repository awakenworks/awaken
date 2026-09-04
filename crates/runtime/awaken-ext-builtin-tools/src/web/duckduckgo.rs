//! Credential-free DuckDuckGo search adapter and response normalization.

use async_trait::async_trait;
use awaken_credential::Credential;
use awaken_runtime_contract::tool::ToolError;
use serde::Deserialize;
use serde_json::json;

use super::{
    DUCKDUCKGO_PROVIDER_ID, WebSearchCredentialRequirement, WebSearchProvider,
    WebSearchProviderDescriptor, WebSearchRequest, WebSearchResult, blocking,
};

pub struct DuckDuckGoProvider;

#[derive(Deserialize, Default)]
struct DdgResponse {
    #[serde(default, rename = "Heading")]
    heading: String,
    #[serde(default, rename = "AbstractText")]
    abstract_text: String,
    #[serde(default, rename = "AbstractURL")]
    abstract_url: String,
    #[serde(default, rename = "RelatedTopics")]
    related_topics: Vec<DdgTopic>,
}

#[derive(Deserialize, Default)]
struct DdgTopic {
    #[serde(default, rename = "Text")]
    text: String,
    #[serde(default, rename = "FirstURL")]
    first_url: String,
}

fn normalize_results(response: DdgResponse, count: usize) -> Vec<WebSearchResult> {
    let mut results = Vec::new();
    if !response.abstract_text.is_empty() {
        results.push(WebSearchResult {
            title: response.heading,
            url: response.abstract_url,
            snippet: response.abstract_text,
        });
    }
    results.extend(
        response
            .related_topics
            .into_iter()
            .filter(|topic| !topic.text.is_empty())
            .map(|topic| WebSearchResult {
                title: topic.text.clone(),
                url: topic.first_url,
                snippet: topic.text,
            }),
    );
    results.truncate(count);
    results
}

#[async_trait]
impl WebSearchProvider for DuckDuckGoProvider {
    fn descriptor(&self) -> WebSearchProviderDescriptor {
        WebSearchProviderDescriptor {
            id: DUCKDUCKGO_PROVIDER_ID.into(),
            label: "DuckDuckGo (free)".into(),
            credential: WebSearchCredentialRequirement::None,
            options_schema: json!({ "type": "object", "additionalProperties": false }),
        }
    }

    async fn search(
        &self,
        request: WebSearchRequest,
        _credential: Option<&Credential>,
    ) -> Result<Vec<WebSearchResult>, ToolError> {
        blocking(move || {
            let response: DdgResponse = ureq::get("https://api.duckduckgo.com/")
                .query("q", &request.query)
                .query("format", "json")
                .query("no_html", "1")
                .query("no_redirect", "1")
                .call()
                .map_err(|err| ToolError::Execution(format!("DuckDuckGo search: {err}")))?
                .into_json()
                .map_err(|err| ToolError::Execution(format!("parse DuckDuckGo search: {err}")))?;
            Ok(normalize_results(response, request.count))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_normalizes_abstract_and_topics() {
        // Cause/effect rule D1: one abstract plus one non-empty related topic,
        // with a limit above both, yields two normalized results in source
        // order. Normalization remains owned by the provider adapter.
        let results = normalize_results(
            DdgResponse {
                heading: "Rust".into(),
                abstract_text: "A language".into(),
                abstract_url: "https://rust-lang.org".into(),
                related_topics: vec![DdgTopic {
                    text: "Cargo".into(),
                    first_url: "https://doc.rust-lang.org/cargo".into(),
                }],
            },
            8,
        );
        assert_eq!(results.len(), 2, "D1");
        assert_eq!(results[0].title, "Rust", "D1");
        assert_eq!(results[1].url, "https://doc.rust-lang.org/cargo", "D1");
    }
}
