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
    #[cfg(feature = "serpapi")]
    pub fn new(api_key: impl Into<String>, base_url: Option<String>) -> Self {
        let provider = crate::providers::SerpApiProvider::new(api_key, base_url);
        Self::with_provider(std::sync::Arc::new(provider))
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
                if n < 1 || n > 20 {
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
                json!({
                    "title": r.title,
                    "url": r.url,
                    "snippet": r.snippet,
                })
            })
            .collect();

        Ok(ToolResult::success(Self::TOOL_ID, Value::Array(output_results)).into())
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
