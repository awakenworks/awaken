pub mod frontend_tools;
pub mod state;
pub mod tools;

use clap::Parser;
use mcp::transport::McpServerConnectionConfig;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tirea_agentos::composition::StopConditionSpec;
use tirea_agentos::composition::{
    AgentDefinition, AgentDefinitionSpec, AgentOsBuilder, ToolExecutionMode, ToolRegistry,
};
use tirea_agentos::contracts::runtime::behavior::AgentBehavior;
use tirea_agentos::contracts::runtime::tool_call::Tool;
use tirea_agentos::contracts::storage::{MailboxStore, ThreadReader, ThreadStore};
use tirea_agentos::extensions::permission::{PermissionPlugin, ToolPolicyPlugin};
use tirea_agentos_server::http::{self, AppState};
use tirea_agentos_server::protocol;
use tirea_agentos_server::service::MailboxService;
use tirea_extension_a2ui::{A2uiPlugin, A2uiRenderTool};
use tirea_extension_mcp::McpToolRegistryManager;
use tirea_store_adapters::FileStore;
use tower_http::cors::{Any, CorsLayer};

use crate::research::tools::{
    DeleteResourcesTool, ExtractResourcesTool, SearchTool, SetQuestionTool, WriteReportTool,
};
use crate::starter_backend::frontend_tools::FrontendToolPlugin;
use crate::starter_backend::tools::{
    AppendNoteTool, AskUserQuestionTool, FailingTool, FinishTool, GetStockPriceTool,
    GetWeatherTool, ProgressDemoTool, ServerInfoTool, SetBackgroundColorTool,
};
use crate::tool_map_from_arc;
use crate::travel::tools::{
    AddTripTool, DeleteTripTool, SearchPlacesTool, SelectTripTool, UpdateTripTool,
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,genai=warn,tirea_agentos=info".parse().unwrap()),
        )
        .with_target(true)
        .init();

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
    let travel_agent = AgentDefinition {
        id: "travel".to_string(),
        model: args.model.clone(),
        system_prompt: concat!(
            "You are a travel planning assistant. Help users plan trips by adding, ",
            "updating, and searching for places of interest. Use the provided tools ",
            "to manage trips and find destinations.\n\n",
            "When the user asks to plan a trip, create it with add_trips, then ",
            "search_for_places to find interesting locations. Always select the ",
            "active trip with select_trip after creating it."
        )
        .into(),
        max_rounds: args.max_rounds,
        behavior_ids: vec!["permission".to_string()],
        ..Default::default()
    };
    let research_agent = AgentDefinition {
        id: "research".to_string(),
        model: args.model.clone(),
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
        max_rounds: args.max_rounds,
        behavior_ids: vec!["permission".to_string()],
        ..Default::default()
    };

    let a2ui_agent = AgentDefinition {
        id: "a2ui".to_string(),
        model: args.model.clone(),
        system_prompt: concat!(
            "You are an A2UI demo assistant. When the user asks you to render UI, ",
            "use the render_a2ui tool to send A2UI declarative UI messages.\n\n",
            "Deterministic compatibility directives:\n",
            "- If message contains RUN_A2UI_TOOL, call render_a2ui with a createSurface ",
            "for surfaceId \"demo\" and catalogId \"https://a2ui.org/specification/v0_9/basic_catalog.json\", ",
            "then updateComponents with a Card containing a Text component saying \"Hello A2UI\", ",
            "then updateDataModel with an empty root object."
        )
        .into(),
        max_rounds: args.max_rounds,
        behavior_ids: vec!["a2ui".to_string()],
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
        Arc::new(AddTripTool),
        Arc::new(UpdateTripTool),
        Arc::new(DeleteTripTool),
        Arc::new(SelectTripTool),
        Arc::new(SearchPlacesTool),
        Arc::new(SearchTool),
        Arc::new(WriteReportTool),
        Arc::new(SetQuestionTool),
        Arc::new(DeleteResourcesTool),
        Arc::new(ExtractResourcesTool),
        Arc::new(A2uiRenderTool::new()),
    ];
    let tool_map: HashMap<String, Arc<dyn Tool>> = tool_map_from_arc(tools);
    let mut mcp_tool_registry: Option<Arc<dyn ToolRegistry>> = None;

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
                let registry = manager.registry();
                eprintln!("MCP: connected, discovered {} tools", registry.len());
                mcp_tool_registry = Some(Arc::new(registry));
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
        .with_agent_spec(AgentDefinitionSpec::local(default_agent))
        .with_tools(tool_map)
        .with_agent_state_store(file_store.clone() as Arc<dyn ThreadStore>);
    if let Some(registry) = mcp_tool_registry {
        builder = builder.with_tool_registry(registry);
    }

    if default_id != "permission" {
        builder = builder.with_agent_spec(AgentDefinitionSpec::local(permission_agent));
    }
    if default_id != "stopper" {
        builder = builder.with_agent_spec(AgentDefinitionSpec::local(stopper_agent));
    }
    if default_id != "travel" {
        builder = builder.with_agent_spec(AgentDefinitionSpec::local(travel_agent));
    }
    if default_id != "research" {
        builder = builder.with_agent_spec(AgentDefinitionSpec::local(research_agent));
    }
    if default_id != "a2ui" {
        builder = builder.with_agent_spec(AgentDefinitionSpec::local(a2ui_agent));
    }

    let plugins: Vec<(String, Arc<dyn AgentBehavior>)> = vec![
        ("tool_policy".to_string(), Arc::new(ToolPolicyPlugin)),
        ("permission".to_string(), Arc::new(PermissionPlugin)),
        (
            "frontend_tools".to_string(),
            Arc::new(FrontendToolPlugin::new()),
        ),
        (
            "a2ui".to_string(),
            Arc::new(A2uiPlugin::with_catalog_id(
                "https://a2ui.org/specification/v0_9/basic_catalog.json",
            )),
        ),
    ];
    for (id, plugin) in plugins {
        builder = builder.with_registered_behavior(id, plugin);
    }

    let os = builder.build().expect("failed to build AgentOs");
    let os = Arc::new(os);
    let read_store: Arc<dyn ThreadReader> = file_store.clone();
    let mailbox_store: Arc<dyn MailboxStore> = file_store;

    let mailbox_svc = Arc::new(MailboxService::new(
        os.clone(),
        mailbox_store,
        "starter-backend-mailbox",
    ));
    let _ = mailbox_svc.recover().await;
    tokio::spawn(mailbox_svc.clone().run_sweep_forever());

    let mut app = axum::Router::new()
        .merge(http::health_routes())
        .merge(http::thread_routes())
        .merge(http::run_routes())
        .merge(protocol::a2a::http::well_known_routes())
        .nest("/v1/ag-ui", protocol::ag_ui::http::routes())
        .nest("/v1/ai-sdk", protocol::ai_sdk_v6::http::routes())
        .nest("/v1/a2a", protocol::a2a::http::routes())
        .with_state(AppState::new(os, read_store, mailbox_svc));

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
