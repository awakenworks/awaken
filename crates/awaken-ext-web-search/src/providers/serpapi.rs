//! SerpAPI search provider implementation
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
    const RETRY_BASE_DELAY_MS: u64 = 100;

    /// Check if an error is retryable
    fn is_retryable_error(e: &reqwest::Error) -> bool {
        e.is_timeout()
            || e.status()
                .map(|s| s.is_server_error() || s.as_u16() == 429)
                .unwrap_or(false)
    }

    /// Get a sanitized error message without the URL to prevent API key leakage
    fn sanitized_error(e: reqwest::Error) -> String {
        e.without_url().to_string()
    }

    /// Get retry delay with exponential backoff
    fn retry_delay(attempt: u32) -> std::time::Duration {
        std::time::Duration::from_millis(Self::RETRY_BASE_DELAY_MS * (1 << attempt))
    }

    /// Create a new SerpAPI provider with the given API key and optional base URL.
    ///
    /// If `api_key` is empty, it will fall back to reading from the
    /// `SERPAPI_KEY` environment variable.
    /// Create a new SerpAPI provider with the given API key and optional base URL.
    ///
    /// If `api_key` is empty, it will fall back to reading from the
    /// `SERPAPI_KEY` environment variable.
    ///
    /// Returns an error if the API key is empty after fallback.
    pub fn new(api_key: impl Into<String>, base_url: Option<String>) -> Result<Self, ToolError> {
        let mut api_key = api_key.into();
        // Fallback to environment variable if empty
        if api_key.is_empty() {
            api_key = std::env::var("SERPAPI_KEY").unwrap_or_default();
        }

        // Validate API key locally before making requests
        if api_key.is_empty() {
            return Err(ToolError::InvalidArguments(
                "SerpAPI API key is required. Provide it explicitly or set the SERPAPI_KEY environment variable.".into()
            ));
        }

        let base_url = base_url.unwrap_or_else(|| Self::DEFAULT_BASE_URL.to_string());

        // Build client with default timeout
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();

        Ok(Self {
            api_key,
            base_url,
            client,
        })
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

        // Retry logic for timeout, 429, and 5xx errors with exponential backoff
        let mut _last_error = None;
        let mut response = None;

        for attempt in 0..=Self::MAX_RETRIES {
            let request_clone = request
                .try_clone()
                .ok_or_else(|| ToolError::ExecutionFailed("failed to clone request".into()))?;

            // Check for early cancellation before making the request
            if ctx
                .cancellation_token
                .as_ref()
                .map(|t| t.is_cancelled())
                .unwrap_or(false)
            {
                return Err(ToolError::ExecutionFailed("search was cancelled".into()));
            }

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
                    // Check for HTTP error status
                    match resp.error_for_status_ref() {
                        Ok(_) => {
                            response = Some(resp);
                            break;
                        }
                        Err(e) => {
                            if attempt < Self::MAX_RETRIES && Self::is_retryable_error(&e) {
                                _last_error = Some(e);
                                tokio::time::sleep(Self::retry_delay(attempt)).await;
                                continue;
                            }
                            return Err(ToolError::ExecutionFailed(Self::sanitized_error(e)));
                        }
                    }
                }
                Err(e) => {
                    if attempt < Self::MAX_RETRIES && Self::is_retryable_error(&e) {
                        _last_error = Some(e);
                        tokio::time::sleep(Self::retry_delay(attempt)).await;
                        continue;
                    }
                    return Err(if e.is_timeout() {
                        ToolError::ExecutionFailed("web search timed out after 10s".into())
                    } else {
                        ToolError::ExecutionFailed(Self::sanitized_error(e))
                    });
                }
            }
        }

        let response = response.expect("at least one attempt should succeed");

        // Apply cancellation to body reading and JSON parsing
        let payload = match ctx.cancellation_token {
            Some(ref token) => {
                tokio::select! {
                    result = response.json() => result,
                    _ = token.cancelled() => {
                        return Err(ToolError::ExecutionFailed("search was cancelled during response parsing".into()));
                    }
                }
            }
            None => response.json().await,
        }
        .map_err(|e: reqwest::Error| ToolError::ExecutionFailed(Self::sanitized_error(e)))?;

        let payload: SerpApiResponse = payload;

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
                provider: Some("serpapi".into()),
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

        let provider = SerpApiProvider::new("k", Some(endpoint)).unwrap();
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

        let provider = SerpApiProvider::new("k", None).unwrap();
        let err = provider
            .search("rust", 5, &ctx)
            .await
            .expect_err("should fail when cancelled");

        match err {
            ToolError::ExecutionFailed(msg) => assert_eq!(msg, "search was cancelled"),
            other => panic!("expected ExecutionFailed with 'search was cancelled', got {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_error_does_not_leak_api_key() {
        let server = MockServer::start_async().await;
        let endpoint = server.url("/search");

        let mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/search");
                then.status(401).body("Unauthorized");
            })
            .await;

        let provider = SerpApiProvider::new("super-secret-key-12345", Some(endpoint)).unwrap();
        let ctx = ToolCallContext::test_default();
        let result = provider.search("rust", 5, &ctx).await;

        mock.assert_async().await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            !err.contains("super-secret-key-12345"),
            "API key leaked in error: {}",
            err
        );
    }

    #[tokio::test]
    async fn retries_on_5xx_with_backoff() {
        let server = MockServer::start_async().await;
        let endpoint = server.url("/search");

        // First 2 requests return 503, third succeeds
        let mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/search");
                then.status(503).body("Service Unavailable");
            })
            .await;

        let provider = SerpApiProvider::new("k", Some(endpoint)).unwrap();
        let ctx = ToolCallContext::test_default();
        let result = provider.search("rust", 5, &ctx).await;

        // Should have retried twice (3 total attempts)
        mock.assert_hits_async(3).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn api_error_from_response_is_propagated() {
        let server = MockServer::start_async().await;
        let endpoint = server.url("/search");

        let mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/search");
                then.status(200).json_body(json!({
                    "error": "Invalid API key",
                    "organic_results": []
                }));
            })
            .await;

        let provider = SerpApiProvider::new("bad-key", Some(endpoint)).unwrap();
        let ctx = ToolCallContext::test_default();
        let result = provider.search("rust", 5, &ctx).await;

        mock.assert_async().await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Invalid API key"));
    }
}
