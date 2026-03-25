//! Minimal HTTP server template using axum and the awaken framework.
//!
//! Demonstrates:
//! - Axum server with awaken-server routes
//! - Permission + Observability plugins
//! - InMemoryStore for simplicity
//! - Health check + thread + run endpoints

use std::sync::Arc;

use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::storage::ThreadRunStore;
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult,
};
use awaken_contract::registry_spec::AgentSpec;
use awaken_ext_observability::{InMemorySink, ObservabilityPlugin};
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
// Demo tools
// ---------------------------------------------------------------------------

/// Returns mock weather data for a location.
struct GetWeatherTool;

#[async_trait::async_trait]
impl Tool for GetWeatherTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "get_weather",
            "Get Weather",
            "Get weather details for a location.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "location": { "type": "string", "description": "City or location name" }
            },
            "required": ["location"]
        }))
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let location = args["location"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'location'".into()))?;

        Ok(ToolResult::success(
            "get_weather",
            json!({
                "location": location,
                "temperature_f": 70,
                "condition": "Sunny",
                "humidity_pct": 45
            }),
        ))
    }
}

/// Returns a demo stock quote.
struct GetStockPriceTool;

#[async_trait::async_trait]
impl Tool for GetStockPriceTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "get_stock_price",
            "Get Stock Price",
            "Return a demo stock quote for the provided ticker symbol.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Ticker symbol, e.g. AAPL" }
            },
            "required": ["symbol"]
        }))
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let symbol = args["symbol"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'symbol'".into()))?
            .to_uppercase();

        let price = match symbol.as_str() {
            "AAPL" => 188.42_f64,
            "MSFT" => 421.10_f64,
            "NVDA" => 131.75_f64,
            _ => 99.99_f64,
        };

        Ok(ToolResult::success(
            "get_stock_price",
            json!({
                "symbol": symbol,
                "price_usd": price,
                "source": "starter-demo"
            }),
        ))
    }
}

/// Returns backend server identity and unix timestamp.
struct ServerInfoTool {
    service_name: String,
}

#[async_trait::async_trait]
impl Tool for ServerInfoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "server_info",
            "Server Info",
            "Return backend server identity and unix timestamp.",
        )
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Ok(ToolResult::success(
            "server_info",
            json!({ "name": self.service_name, "timestamp": ts }),
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
        id: "starter".into(),
        model: "default".into(),
        system_prompt: concat!(
            "You are the awaken starter assistant. Use tools proactively ",
            "when users ask for weather, stock quotes, or server information."
        )
        .into(),
        max_rounds: 8,
        plugin_ids: vec!["permission".into(), "observability".into()],
        ..Default::default()
    };

    let tools: Vec<(&str, Arc<dyn Tool>)> = vec![
        ("get_weather", Arc::new(GetWeatherTool)),
        ("get_stock_price", Arc::new(GetStockPriceTool)),
        (
            "server_info",
            Arc::new(ServerInfoTool {
                service_name: "awaken-starter".into(),
            }),
        ),
    ];

    let observability = ObservabilityPlugin::new(InMemorySink::new())
        .with_model("default")
        .with_provider("default");

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
    builder = builder.with_plugin("observability", Arc::new(observability) as Arc<dyn Plugin>);

    let store = Arc::new(InMemoryStore::new());
    builder = builder.with_thread_run_store(store.clone() as Arc<dyn ThreadRunStore>);

    let runtime = builder.build().expect("failed to build runtime");
    let runtime = Arc::new(runtime);

    let resolver = runtime.resolver_arc();

    let mailbox_store = Arc::new(awaken_stores::InMemoryMailboxStore::new());
    let mailbox = Arc::new(awaken_server::mailbox::Mailbox::new(
        runtime.clone(),
        mailbox_store as Arc<dyn awaken_contract::MailboxStore>,
        format!("starter:{}", std::process::id()),
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
        endpoints = "/health, /v1/threads, /v1/runs, /v1/ai-sdk, /v1/ag-ui",
        "starter backend listening"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server crashed");
}
