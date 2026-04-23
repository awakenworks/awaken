//! Web search tool core types and implementation

use async_trait::async_trait;
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use serde_json::{Value, json};
use std::sync::Arc;

/// Represents a single search result from any provider
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    /// Name of the provider that returned this result (for attribution)
    pub provider: Option<String>,
}

/// Abstract trait for search providers
///
/// Implement this trait to add support for new search backends:
/// - Google Custom Search
/// - Bing Search
/// - DuckDuckGo
/// - etc.
#[async_trait]
pub trait SearchProvider: Send + Sync {
    async fn search(
        &self,
        query: &str,
        num_results: usize,
        ctx: &ToolCallContext,
    ) -> Result<Vec<SearchResult>, ToolError>;
}

/// Web search tool that delegates to a generic search provider
///
/// This tool follows the [Tool](awaken_contract::contract::tool::Tool) contract
/// and can be registered with the [AgentRuntimeBuilder](awaken_runtime::builder::AgentRuntimeBuilder).
pub struct WebSearchTool {
    provider: Arc<dyn SearchProvider>,
}

impl WebSearchTool {
    /// Unique tool identifier
    pub const TOOL_ID: &'static str = "web_search";

    /// Create a new web search tool with the given provider
    ///
    /// Use this constructor when you have a custom provider implementation.
    pub fn with_provider(provider: Arc<dyn SearchProvider>) -> Self {
        Self { provider }
    }

    /// Create a new web search tool with the default SerpAPI provider
    ///
    /// If `api_key` is empty, it will be read from the `SERPAPI_KEY` environment variable.
    /// If `base_url` is `None`, the default SerpAPI base URL will be used.
    ///
    /// **Note**: This constructor is only available when the `serpapi` feature is enabled.
    ///
    /// Returns an error if the API key is empty after fallback.
    #[cfg(feature = "serpapi")]
    pub fn new(api_key: impl Into<String>, base_url: Option<String>) -> Result<Self, ToolError> {
        let provider = crate::providers::SerpApiProvider::new(api_key, base_url)?;
        Ok(Self::with_provider(std::sync::Arc::new(provider)))
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            Self::TOOL_ID,
            Self::TOOL_ID,
            "Run a web search query and return result snippets.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query"
                },
                "num_results": {
                    "type": "integer",
                    "description": "Maximum number of results to return",
                    "default": 5,
                    "minimum": 1,
                    "maximum": 20
                }
            },
            "required": ["query"]
        }))
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("missing 'query'".into()))?;

        let num_results = match args.get("num_results") {
            Some(v) => {
                let n = v.as_u64().ok_or_else(|| {
                    ToolError::InvalidArguments("'num_results' must be an integer".into())
                })?;
                if !(1..=20).contains(&n) {
                    return Err(ToolError::InvalidArguments(
                        "'num_results' must be between 1 and 20 inclusive".into(),
                    ));
                }
                n as usize
            }
            None => 5,
        };

        let results = self.provider.search(query, num_results, ctx).await?;

        let output_results: Vec<Value> = results
            .into_iter()
            .map(|r| {
                let mut result = json!({
                    "title": r.title,
                    "url": r.url,
                    "snippet": r.snippet,
                });
                if let Some(provider) = r.provider {
                    result["provider"] = Value::String(provider);
                }
                result
            })
            .collect();

        Ok(ToolResult::success(Self::TOOL_ID, Value::Array(output_results)).into())
    }
}


/// Strategy for combining results from multiple providers
#[derive(Debug, Clone, Copy)]
pub enum CompositeMode {
    /// Try providers in order until one succeeds
    Fallback,
    /// Query all providers in parallel and merge results
    Merge,
}

/// A composite search provider that combines results from multiple providers
///
/// Supports two modes:
/// - Fallback: Try providers sequentially until one succeeds
/// - Merge: Query all providers, deduplicate by URL
pub struct CompositeSearchProvider {
    providers: Vec<(String, Arc<dyn SearchProvider>)>,
    mode: CompositeMode,
}

impl CompositeSearchProvider {
    /// Create a new composite provider with the given mode
    pub fn new(mode: CompositeMode) -> Self {
        Self {
            providers: Vec::new(),
            mode,
        }
    }

    /// Add a provider to the composite
    pub fn add_provider(&mut self, name: String, provider: Arc<dyn SearchProvider>) {
        self.providers.push((name, provider));
    }

    /// Execute in fallback mode: try providers in order
    async fn search_fallback(
        &self,
        query: &str,
        num_results: usize,
        ctx: &ToolCallContext,
    ) -> Result<Vec<SearchResult>, ToolError> {
        let mut last_error = None;

        for (_name, provider) in &self.providers {
            match provider.search(query, num_results, ctx).await {
                Ok(results) if !results.is_empty() => return Ok(results),
                Ok(_) => continue,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            }
        }

        Err(last_error
            .unwrap_or_else(|| ToolError::ExecutionFailed("no providers configured".into())))
    }

    /// Execute in merge mode: query all providers in parallel
    async fn search_merge(
        &self,
        query: &str,
        num_results: usize,
        ctx: &ToolCallContext,
    ) -> Result<Vec<SearchResult>, ToolError> {
        // Spawn all provider requests in parallel
        let mut futures = Vec::new();
        for (name, provider) in &self.providers {
            let name = name.clone();
            let provider = provider.clone();
            let query = query.to_string();
            let ctx = ctx.clone();
            futures.push(async move {
                (name, provider.search(&query, num_results, &ctx).await)
            });
        }

        // Wait for all results
        let results = futures::future::join_all(futures).await;

        // Merge and deduplicate by URL
        let mut seen_urls = std::collections::HashSet::new();
        let mut merged = Vec::new();

        for (_name, result) in results {
            if let Ok(items) = result {
                for item in items {
                    if seen_urls.insert(item.url.clone()) {
                    // Deduplicate by URL
                    merged.push(item);
                    if merged.len() >= num_results {
                        break;
                    }
                }
            }
        }
    }

        Ok(merged)
    }
}

#[async_trait]
impl SearchProvider for CompositeSearchProvider {
    async fn search(
        &self,
        query: &str,
        num_results: usize,
        ctx: &ToolCallContext,
    ) -> Result<Vec<SearchResult>, ToolError> {
        match self.mode {
            CompositeMode::Fallback => {
                self.search_fallback(query, num_results, ctx).await
            }
            CompositeMode::Merge => {
                self.search_merge(query, num_results, ctx).await
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_contract::contract::tool::{ToolCallContext, ToolError};

    #[derive(Debug, Clone)]
    struct MockProvider;

    #[async_trait]
    impl SearchProvider for MockProvider {
        async fn search(
            &self,
            _query: &str,
            _num_results: usize,
            _ctx: &ToolCallContext,
        ) -> Result<Vec<SearchResult>, ToolError> {
            Ok(vec![SearchResult {
                title: "Test".into(),
                url: "https://example.com".into(),
                snippet: "Test snippet".into(),
                provider: None,
            }])
        }
    }

    #[tokio::test]
    async fn executes_with_custom_provider() {
        let tool = WebSearchTool::with_provider(Arc::new(MockProvider));
        let ctx = ToolCallContext::test_default();
        let out = tool.execute(json!({"query": "test"}), &ctx).await.unwrap();

        assert!(out.result.is_success());
    }
}
