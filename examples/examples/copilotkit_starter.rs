//! CopilotKit integration pattern using the awaken framework.
//!
//! Demonstrates:
//! - AG-UI protocol endpoint (`/v1/ag-ui`) for CopilotKit clients
//! - Shared state tool that emits structured state updates
//! - State-driven UI rendering via tool result metadata
//!
//! CopilotKit connects to the AG-UI endpoint and receives events in the
//! AG-UI wire format, which it uses to drive React component updates.

use std::sync::Arc;

use awaken_contract::contract::storage::{MailboxStore, RunStore, ThreadStore};
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult,
};
use awaken_contract::registry_spec::AgentSpec;
use awaken_ext_permission::PermissionPlugin;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::plugins::Plugin;
use awaken_server::app::{AppState, ServerConfig};
use awaken_server::routes::build_router;
use awaken_stores::InMemoryStore;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// CopilotKit-oriented tools
// ---------------------------------------------------------------------------

/// Updates shared UI state that CopilotKit renders via `useCopilotReadable`.
struct SetAgentStateTool;

#[async_trait::async_trait]
impl Tool for SetAgentStateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "set_agent_state",
            "Set Agent State",
            "Update the shared agent state for the CopilotKit frontend.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "key":   { "type": "string",  "description": "State key" },
                "value": { "description": "State value (any JSON)" }
            },
            "required": ["key", "value"]
        }))
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let key = args["key"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'key'".into()))?;
        let value = &args["value"];

        Ok(ToolResult::success(
            "set_agent_state",
            json!({
                "key": key,
                "value": value,
                "applied": true
            }),
        ))
    }
}

/// Suggests an action the frontend can render as a CopilotKit action button.
struct SuggestActionTool;

#[async_trait::async_trait]
impl Tool for SuggestActionTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "suggest_action",
            "Suggest Action",
            "Suggest a UI action that the CopilotKit frontend can render.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "label":       { "type": "string", "description": "Button label" },
                "description": { "type": "string", "description": "Action description" },
                "action_id":   { "type": "string", "description": "Unique action identifier" }
            },
            "required": ["label", "action_id"]
        }))
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let label = args["label"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'label'".into()))?;
        let action_id = args["action_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'action_id'".into()))?;

        Ok(ToolResult::success(
            "suggest_action",
            json!({
                "label": label,
                "action_id": action_id,
                "description": args["description"].as_str().unwrap_or(""),
                "suggested": true
            }),
        ))
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
        id: "copilotkit-agent".into(),
        model: "default".into(),
        system_prompt: concat!(
            "You are a CopilotKit-powered assistant. You can update the shared UI state ",
            "using set_agent_state and suggest interactive actions via suggest_action.\n\n",
            "When users ask about topics, update the state with relevant data so the ",
            "frontend can render it. Suggest follow-up actions when appropriate."
        )
        .into(),
        max_rounds: 10,
        plugin_ids: vec!["permission".into()],
        ..Default::default()
    };

    let tools: Vec<(&str, Arc<dyn Tool>)> = vec![
        ("set_agent_state", Arc::new(SetAgentStateTool)),
        ("suggest_action", Arc::new(SuggestActionTool)),
    ];

    let mut builder = AgentRuntimeBuilder::new().with_agent_spec(agent_spec);
    for (id, tool) in tools {
        builder = builder.with_tool(id, tool);
    }
    builder = builder.with_plugin("permission", Arc::new(PermissionPlugin) as Arc<dyn Plugin>);

    let runtime = builder.build().expect("failed to build runtime");
    let runtime = Arc::new(runtime);

    let store = Arc::new(InMemoryStore::new());
    let resolver = runtime.resolver_arc();

    let state = AppState::new(
        runtime,
        store.clone() as Arc<dyn ThreadStore>,
        store.clone() as Arc<dyn RunStore>,
        store.clone() as Arc<dyn MailboxStore>,
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
        ag_ui = "/v1/ag-ui",
        "CopilotKit starter listening (connect CopilotKit to the AG-UI endpoint)"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server crashed");
}
