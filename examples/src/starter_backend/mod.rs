pub mod frontend_tools;
pub mod state;
pub mod tools;

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
use tirea_agentos_server::run_service::init_run_service;
use tirea_extension_mcp::McpToolRegistryManager;
use tirea_store_adapters::{FileRunStore, FileStore};
use tower_http::cors::{Any, CorsLayer};

use crate::starter_backend::frontend_tools::FrontendToolPlugin;
use crate::starter_backend::tools::{
    AppendNoteTool, AskUserQuestionTool, FailingTool, FinishTool, GetStockPriceTool,
    GetWeatherTool, ProgressDemoTool, ServerInfoTool, SetBackgroundColorTool,
};

#[derive(Debug, Clone, Parser)]
pub struct StarterBackendArgs {
    #[arg(long, env = "AGENTOS_HTTP_ADDR", default_value = "127.0.0.1:38080")]
    pub http_addr: String,

    #[arg(long, env = "AGENTOS_STORAGE_DIR", default_value = "./sessions")]
    pub storage_dir: PathBuf,

    #[arg(long, env = "AGENT_ID", default_value = "default")]
    pub agent_id: String,

    #[arg(long, env = "AGENT_MODEL", default_value = "deepseek-chat")]
    pub model: String,

    #[arg(long, env = "AGENT_MAX_ROUNDS", default_value_t = 8)]
    pub max_rounds: usize,

    #[arg(
        long,
        env = "AGENT_SYSTEM_PROMPT",
        default_value = "You are the with-tirea starter assistant. Use tools proactively when users ask for weather, stock quotes, or note updates."
    )]
    pub system_prompt: String,

    #[arg(long, env = "MCP_SERVER_CMD")]
    pub mcp_server_cmd: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StarterBackendConfig {
    pub service_name: String,
    pub enable_cors: bool,
}

impl StarterBackendConfig {
    pub fn new(service_name: impl Into<String>, enable_cors: bool) -> Self {
        Self {
            service_name: service_name.into(),
            enable_cors,
        }
    }
}

pub async fn serve_starter_backend(args: StarterBackendArgs, config: StarterBackendConfig) {
    let base_prompt = format!(
        "{}\n\
Tool routing contract for this demo:\n\
- When user explicitly asks for a tool by name, call that exact tool name (do not substitute).\n\
- Frontend tools are askUserQuestion and set_background_color. If requested, call them directly.\n\
\n\
Deterministic compatibility directives:\n\
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

    let default_id = if args.agent_id.trim().is_empty() {
        "default".to_string()
    } else {
        args.agent_id.trim().to_string()
    };
    let default_agent = AgentDefinition {
        id: default_id.clone(),
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
        Arc::new(ServerInfoTool::new(config.service_name)),
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

    let run_store_dir = args.storage_dir.join("runs");
    let file_store = Arc::new(FileStore::new(args.storage_dir));
    let _ = init_run_service(Arc::new(FileRunStore::new(run_store_dir)));
    let default_agent_id = default_agent.id.clone();
    let mut builder = AgentOsBuilder::new()
        .with_agent(&default_agent_id, default_agent)
        .with_tools(tool_map)
        .with_agent_state_store(file_store.clone() as Arc<dyn ThreadStore>);

    if default_id != "permission" {
        builder = builder.with_agent("permission", permission_agent);
    }
    if default_id != "stopper" {
        builder = builder.with_agent("stopper", stopper_agent);
    }

    let plugins: Vec<(String, Arc<dyn AgentBehavior>)> = vec![
        ("tool_policy".to_string(), Arc::new(ToolPolicyPlugin)),
        ("permission".to_string(), Arc::new(PermissionPlugin)),
        (
            "frontend_tools".to_string(),
            Arc::new(FrontendToolPlugin::new()),
        ),
    ];
    for (id, plugin) in plugins {
        builder = builder.with_registered_behavior(id, plugin);
    }

    let os = builder.build().expect("failed to build AgentOs");
    let read_store: Arc<dyn ThreadReader> = file_store;

    let mut app = axum::Router::new()
        .merge(http::health_routes())
        .merge(http::thread_routes())
        .merge(http::run_routes())
        .merge(protocol::a2a::http::well_known_routes())
        .nest("/v1/ag-ui", protocol::ag_ui::http::routes())
        .nest("/v1/ai-sdk", protocol::ai_sdk_v6::http::routes())
        .nest("/v1/a2a", protocol::a2a::http::routes())
        .with_state(AppState {
            os: Arc::new(os),
            read_store,
        });

    if config.enable_cors {
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);
        app = app.layer(cors);
    }

    let listener = tokio::net::TcpListener::bind(&args.http_addr)
        .await
        .expect("failed to bind server listener");
    eprintln!(
        "{} listening on {}",
        default_id,
        listener.local_addr().unwrap()
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server crashed");
}
