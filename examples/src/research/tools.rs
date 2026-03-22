use async_trait::async_trait;
use serde_json::{Value, json};

use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult,
};

use super::state::ResearchState;

/// Search the web for information on a research topic.
///
/// Returns mock search results (can be replaced with a real search API).
pub struct SearchTool;

#[async_trait]
impl Tool for SearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "search",
            "Search",
            "Search the web for research information",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query"
                }
            },
            "required": ["query"]
        }))
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'query'".into()))?;

        let results = mock_search_results(query);

        Ok(ToolResult::success(
            "search",
            json!({
                "query": query,
                "results": results,
            }),
        ))
    }
}

/// Write or update the research report.
pub struct WriteReportTool;

#[async_trait]
impl Tool for WriteReportTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "write_report",
            "Write Report",
            "Write or update the research report",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "report": {
                    "type": "string",
                    "description": "The full report content in markdown"
                }
            },
            "required": ["report"]
        }))
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let report = args["report"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'report'".into()))?;

        Ok(ToolResult::success(
            "write_report",
            json!({ "length": report.len() }),
        ))
    }
}

/// Set the research question.
pub struct SetQuestionTool;

#[async_trait]
impl Tool for SetQuestionTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "set_research_question",
            "Set Research Question",
            "Set or update the research question",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The research question"
                }
            },
            "required": ["question"]
        }))
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let question = args["question"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'question'".into()))?;

        Ok(ToolResult::success(
            "set_research_question",
            json!({ "question": question }),
        ))
    }
}

/// Delete resources from the resource list.
pub struct DeleteResourcesTool;

#[async_trait]
impl Tool for DeleteResourcesTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "delete_resources",
            "Delete Resources",
            "Delete resources from the research collection",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "resource_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "IDs of resources to delete"
                }
            },
            "required": ["resource_ids"]
        }))
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let ids: Vec<&str> = args["resource_ids"]
            .as_array()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'resource_ids' array".into()))?
            .iter()
            .filter_map(|v| v.as_str())
            .collect();

        let deleted = ctx.state::<ResearchState>().map_or(0, |s| {
            s.resources
                .iter()
                .filter(|r| ids.contains(&r.id.as_str()))
                .count()
        });

        Ok(ToolResult::success(
            "delete_resources",
            json!({ "deleted": deleted }),
        ))
    }
}

/// Extract resources from search results and add them to the collection.
pub struct ExtractResourcesTool;

#[async_trait]
impl Tool for ExtractResourcesTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "extract_resources",
            "Extract Resources",
            "Extract and add resources from search results",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "resources": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id":          { "type": "string" },
                            "url":         { "type": "string" },
                            "title":       { "type": "string" },
                            "description": { "type": "string" }
                        },
                        "required": ["id", "url", "title", "description"]
                    },
                    "description": "Resources to add"
                }
            },
            "required": ["resources"]
        }))
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let raw = args["resources"]
            .as_array()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'resources' array".into()))?;

        let existing = ctx
            .state::<ResearchState>()
            .map_or(0, |s| s.resources.len());

        Ok(ToolResult::success(
            "extract_resources",
            json!({ "added": raw.len(), "total": existing + raw.len() }),
        ))
    }
}

// ---------------------------------------------------------------------------
// Mock data
// ---------------------------------------------------------------------------

fn mock_search_results(query: &str) -> Vec<Value> {
    let prefix = uuid::Uuid::new_v4().to_string()[..8].to_string();
    vec![
        json!({
            "id": format!("{prefix}-1"),
            "url": format!("https://example.com/article-1?q={}", urlencoded(query)),
            "title": format!("Understanding {query}: A Comprehensive Guide"),
            "snippet": format!("This article explores the key aspects of {query}...")
        }),
        json!({
            "id": format!("{prefix}-2"),
            "url": format!("https://example.com/article-2?q={}", urlencoded(query)),
            "title": format!("{query} - Latest Research and Findings"),
            "snippet": format!("Recent studies on {query} have shown...")
        }),
        json!({
            "id": format!("{prefix}-3"),
            "url": format!("https://example.com/article-3?q={}", urlencoded(query)),
            "title": format!("Expert Analysis: The Impact of {query}"),
            "snippet": format!("Experts weigh in on {query} and its implications...")
        }),
    ]
}

fn urlencoded(s: &str) -> String {
    s.replace(' ', "+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_search_returns_three_results() {
        let results = mock_search_results("quantum computing");
        assert_eq!(results.len(), 3);
        assert!(
            results[0]["title"]
                .as_str()
                .unwrap()
                .contains("quantum computing")
        );
    }

    #[tokio::test]
    async fn search_tool_descriptor() {
        let tool = SearchTool;
        let desc = tool.descriptor();
        assert_eq!(desc.id, "search");
    }

    #[tokio::test]
    async fn search_tool_returns_results() {
        let tool = SearchTool;
        let ctx = ToolCallContext::test_default();
        let result = tool
            .execute(json!({"query": "rust programming"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_success());
        assert_eq!(result.data["results"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn write_report_tool_returns_length() {
        let tool = WriteReportTool;
        let ctx = ToolCallContext::test_default();
        let result = tool
            .execute(json!({"report": "Test report content"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_success());
        assert_eq!(result.data["length"], 19);
    }

    #[tokio::test]
    async fn set_question_tool_returns_question() {
        let tool = SetQuestionTool;
        let ctx = ToolCallContext::test_default();
        let result = tool
            .execute(json!({"question": "What is AI?"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_success());
        assert_eq!(result.data["question"], "What is AI?");
    }
}
