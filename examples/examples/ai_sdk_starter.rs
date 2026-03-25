//! AI SDK v6 integration pattern using the awaken framework.
//!
//! Demonstrates:
//! - AI SDK v6 protocol endpoint (`/v1/ai-sdk`) for Vercel AI SDK clients
//! - Tool-call streaming via the AI SDK wire format
//! - `useChat` / `useCompletion` compatible backend
//!
//! The AI SDK client (React) connects to `/v1/ai-sdk` and receives the
//! streaming data protocol events that drive `useChat` state updates.

use std::sync::Arc;

use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::storage::ThreadRunStore;
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult,
};
use awaken_contract::registry_spec::AgentSpec;
use awaken_ext_permission::PermissionPlugin;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::engine::{GenaiExecutor, MockLlmExecutor};
use awaken_runtime::plugins::Plugin;
use awaken_runtime::registry::traits::ModelEntry;
use awaken_server::app::{AppState, ServerConfig};
use awaken_server::routes::build_router;
use awaken_stores::InMemoryStore;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// AI SDK-oriented tools
// ---------------------------------------------------------------------------

/// Performs a mock web search, returning results for `useChat` rendering.
struct WebSearchTool;

#[async_trait::async_trait]
impl Tool for WebSearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "web_search",
            "Web Search",
            "Search the web and return summarized results.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" }
            },
            "required": ["query"]
        }))
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'query'".into()))?;

        Ok(ToolResult::success(
            "web_search",
            json!({
                "query": query,
                "results": [
                    {
                        "title": format!("Result 1 for: {query}"),
                        "url": "https://example.com/result1",
                        "snippet": "This is a sample search result."
                    },
                    {
                        "title": format!("Result 2 for: {query}"),
                        "url": "https://example.com/result2",
                        "snippet": "Another relevant search result."
                    }
                ]
            }),
        ))
    }
}

/// Returns a structured data object for AI SDK client-side rendering.
struct GetStructuredDataTool;

#[async_trait::async_trait]
impl Tool for GetStructuredDataTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "get_structured_data",
            "Get Structured Data",
            "Return structured data for client-side rendering via AI SDK.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "data_type": {
                    "type": "string",
                    "description": "Type of data: 'chart', 'table', or 'card'"
                },
                "title": { "type": "string", "description": "Title of the data" }
            },
            "required": ["data_type", "title"]
        }))
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let data_type = args["data_type"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'data_type'".into()))?;
        let title = args["title"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'title'".into()))?;

        let sample_data = match data_type {
            "chart" => json!({
                "type": "chart",
                "title": title,
                "labels": ["Jan", "Feb", "Mar", "Apr"],
                "values": [10, 25, 18, 32]
            }),
            "table" => json!({
                "type": "table",
                "title": title,
                "columns": ["Name", "Value", "Status"],
                "rows": [
                    ["Alpha", "42", "Active"],
                    ["Beta", "17", "Pending"]
                ]
            }),
            _ => json!({
                "type": "card",
                "title": title,
                "content": "Structured data card rendered by AI SDK."
            }),
        };

        Ok(ToolResult::success("get_structured_data", sample_data))
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(true).init();

    let addr = std::env::var("AWAKEN_HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".into());

    let agent_spec = AgentSpec {
        id: "ai-sdk-agent".into(),
        model: "default".into(),
        system_prompt: concat!(
            "You are an AI SDK v6 compatible assistant. You can search the web ",
            "and return structured data for client-side rendering.\n\n",
            "Use web_search when users ask questions about topics. Use ",
            "get_structured_data to provide charts, tables, or cards."
        )
        .into(),
        max_rounds: 10,
        plugin_ids: vec!["permission".into()],
        ..Default::default()
    };

    let tools: Vec<(&str, Arc<dyn Tool>)> = vec![
        ("web_search", Arc::new(WebSearchTool)),
        ("get_structured_data", Arc::new(GetStructuredDataTool)),
    ];

    let (provider, model_name): (Arc<dyn LlmExecutor>, String) =
        if std::env::var("OPENAI_API_KEY").is_ok() {
            let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
            (Arc::new(GenaiExecutor::new()), model)
        } else if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            let model = std::env::var("ANTHROPIC_MODEL")
                .unwrap_or_else(|_| "claude-sonnet-4-20250514".into());
            (Arc::new(GenaiExecutor::new()), model)
        } else {
            tracing::warn!("No LLM API key found, using mock executor");
            (Arc::new(MockLlmExecutor::new()), "mock".into())
        };

    let mut builder = AgentRuntimeBuilder::new()
        .with_agent_spec(agent_spec)
        .with_provider("default", provider)
        .with_model(
            "default",
            ModelEntry {
                provider: "default".into(),
                model_name,
            },
        );
    for (id, tool) in tools {
        builder = builder.with_tool(id, tool);
    }
    builder = builder.with_plugin("permission", Arc::new(PermissionPlugin) as Arc<dyn Plugin>);

    let store = Arc::new(InMemoryStore::new());
    builder = builder.with_thread_run_store(store.clone() as Arc<dyn ThreadRunStore>);

    let runtime = builder.build().expect("failed to build runtime");
    let runtime = Arc::new(runtime);

    let resolver = runtime.resolver_arc();

    let mailbox_store = Arc::new(awaken_stores::InMemoryMailboxStore::new());
    let mailbox = Arc::new(awaken_server::mailbox::Mailbox::new(
        runtime.clone(),
        mailbox_store as Arc<dyn awaken_contract::MailboxStore>,
        format!("ai-sdk:{}", std::process::id()),
        awaken_server::mailbox::MailboxConfig::default(),
    ));
    let state = AppState::new(
        runtime,
        mailbox,
        store.clone() as Arc<dyn ThreadRunStore>,
        resolver,
        ServerConfig {
            address: addr.clone(),
            ..Default::default()
        },
    );

    let app = build_router().with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind");
    tracing::info!(
        addr = %listener.local_addr().unwrap(),
        ai_sdk = "/v1/ai-sdk",
        "AI SDK v6 starter listening (connect AI SDK useChat to the endpoint)"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server crashed");
}
