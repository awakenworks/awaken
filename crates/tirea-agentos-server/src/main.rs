use async_trait::async_trait;
use clap::Parser;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tirea_agentos::composition::{
    AgentConfig, AgentConfigEntry, AgentConfigError, AgentOsBuilder, LocalAgentConfig,
    ModelDefinition, ToolExecutionModeConfig,
};
use tirea_agentos::contracts::storage::{MailboxStore, ThreadReader, ThreadStore};
use tirea_agentos::runtime::AgentOs;
use tirea_agentos_server::nats::NatsConfig;
use tirea_agentos_server::service::{AppState, MailboxService};
use tirea_agentos_server::{http, protocol};
use tirea_contract::runtime::tool_call::tool::{Tool, ToolDescriptor, ToolError, ToolResult};
use tirea_contract::runtime::tool_call::ToolCallContext;
#[cfg(feature = "permission")]
use tirea_extension_permission::{PermissionPlugin, ToolPolicyPlugin};
use tirea_store_adapters::FileStore;

#[derive(Debug, Parser)]
#[command(name = "tirea-agentos-server")]
struct Args {
    #[arg(long, env = "AGENTOS_HTTP_ADDR", default_value = "127.0.0.1:8080")]
    http_addr: String,

    #[arg(long, env = "AGENTOS_STORAGE_DIR", default_value = "./threads")]
    storage_dir: PathBuf,

    #[arg(long, env = "AGENTOS_CONFIG")]
    config: Option<PathBuf>,

    #[arg(long, env = "AGENTOS_NATS_URL")]
    nats_url: Option<String>,

    /// TensorZero gateway URL (e.g. http://localhost:4000/openai/v1/).
    /// When set, all model calls are routed through TensorZero for observability.
    #[arg(long, env = "TENSORZERO_URL")]
    tensorzero_url: Option<String>,

    /// Print the canonical AGENTOS_CONFIG JSON Schema and exit.
    #[arg(long)]
    print_agent_config_schema: bool,
}

/// Simple backend tool that returns server information.
///
/// Used in E2E tests to exercise the Permission plugin against a real backend tool.
struct ServerInfoTool;

#[async_trait]
impl Tool for ServerInfoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "serverInfo",
            "Server Info",
            "Returns server name and current timestamp",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))
    }

    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Ok(ToolResult::success(
            "serverInfo",
            json!({
                "server": "tirea-agentos",
                "timestamp": now
            }),
        ))
    }
}

/// Backend tool that always fails with an error.
///
/// Used in E2E tests to verify error rendering in the frontend.
struct FailingTool;

#[async_trait]
impl Tool for FailingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "failingTool",
            "Failing Tool",
            "A tool that always fails. Use this to test error handling.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))
    }

    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Err(ToolError::ExecutionFailed(
            "This tool always fails for testing purposes".to_string(),
        ))
    }
}

/// Simple tool that signals the agent run should finish.
///
/// Used with `StopOnTool` stop condition to test early termination.
struct FinishTool;

#[async_trait]
impl Tool for FinishTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "finish",
            "Finish",
            "Call this tool to signal that you are done. Pass a brief summary.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "A brief summary of what was accomplished"
                }
            },
            "required": ["summary"],
            "additionalProperties": false
        }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let summary = args
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("done");
        Ok(ToolResult::success(
            "finish",
            json!({
                "status": "done",
                "summary": summary
            }),
        ))
    }
}

fn build_os(
    cfg: Option<AgentConfig>,
    tensorzero_url: Option<String>,
    write_store: Arc<dyn ThreadStore>,
) -> AgentOs {
    let mut builder = AgentOsBuilder::new().with_agent_state_store(write_store);
    #[cfg(feature = "permission")]
    {
        builder = builder
            .with_registered_behavior("tool_policy", Arc::new(ToolPolicyPlugin))
            .with_registered_behavior("permission", Arc::new(PermissionPlugin));
    }
    builder = builder.with_tools({
        let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        tools.insert("serverInfo".to_string(), Arc::new(ServerInfoTool));
        tools.insert("failingTool".to_string(), Arc::new(FailingTool));
        tools.insert("finish".to_string(), Arc::new(FinishTool));
        tools
    });

    let agents = match cfg {
        Some(c) => c.agents,
        None => vec![AgentConfigEntry::LegacyLocal(LocalAgentConfig {
            id: "default".to_string(),
            name: None,
            description: None,
            model: None,
            system_prompt: String::new(),
            max_rounds: None,
            tool_execution_mode: ToolExecutionModeConfig::ParallelStreaming,
            behavior_ids: Vec::new(),
            stop_condition_specs: Vec::new(),
            modes: Default::default(),
        })],
    };

    if agents.is_empty() {
        eprintln!("config has no agents");
        std::process::exit(2);
    }

    // When TensorZero URL is provided, register a custom provider and model mapping.
    if let Some(tz_url) = &tensorzero_url {
        let endpoint = tz_url.clone();
        let tz_client = genai::Client::builder()
            .with_service_target_resolver_fn(move |mut t: genai::ServiceTarget| {
                t.endpoint = genai::resolver::Endpoint::from_owned(&*endpoint);
                t.auth = genai::resolver::AuthData::from_single("tensorzero");
                Ok(t)
            })
            .build();
        builder = builder.with_provider("tz", tz_client);
        eprintln!("TensorZero provider registered at {tz_url}");
    }

    for agent in agents {
        // Map each local agent's model through TensorZero when configured.
        if tensorzero_url.is_some() {
            if let Some(model) = agent.local_model() {
                builder = builder.with_model(
                    model,
                    ModelDefinition::new("tz", "openai::tensorzero::function_name::agent_chat"),
                );
                eprintln!("  model '{model}' → TensorZero agent_chat");
            }
        }

        let spec = match agent.into_spec() {
            Ok(spec) => spec,
            Err(err) => {
                report_agent_config_error(err);
                std::process::exit(2);
            }
        };
        builder = builder.with_agent_spec(spec);
    }

    match builder.build() {
        Ok(os) => os,
        Err(e) => {
            eprintln!("failed to build AgentOs: {e}");
            std::process::exit(2);
        }
    }
}

