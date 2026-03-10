use async_trait::async_trait;
use serde_json::{json, Value};
use tirea_agentos::contracts::runtime::tool_call::{
    Tool, ToolDescriptor, ToolError, ToolExecutionEffect, ToolResult,
};
use tirea_agentos::contracts::AnyStateAction;
use tirea_agentos::contracts::ToolCallContext;

use super::state::{LogEntry, ResearchAction, ResearchState, Resource};

/// Search the web for information on a research topic.
///
/// Corresponds to CopilotKit's `Search` tool (mock Tavily).
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

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(<Self as Tool>::execute_effect(self, args, ctx)
            .await?
            .result)
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'query'".into()))?;

        let state = ctx.snapshot_of::<ResearchState>().unwrap_or_default();
        let mut logs = state.logs;
        logs.push(LogEntry {
            message: format!("Searching for: {query}"),
            level: "info".into(),
            step: "search".into(),
        });

        let results = mock_search_results(query);

        logs.push(LogEntry {
            message: format!("Found {} results for: {query}", results.len()),
            level: "info".into(),
            step: "search".into(),
        });

        Ok(ToolExecutionEffect::new(ToolResult::success(
            "search",
            json!({
                "query": query,
                "results": results,
            }),
        ))
        .with_action(AnyStateAction::new::<ResearchState>(
            ResearchAction::SetLogs(logs),
        )))
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

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(<Self as Tool>::execute_effect(self, args, ctx)
            .await?
            .result)
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let report = args["report"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'report'".into()))?;

        let state = ctx.snapshot_of::<ResearchState>().unwrap_or_default();
        let mut logs = state.logs;
        logs.push(LogEntry {
            message: format!("Report updated ({} chars)", report.len()),
            level: "info".into(),
            step: "write_report".into(),
        });

        Ok(ToolExecutionEffect::new(ToolResult::success(
            "write_report",
            json!({ "length": report.len() }),
        ))
        .with_action(AnyStateAction::new::<ResearchState>(
            ResearchAction::SetReport(report.to_string()),
        ))
        .with_action(AnyStateAction::new::<ResearchState>(
            ResearchAction::SetLogs(logs),
        )))
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

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(<Self as Tool>::execute_effect(self, args, ctx)
            .await?
            .result)
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let question = args["question"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'question'".into()))?;

        let state = ctx.snapshot_of::<ResearchState>().unwrap_or_default();
        let mut logs = state.logs;
        logs.push(LogEntry {
            message: format!("Research question set: {question}"),
            level: "info".into(),
            step: "set_question".into(),
        });

        Ok(ToolExecutionEffect::new(ToolResult::success(
            "set_research_question",
            json!({ "question": question }),
        ))
        .with_action(AnyStateAction::new::<ResearchState>(
            ResearchAction::SetResearchQuestion(question.to_string()),
        ))
        .with_action(AnyStateAction::new::<ResearchState>(
            ResearchAction::SetLogs(logs),
        )))
    }
}

/// Delete resources from the resource list.
///
/// Requires HITL confirmation (Pattern 2).
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

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(<Self as Tool>::execute_effect(self, args, ctx)
            .await?
            .result)
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let ids: Vec<&str> = args["resource_ids"]
            .as_array()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'resource_ids' array".into()))?
            .iter()
            .filter_map(|v| v.as_str())
            .collect();

        let state = ctx.snapshot_of::<ResearchState>().unwrap_or_default();
        let mut resources = state.resources;
        let before = resources.len();
        resources.retain(|r| !ids.contains(&r.id.as_str()));
        let deleted = before - resources.len();

        let mut logs = state.logs;
        logs.push(LogEntry {
            message: format!("Deleted {deleted} resources"),
            level: "info".into(),
            step: "delete_resources".into(),
        });

        Ok(ToolExecutionEffect::new(ToolResult::success(
            "delete_resources",
            json!({ "deleted": deleted }),
        ))
        .with_action(AnyStateAction::new::<ResearchState>(
            ResearchAction::SetResources(resources),
        ))
        .with_action(AnyStateAction::new::<ResearchState>(
            ResearchAction::SetLogs(logs),
        )))
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

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(<Self as Tool>::execute_effect(self, args, ctx)
            .await?
            .result)
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let raw = args["resources"]
            .as_array()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'resources' array".into()))?;

        let state = ctx.snapshot_of::<ResearchState>().unwrap_or_default();
        let mut resources = state.resources;

        for item in raw {
            resources.push(Resource {
                id: item["id"].as_str().unwrap_or_default().to_string(),
                url: item["url"].as_str().unwrap_or_default().to_string(),
                title: item["title"].as_str().unwrap_or_default().to_string(),
                description: item["description"].as_str().unwrap_or_default().to_string(),
            });
        }

        let mut logs = state.logs;
        logs.push(LogEntry {
            message: format!("Extracted {} resources", raw.len()),
            level: "info".into(),
            step: "extract_resources".into(),
        });

        Ok(ToolExecutionEffect::new(ToolResult::success(
            "extract_resources",
            json!({ "added": raw.len(), "total": resources.len() }),
        ))
        .with_action(AnyStateAction::new::<ResearchState>(
            ResearchAction::SetResources(resources),
        ))
        .with_action(AnyStateAction::new::<ResearchState>(
            ResearchAction::SetLogs(logs),
        )))
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
