mod frontend_tools;
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
use tirea_agentos::extensions::permission::{PermissionPlugin, ToolPolicyPlugin};
use tirea_agentos::orchestrator::{
    AgentDefinition, AgentOsBuilder, StopConditionSpec, ToolExecutionMode,
};
use tirea_agentos::runtime::loop_runner::tool_map_from_arc;
use tirea_agentos_server::http::{self, AppState};
use tirea_agentos_server::protocol;
use tirea_extension_mcp::McpToolRegistryManager;
use tirea_store_adapters::FileStore;
use tools::{
    AppendNoteTool, AskUserQuestionTool, FailingTool, FinishTool, GetStockPriceTool,
    GetWeatherTool, ProgressDemoTool, ServerInfoTool, SetBackgroundColorTool,
};
use tower_http::cors::{Any, CorsLayer};
use frontend_tools::FrontendToolPlugin;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, env = "AGENTOS_HTTP_ADDR", default_value = "127.0.0.1:38080")]
    http_addr: String,

    #[arg(long, env = "AGENTOS_STORAGE_DIR", default_value = "./sessions")]
    storage_dir: PathBuf,

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

    let base_prompt = format!(
        "{}\n\
For deterministic tests, obey these exact directives:\n\
- If message contains RUN_WEATHER_TOOL, call get_weather with location=Tokyo.\n\
- If message contains RUN_STOCK_TOOL, call get_stock_price with symbol=AAPL.\n\
- If message contains RUN_APPEND_NOTE, call append_note with the remaining sentence as note.\n\
- If message contains RUN_SERVER_INFO, call serverInfo.\n\
- If message contains RUN_FAILING_TOOL, call failingTool.\n\
- If message contains RUN_PROGRESS_DEMO, call progress_demo.\n\
- If message contains RUN_ASK_USER_TOOL, call askUserQuestion with a short question.\n\
- If message contains RUN_BG_TOOL, call set_background_color with colors ['#dbeafe','#dcfce7'].\n\
- If message contains RUN_FINISH_TOOL, call finish with summary and then stop.",
        args.system_prompt
    );

    let default_agent = AgentDefinition {
        id: "default".to_string(),
        model: args.model.clone(),
        system_prompt: base_prompt.clone(),
        max_rounds: args.max_rounds,
        tool_execution_mode: ToolExecutionMode::ParallelStreaming,
        behavior_ids: vec!["frontend_tools".to_string()],
        ..Default::default()
    };
    let permission_agent = AgentDefinition {
        id: "permission".to_string(),
        model: args.model.clone(),
        system_prompt: base_prompt.clone(),
        max_rounds: args.max_rounds,
        tool_execution_mode: ToolExecutionMode::ParallelBatchApproval,
        behavior_ids: vec![
            "tool_policy".to_string(),
            "permission".to_string(),
            "frontend_tools".to_string(),
        ],
        ..Default::default()
    };
    let stopper_agent = AgentDefinition {
        id: "stopper".to_string(),
        model: args.model.clone(),
        system_prompt: base_prompt,
        max_rounds: args.max_rounds,
        behavior_ids: vec!["frontend_tools".to_string()],
        stop_condition_specs: vec![StopConditionSpec::StopOnTool {
            tool_name: "finish".to_string(),
        }],
        tool_execution_mode: ToolExecutionMode::ParallelStreaming,
        ..Default::default()
    };

    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(GetWeatherTool),
        Arc::new(GetStockPriceTool),
        Arc::new(AppendNoteTool),
        Arc::new(ServerInfoTool),
        Arc::new(FailingTool),
        Arc::new(FinishTool),
        Arc::new(ProgressDemoTool),
        Arc::new(AskUserQuestionTool),
        Arc::new(SetBackgroundColorTool),
    ];
    let mut tool_map: HashMap<String, Arc<dyn Tool>> = tool_map_from_arc(tools);

    let _mcp_manager = if let Some(ref cmd_str) = args.mcp_server_cmd {
        let parts: Vec<&str> = cmd_str.split_whitespace().collect();
        let (command, cmd_args) = parts
            .split_first()
            .expect("MCP_SERVER_CMD must not be empty");
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
        .with_agent("default", default_agent)
        .with_agent("permission", permission_agent)
        .with_agent("stopper", stopper_agent)
        .with_tools(tool_map)
        .with_agent_state_store(file_store.clone() as Arc<dyn ThreadStore>);

    let plugins: Vec<(String, Arc<dyn AgentBehavior>)> = vec![
        ("tool_policy".to_string(), Arc::new(ToolPolicyPlugin)),
        ("permission".to_string(), Arc::new(PermissionPlugin)),
        ("frontend_tools".to_string(), Arc::new(FrontendToolPlugin::new())),
    ];
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