fn report_agent_config_error(err: AgentConfigError) {
    eprintln!("invalid agent config: {err}");
}

fn render_agent_config_schema() -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&AgentConfig::json_schema())
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if args.print_agent_config_schema {
        match render_agent_config_schema() {
            Ok(schema) => {
                println!("{schema}");
                return;
            }
            Err(err) => {
                eprintln!("failed to serialize agent config schema: {err}");
                std::process::exit(2);
            }
        }
    }

    let cfg = match args.config.as_ref() {
        Some(path) => {
            let raw = match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("failed to read config {}: {e}", path.display());
                    std::process::exit(2);
                }
            };
            let parsed = match AgentConfig::from_json_str(&raw) {
                Ok(c) => c,
                Err(e) => {
                    report_agent_config_error(e);
                    std::process::exit(2);
                }
            };
            Some(parsed)
        }
        None => None,
    };

    let file_store = Arc::new(FileStore::new(args.storage_dir));
    let write_store: Arc<dyn ThreadStore> = file_store.clone();
    let read_store: Arc<dyn ThreadReader> = file_store.clone();
    let mailbox_store: Arc<dyn MailboxStore> = file_store.clone();
    let os = Arc::new(build_os(cfg, args.tensorzero_url, write_store));
    let mailbox_svc = Arc::new(MailboxService::new(
        os.clone(),
        mailbox_store,
        "http-server-mailbox",
    ));
    let _ = mailbox_svc.recover().await;
    tokio::spawn(mailbox_svc.clone().run_sweep_forever());

    let app = axum::Router::new()
        .merge(http::health_routes())
        .merge(http::thread_routes())
        .merge(http::run_routes())
        .merge(protocol::a2a::http::well_known_routes())
        .nest("/v1/ag-ui", protocol::ag_ui::http::routes())
        .nest("/v1/ai-sdk", protocol::ai_sdk_v6::http::routes())
        .nest("/v1/a2a", protocol::a2a::http::routes())
        .with_state(AppState::new(os.clone(), read_store, mailbox_svc.clone()));

    if let Some(nats_url) = args.nats_url.clone() {
        let nats_config = NatsConfig::new(nats_url);
        match nats_config.connect().await {
            Ok(transport) => {
                let os_for_agui = os.clone();
                let os_for_aisdk = os.clone();
                let svc_for_agui = mailbox_svc.clone();
                let svc_for_aisdk = mailbox_svc.clone();
                let agui_transport = transport.clone();
                let agui_subject = nats_config.ag_ui_subject.clone();
                let aisdk_subject = nats_config.ai_sdk_subject;
                tokio::spawn(async move {
                    if let Err(e) = protocol::ag_ui::nats::serve(
                        agui_transport,
                        os_for_agui,
                        svc_for_agui,
                        agui_subject,
                    )
                    .await
                    {
                        eprintln!("nats ag-ui gateway stopped: {}", e);
                    }
                });
                tokio::spawn(async move {
                    if let Err(e) = protocol::ai_sdk_v6::nats::serve(
                        transport,
                        os_for_aisdk,
                        svc_for_aisdk,
                        aisdk_subject,
                    )
                    .await
                    {
                        eprintln!("nats ai-sdk gateway stopped: {}", e);
                    }
                });
            }
            Err(e) => {
                eprintln!("nats connect failed: {}", e);
            }
        }
    }

    let listener = tokio::net::TcpListener::bind(&args.http_addr)
        .await
        .expect("failed to bind http listener");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("http server crashed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use tirea_store_adapters::MemoryStore;

    #[test]
    fn render_agent_config_schema_outputs_agents_object() {
        let schema = render_agent_config_schema().expect("schema should serialize");
        let schema_json: Value =
            serde_json::from_str(&schema).expect("schema should be valid JSON");
        assert_eq!(schema_json["type"], json!("object"));
        assert!(schema_json["properties"]["agents"].is_object());
    }

    #[test]
    fn build_os_accepts_remote_a2a_agents_from_config() {
        let os = build_os(
            Some(AgentConfig {
                agents: serde_json::from_value(json!([
                    {
                        "kind": "a2a",
                        "id": "researcher",
                        "name": "Researcher",
                        "description": "Remote research agent",
                        "endpoint": "https://example.test/v1/a2a",
                        "remote_agent_id": "remote-researcher",
                        "poll_interval_ms": 80
                    }
                ]))
                .expect("config agents should parse"),
            }),
            None,
            Arc::new(MemoryStore::new()),
        );

        assert!(os.agent("researcher").is_none());
    }
}
