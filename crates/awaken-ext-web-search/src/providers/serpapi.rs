//! SerpAPI search provider implementation

use async_trait::async_trait;
use awaken_contract::contract::tool::{ToolCallContext, ToolError};
use reqwest::Url;
use serde::Deserialize;

use crate::tool::{SearchProvider, SearchResult};

/// SerpAPI search provider
///
/// Implements the [`SearchProvider`] trait for SerpAPI-compatible
/// search endpoints.
#[derive(Debug, Clone)]
pub struct SerpApiProvider {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
}

impl SerpApiProvider {
    const MAX_RETRIES: u32 = 2;
    const DEFAULT_BASE_URL: &'static str = "https://serpapi.com/search.json";

    /// Create a new SerpAPI provider with the given API key and optional base URL.
    ///
    /// If `api_key` is empty, it will fall back to reading from the
    /// `SERPAPI_KEY` environment variable.
    pub fn new(api_key: impl Into<String>, base_url: Option<String>) -> Self {
        let mut api_key = api_key.into();
        // Fallback to environment variable if empty
        if api_key.is_empty() {
            api_key = std::env::var("SERPAPI_KEY").unwrap_or_default();
        }

        let base_url = base_url.unwrap_or_else(|| Self::DEFAULT_BASE_URL.to_string());

        // Build client with default timeout
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();

        Self {
            api_key,
            base_url,
            client,
        }
    }
}

#[derive(Debug, Deserialize)]
struct SerpApiResponse {
    #[serde(default)]
    organic_results: Vec<SerpApiResult>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SerpApiResult {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    link: Option<String>,
    #[serde(default)]
    snippet: Option<String>,
}

#[async_trait]
impl SearchProvider for SerpApiProvider {
    async fn search(
        &self,
        query: &str,
        num_results: usize,
        ctx: &ToolCallContext,
    ) -> Result<Vec<SearchResult>, ToolError> {
        let mut endpoint = self
            .base_url
            .parse::<Url>()
            .map_err(|e| ToolError::ExecutionFailed(format!("invalid web_search base_url: {e}")))?;

        {
            let mut qp = endpoint.query_pairs_mut();
            qp.append_pair("engine", "google");
            qp.append_pair("q", query);
            qp.append_pair("num", &num_results.to_string());
            qp.append_pair("api_key", &self.api_key);
        }

        let request = self.client.get(endpoint);

        // Retry logic for timeout errors
        let mut _last_error = None;
        let mut response = None;

        for attempt in 0..=Self::MAX_RETRIES {
            let request_clone = request
                .try_clone()
                .ok_or_else(|| ToolError::ExecutionFailed("failed to clone request".into()))?;

            let result = match ctx.cancellation_token {
                Some(ref token) => {
                    tokio::select! {
                        result = request_clone.send() => result,
                        _ = token.cancelled() => {
                            return Err(ToolError::ExecutionFailed("search was cancelled".into()));
                        }
                    }
                }
                None => request_clone.send().await,
            };

            match result {
                Ok(resp) => {
                    response = Some(resp);
                    break;
                }
                Err(e) => {
                    if attempt < Self::MAX_RETRIES && e.is_timeout() {
                        _last_error = Some(e);
                        continue;
                    }
                    return Err(if e.is_timeout() {
                        ToolError::ExecutionFailed("web search timed out after 10s".into())
                    } else {
                        ToolError::ExecutionFailed(e.to_string())
                    });
                }
            }
        }

        let response = response.expect("at least one attempt should succeed");

        let response = response
            .error_for_status()
            .map_err(|e: reqwest::Error| ToolError::ExecutionFailed(e.to_string()))?;

        let payload: SerpApiResponse = response
            .json()
            .await
            .map_err(|e: reqwest::Error| ToolError::ExecutionFailed(e.to_string()))?;

        if let Some(err) = payload.error {
            return Err(ToolError::ExecutionFailed(err));
        }

        let mut results = Vec::new();
        for r in payload.organic_results.into_iter() {
            if results.len() >= num_results {
                break;
            }

            let (Some(title), Some(url)) = (r.title, r.link) else {
                continue;
            };

            results.push(SearchResult {
                title,
                url,
                snippet: r.snippet.unwrap_or_default(),
            });
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::cancellation::CancellationToken;
    use awaken_contract::contract::tool::ToolCallContext;
    use httpmock::Method::GET;
    use httpmock::MockServer;
    use serde_json::json;

    #[tokio::test]
    async fn search_parses_results_correctly() {
        let server = MockServer::start_async().await;
        let endpoint = server.url("/search");

        let mock = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/search")
                    .query_param("engine", "google")
                    .query_param("q", "rust")
                    .query_param("num", "5")
                    .query_param("api_key", "k");

                then.status(200).json_body(json!({
                    "organic_results": [
                        {"title": "Rust", "link": "https://www.rust-lang.org/", "snippet": "Rust language"},
                        {"title": "Tokio", "link": "https://tokio.rs/", "snippet": "Async runtime"},
                        {"title": null, "link": "https://skip.example/", "snippet": "missing title"}
                    ]
                }));
            })
            .await;

        let provider = SerpApiProvider::new("k", Some(endpoint));
        let ctx = ToolCallContext::test_default();
        let results = provider.search("rust", 5, &ctx).await.unwrap();

        mock.assert_async().await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert_eq!(results[0].snippet, "Rust language");
    }

    #[tokio::test]
    async fn search_fails_when_already_cancelled() {
        let token = CancellationToken::new();
        token.cancel();

        let mut ctx = ToolCallContext::test_default();
        ctx.cancellation_token = Some(token);

        let provider = SerpApiProvider::new("k", None);
        let err = provider
            .search("rust", 5, &ctx)
            .await
            .expect_err("should fail when cancelled");

        match err {
            ToolError::ExecutionFailed(msg) => assert_eq!(msg, "search was cancelled"),
            other => panic!("expected ExecutionFailed with 'search was cancelled', got {other:?}"),
        }
    }
}
