mod state;
mod tools;

use clap::Parser;
use mcp::transport::McpServerConnectionConfig;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tirea_agentos::contracts::runtime::behavior::AgentBehavior;
use tirea_agentos::contracts::runtime::tool_call::Tool;
use tirea_agentos::contracts::storage::{ThreadReader, ThreadStore};
use tirea_agentos::orchestrator::{AgentDefinition, AgentOsBuilder, ToolExecutionMode};
use tirea_agentos::runtime::loop_runner::tool_map_from_arc;
use tirea_agentos_server::http::{self, AppState};
use tirea_agentos_server::protocol;
use tirea_extension_mcp::McpToolRegistryManager;
use tirea_store_adapters::FileStore;
use tools::{AppendNoteTool, GetStockPriceTool, GetWeatherTool};
use tower_http::cors::{Any, CorsLayer};

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, env = "AGENTOS_HTTP_ADDR", default_value = "127.0.0.1:38080")]
    http_addr: String,

    #[arg(long, env = "AGENTOS_STORAGE_DIR", default_value = "./sessions")]
    storage_dir: PathBuf,

    #[arg(long, env = "AGENT_ID", default_value = "default")]
    agent_id: String,

    #[arg(long, env = "AGENT_MODEL", default_value = "deepseek-chat")]
    model: String,

    #[arg(long, env = "AGENT_MAX_ROUNDS", default_value_t = 8)]
    max_rounds: usize,

    #[arg(
        long,
        env = "AGENT_SYSTEM_PROMPT",
        default_value = "You are the with-tirea starter assistant. Use tools proactively when users ask for weather, stock quotes, or note updates."
    )]
    system_prompt: String,

    #[arg(long, env = "MCP_SERVER_CMD")]
    mcp_server_cmd: Option<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let agent_def = AgentDefinition {
        id: args.agent_id.clone(),
        model: args.model.clone(),
        system_prompt: args.system_prompt,
        max_rounds: args.max_rounds,
        tool_execution_mode: ToolExecutionMode::ParallelStreaming,
        ..Default::default()
    };

    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(GetWeatherTool),
        Arc::new(GetStockPriceTool),
        Arc::new(AppendNoteTool),
    ];
    let mut tool_map: HashMap<String, Arc<dyn Tool>> = tool_map_from_arc(tools);

    let _mcp_manager = if let Some(ref cmd_str) = args.mcp_server_cmd {
        let parts: Vec<&str> = cmd_str.split_whitespace().collect();
        let (command, cmd_args) = parts.split_first().expect("MCP_SERVER_CMD must not be empty");
        let cfg = McpServerConnectionConfig::stdio(
            "mcp_demo",
            *command,
            cmd_args.iter().map(|s| s.to_string()).collect(),
        );
        match McpToolRegistryManager::connect([cfg]).await {
            Ok(manager) => {
                let mcp_tools = manager.registry().snapshot();
                eprintln!("MCP: connected, discovered {} tools", mcp_tools.len());
                tool_map.extend(mcp_tools);
                Some(manager)
            }
            Err(e) => {
                eprintln!("MCP: failed to connect: {e}");
                None
            }
        }
    } else {
        None
    };

    let file_store = Arc::new(FileStore::new(args.storage_dir));
    let mut builder = AgentOsBuilder::new()
        .with_agent(&args.agent_id, agent_def)
        .with_tools(tool_map)
        .with_agent_state_store(file_store.clone() as Arc<dyn ThreadStore>);

    let plugins: Vec<(String, Arc<dyn AgentBehavior>)> = Vec::new();
    for (id, plugin) in plugins {
        builder = builder.with_registered_behavior(id, plugin);
    }

    let os = builder.build().expect("failed to build AgentOs");
    let read_store: Arc<dyn ThreadReader> = file_store;

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let state = AppState {
        os: Arc::new(os),
        read_store,
    };
    let app = axum::Router::new()
        .merge(http::health_routes())
        .merge(http::thread_routes())
        .nest("/v1/ag-ui", protocol::ag_ui::http::routes())
        .nest("/v1/ai-sdk", protocol::ai_sdk_v6::http::routes())
        .with_state(state)
        .layer(cors);

    let listener = tokio::net::TcpListener::bind(&args.http_addr)
        .await
        .expect("failed to bind server listener");
    eprintln!(
        "ai-sdk-starter agent listening on {}",
        listener.local_addr().unwrap()
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server crashed");
}
