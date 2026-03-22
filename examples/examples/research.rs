//! Research agent demonstrating multi-protocol support (AG-UI + AI SDK v6),
//! custom tools for information gathering, and server startup with both endpoints.

use std::sync::Arc;

use awaken_contract::contract::storage::{MailboxStore, RunStore, ThreadStore};
use awaken_contract::contract::tool::Tool;
use awaken_contract::registry_spec::AgentSpec;
use awaken_examples::research::tools::*;
use awaken_ext_permission::PermissionPlugin;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::plugins::Plugin;
use awaken_server::app::{AppState, ServerConfig};
use awaken_server::routes::build_router;
use awaken_stores::InMemoryStore;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(true).init();

    let agent_spec = AgentSpec {
        id: "research".into(),
        model: "default".into(),
        system_prompt: concat!(
            "You are a research assistant. Help users research topics by searching ",
            "the web, extracting resources, and writing comprehensive reports.\n\n",
            "Workflow:\n",
            "1. Set the research question with set_research_question\n",
            "2. Search for information with search\n",
            "3. Extract useful resources with extract_resources\n",
            "4. Write a report with write_report\n\n",
            "Always keep the user informed of your progress."
        )
        .into(),
        max_rounds: 15,
        plugin_ids: vec!["permission".into()],
        ..Default::default()
    };

    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(SearchTool),
        Arc::new(WriteReportTool),
        Arc::new(SetQuestionTool),
        Arc::new(DeleteResourcesTool),
        Arc::new(ExtractResourcesTool),
    ];

    let mut builder = AgentRuntimeBuilder::new().with_agent_spec(agent_spec);

    for tool in &tools {
        let id = tool.descriptor().id.clone();
        builder = builder.with_tool(id, Arc::clone(tool));
    }

    builder = builder.with_plugin("permission", Arc::new(PermissionPlugin) as Arc<dyn Plugin>);

    let runtime = builder.build().expect("failed to build runtime");
    let runtime = Arc::new(runtime);

    let store = Arc::new(InMemoryStore::new());
    let resolver = runtime.resolver_arc();

    let addr = std::env::var("AWAKEN_HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());

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

    // Build router with all protocol endpoints:
    // - /health
    // - /v1/threads, /v1/runs (core API)
    // - /v1/ai-sdk/* (AI SDK v6 protocol)
    // - /v1/ag-ui/* (AG-UI protocol)
    // - /v1/a2a/* (A2A protocol)
    let app = build_router().with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind");
    tracing::info!(
        addr = %listener.local_addr().unwrap(),
        protocols = "AG-UI, AI SDK v6, A2A",
        "research agent listening"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server crashed");
}
