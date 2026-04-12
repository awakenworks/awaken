use async_trait::async_trait;
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use serde::Deserialize;
use serde_json::{Value, json};

pub struct WebSearchTool {
    api_key: String,
    base_url: Option<String>,
    client: reqwest::Client,
}

impl WebSearchTool {
    pub const TOOL_ID: &'static str = "web_search";

    pub fn new(api_key: impl Into<String>, base_url: Option<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url,
            client: reqwest::Client::new(),
        }
    }

    fn endpoint_url(&self) -> &str {
        self.base_url
            .as_deref()
            .unwrap_or("https://serpapi.com/search.json")
    }
}

#[derive(Debug, Deserialize)]
struct SerpApiResponse {
    #[serde(default)]
    organic_results: Vec<SerpApiOrganicResult>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SerpApiOrganicResult {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    link: Option<String>,
    #[serde(default)]
    snippet: Option<String>,
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
                    "description": "Maximum number of results to return"
                }
            },
            "required": ["query"]
        }))
    }
    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let _ = ctx;

        let query = args
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("missing 'query'".into()))?;

        let num_results: usize = args
            .get("num_results")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .try_into()
            .unwrap_or(5);

        let mut endpoint = self
            .endpoint_url()
            .parse::<reqwest::Url>()
            .map_err(|e| ToolError::ExecutionFailed(format!("invalid web_search base_url: {e}")))?;

        {
            let mut qp = endpoint.query_pairs_mut();
            qp.append_pair("engine", "google");
            qp.append_pair("q", query);
            qp.append_pair("api_key", &self.api_key);
            qp.append_pair("num", &num_results.to_string());
        }

        let response = self
            .client
            .get(endpoint)
            .send()
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let response = response
            .error_for_status()
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let payload: SerpApiResponse = response
            .json()
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

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

            results.push(json!({
                "title": title,
                "url": url,
                "snippet": r.snippet.unwrap_or_default(),
            }));
        }

        Ok(ToolResult::success(Self::TOOL_ID, Value::Array(results)).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::Method::GET;
    use httpmock::MockServer;

    #[tokio::test]
    async fn execute_parses_results_and_applies_default_limit() {
        let server = MockServer::start_async().await;
        let endpoint = server.url("/search");

        let mock = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/search")
                    .query_param("engine", "google")
                    .query_param("q", "rust")
                    .query_param("api_key", "k")
                    .query_param("num", "5");

                then.status(200).json_body(json!({
                    "organic_results": [
                        {"title": "Rust", "link": "https://www.rust-lang.org/", "snippet": "Rust language"},
                        {"title": "Tokio", "link": "https://tokio.rs/", "snippet": "Async runtime"},
                        {"title": null, "link": "https://skip.example/", "snippet": "missing title"}
                    ]
                }));
            })
            .await;

        let tool = WebSearchTool::new("k", Some(endpoint));
        let out = tool
            .execute(json!({"query": "rust"}), &ToolCallContext::test_default())
            .await
            .unwrap();

        mock.assert_async().await;

        assert!(out.result.is_success());
        let arr = out.result.data.as_array().expect("array result");
        assert_eq!(arr.len(), 2, "should skip incomplete entries");
        assert_eq!(arr[0]["title"], "Rust");
        assert_eq!(arr[0]["url"], "https://www.rust-lang.org/");
        assert_eq!(arr[0]["snippet"], "Rust language");
    }

    #[tokio::test]
    async fn execute_applies_num_results_limit() {
        let server = MockServer::start_async().await;
        let endpoint = server.url("/search");

        let mock = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/search")
                    .query_param("q", "rust")
                    .query_param("api_key", "k")
                    .query_param("num", "1");

                then.status(200).json_body(json!({
                    "organic_results": [
                        {"title": "Rust", "link": "https://www.rust-lang.org/", "snippet": "Rust language"},
                        {"title": "Tokio", "link": "https://tokio.rs/", "snippet": "Async runtime"}
                    ]
                }));
            })
            .await;

        let tool = WebSearchTool::new("k", Some(endpoint));
        let out = tool
            .execute(
                json!({"query": "rust", "num_results": 1}),
                &ToolCallContext::test_default(),
            )
            .await
            .unwrap();

        mock.assert_async().await;

        let arr = out.result.data.as_array().unwrap();
        assert_eq!(arr.len(), 1);
    }

    #[tokio::test]
    async fn execute_propagates_http_errors() {
        let server = MockServer::start_async().await;
        let endpoint = server.url("/search");

        server
            .mock_async(|when, then| {
                when.method(GET).path("/search");
                then.status(500).body("nope");
            })
            .await;

        let tool = WebSearchTool::new("k", Some(endpoint));
        let err = tool
            .execute(json!({"query": "rust"}), &ToolCallContext::test_default())
            .await
            .expect_err("should fail");

        match err {
            ToolError::ExecutionFailed(_) => {}
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_propagates_api_error_field() {
        let server = MockServer::start_async().await;
        let endpoint = server.url("/search");

        server
            .mock_async(|when, then| {
                when.method(GET).path("/search");
                then.status(200)
                    .json_body(json!({"error": "invalid api key"}));
            })
            .await;

        let tool = WebSearchTool::new("k", Some(endpoint));
        let err = tool
            .execute(json!({"query": "rust"}), &ToolCallContext::test_default())
            .await
            .expect_err("should fail");

        match err {
            ToolError::ExecutionFailed(msg) => assert!(msg.contains("invalid api key")),
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }
}
