pub mod frontend_tools;
pub mod generative_ui_tools;
pub mod phase_logger;
pub mod research;
pub mod scripted_executor;
pub mod state;
pub mod tools;
pub mod travel;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tower_http::cors::{Any, CorsLayer};

use awaken_contract::MailboxStore;
use awaken_contract::ProfileStore;
use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::inference::ContextWindowPolicy;
use awaken_contract::contract::storage::ThreadRunStore;
use awaken_contract::contract::tool::Tool;
use awaken_contract::registry_spec::AgentSpec;
use awaken_ext_generative_ui::{A2uiPlugin, json_render, openui};
use awaken_ext_mcp::{McpPlugin, McpServerConnectionConfig, McpToolRegistryManager};
use awaken_ext_observability::{InMemorySink, ObservabilityPlugin};
use awaken_ext_permission::{
    PermissionConfigKey, PermissionPlugin, PermissionRuleEntry, PermissionRulesConfig,
    ToolPermissionBehavior,
};
use awaken_ext_reminder::ReminderPlugin;
use awaken_ext_skills::{
    CompositeSkillRegistry, EmbeddedSkill, EmbeddedSkillData, FsSkillRegistryManager,
    InMemorySkillRegistry, SkillDiscoveryPlugin, SkillRegistry,
};
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::engine::{GenaiExecutor, LlmRetryPolicy, RetryingExecutor};
use awaken_runtime::plugins::Plugin;
use awaken_runtime::policies::{
    ConsecutiveErrorsPolicy, StopConditionPlugin, StopPolicy, TimeoutPolicy, TokenBudgetPolicy,
};
use awaken_runtime::registry::traits::ModelEntry;
use awaken_server::app::{AppState, ServerConfig};
use awaken_server::mailbox::{Mailbox, MailboxConfig};
use awaken_server::routes::build_router;
use awaken_stores::FileStore;

use crate::starter_backend::frontend_tools::FrontendToolPlugin;
use crate::starter_backend::generative_ui_tools::{SharedAgentResolver, StreamingGenerativeUiTool};
use crate::starter_backend::phase_logger::PhaseLoggerPlugin;
use crate::starter_backend::research::{
    DeleteResourcesTool, ExtractResourcesTool, SearchTool, SetQuestionTool, WriteReportTool,
};
use crate::starter_backend::tools::{
    AppendNoteTool, AskUserQuestionTool, FailingTool, FinishTool, GetStockPriceTool,
    GetWeatherTool, ProgressDemoTool, ServerInfoTool, SetBackgroundColorTool,
};
use crate::starter_backend::travel::{
    AddTripTool, DeleteTripTool, SearchPlacesTool, SelectTripTool, UpdateTripTool,
};

#[derive(Debug, Clone, Parser)]
pub struct StarterBackendArgs {
    #[arg(long, env = "AWAKEN_HTTP_ADDR", default_value = "127.0.0.1:38080")]
    pub http_addr: String,

    #[arg(long, env = "AWAKEN_STORAGE_DIR", default_value = "./sessions")]
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
        default_value = "You are the awaken starter assistant. Use tools proactively when users ask for weather, stock quotes, or note updates."
    )]
    pub system_prompt: String,

    #[arg(long, env = "MCP_SERVER_CMD")]
    pub mcp_server_cmd: Option<String>,

    #[arg(long, env = "A2A_REMOTE_URL")]
    pub a2a_remote_url: Option<String>,

    #[arg(long, env = "A2A_BEARER_TOKEN")]
    pub a2a_bearer_token: Option<String>,
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

/// Build a genai Client, routing through OPENAI_BASE_URL if set.
pub fn build_genai_client() -> genai::Client {
    if let (Ok(mut base_url), Ok(api_key)) = (
        std::env::var("OPENAI_BASE_URL"),
        std::env::var("OPENAI_API_KEY"),
    ) {
        use genai::resolver::{AuthData, Endpoint};
        use genai::{ModelIden, ServiceTarget};

        // genai's Url::join requires trailing slash to avoid path replacement
        if !base_url.ends_with('/') {
            base_url.push('/');
        }

        let adapter_override = std::env::var("OPENAI_ADAPTER").ok();

        genai::Client::builder()
            .with_service_target_resolver_fn(move |st: ServiceTarget| {
                let adapter_kind = resolve_openai_compatible_adapter(
                    &st.model.model_name,
                    adapter_override.as_deref(),
                );
                Ok(ServiceTarget {
                    endpoint: Endpoint::from_owned(base_url.clone()),
                    auth: AuthData::from_single(api_key.clone()),
                    model: ModelIden::new(adapter_kind, st.model.model_name),
                })
            })
            .build()
    } else {
        genai::Client::default()
    }
}

fn resolve_openai_compatible_adapter(
    model_name: &str,
    adapter_override: Option<&str>,
) -> genai::adapter::AdapterKind {
    use genai::adapter::AdapterKind;

    match adapter_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => match value.to_ascii_lowercase().as_str() {
            "openai" => AdapterKind::OpenAI,
            "openai_resp" | "openai-resp" | "responses" => AdapterKind::OpenAIResp,
            "deepseek" => AdapterKind::DeepSeek,
            "together" => AdapterKind::Together,
            "fireworks" => AdapterKind::Fireworks,
            _ => infer_openai_compatible_adapter(model_name),
        },
        None => infer_openai_compatible_adapter(model_name),
    }
}

fn infer_openai_compatible_adapter(model_name: &str) -> genai::adapter::AdapterKind {
    use genai::adapter::AdapterKind;

    let inferred = AdapterKind::from_model(model_name).unwrap_or(AdapterKind::OpenAI);
    match inferred {
        AdapterKind::OpenAI
        | AdapterKind::OpenAIResp
        | AdapterKind::DeepSeek
        | AdapterKind::Together
        | AdapterKind::Fireworks => inferred,
        _ => AdapterKind::OpenAI,
    }
}

#[cfg(test)]
mod build_genai_client_tests {
    use super::{infer_openai_compatible_adapter, resolve_openai_compatible_adapter};
    use genai::adapter::AdapterKind;

    #[test]
    fn infers_openai_responses_for_gpt_five_models() {
        assert_eq!(
            infer_openai_compatible_adapter("gpt-5.4"),
            AdapterKind::OpenAIResp
        );
    }

    #[test]
    fn infers_openai_chat_completions_for_gpt_four_models() {
        assert_eq!(
            infer_openai_compatible_adapter("gpt-4o-mini"),
            AdapterKind::OpenAI
        );
    }

    #[test]
    fn supports_explicit_openai_responses_override() {
        assert_eq!(
            resolve_openai_compatible_adapter("gpt-4o-mini", Some("openai_resp")),
            AdapterKind::OpenAIResp
        );
    }

    #[test]
    fn falls_back_to_inferred_adapter_for_unknown_override() {
        assert_eq!(
            resolve_openai_compatible_adapter("gpt-5.4", Some("unknown")),
            AdapterKind::OpenAIResp
        );
    }
}

/// Convert a vec of (id, tool) pairs into a HashMap.
pub fn tool_map_from_vec(tools: Vec<(&str, Arc<dyn Tool>)>) -> HashMap<String, Arc<dyn Tool>> {
    tools
        .into_iter()
        .map(|(id, tool)| (id.to_string(), tool))
        .collect()
}

pub async fn serve_starter_backend(args: StarterBackendArgs, config: StarterBackendConfig) {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,genai=warn,awaken_runtime=info".parse().unwrap()),
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

    // -- Agent specs --

    let default_agent = AgentSpec {
        id: default_id.clone(),
        model: "default".into(),
        system_prompt: base_prompt.clone(),
        max_rounds: 3,
        plugin_ids: vec!["frontend_tools".into(), "observability".into()],
        ..Default::default()
    };
    let permission_agent = AgentSpec {
        id: "permission".into(),
        model: "default".into(),
        system_prompt: base_prompt.clone(),
        max_rounds: args.max_rounds,
        plugin_ids: vec!["permission".into(), "frontend_tools".into()],
        ..Default::default()
    };
    let permission_agent = permission_agent
        .with_config::<PermissionConfigKey>(PermissionRulesConfig {
            default_behavior: ToolPermissionBehavior::Ask,
            rules: vec![
                // Allow all read-like tools (get_*) automatically
                PermissionRuleEntry {
                    tool: "get_*".into(),
                    behavior: ToolPermissionBehavior::Allow,
                    scope: Default::default(),
                },
                // Allow serverInfo — safe, read-only
                PermissionRuleEntry {
                    tool: "serverInfo".into(),
                    behavior: ToolPermissionBehavior::Allow,
                    scope: Default::default(),
                },
                // Deny the known-broken tool unconditionally
                PermissionRuleEntry {
                    tool: "failingTool".into(),
                    behavior: ToolPermissionBehavior::Deny,
                    scope: Default::default(),
                },
                // Deny any tool that deletes resources
                PermissionRuleEntry {
                    tool: "delete_*".into(),
                    behavior: ToolPermissionBehavior::Deny,
                    scope: Default::default(),
                },
            ],
        })
        .expect("invalid permission config for permission agent");
    let travel_agent = AgentSpec {
        id: "travel".into(),
        model: "default".into(),
        system_prompt: concat!(
            "You are a travel planning assistant. Help users plan trips by adding, ",
            "updating, and searching for places of interest. Use the provided tools ",
            "to manage trips and find destinations.\n\n",
            "When the user asks to plan a trip, create it with add_trips, then ",
            "search_for_places to find interesting locations. Always select the ",
            "active trip with select_trip after creating it."
        )
        .into(),
        max_rounds: 3,
        plugin_ids: vec!["permission".into()],
        ..Default::default()
    };
    let research_agent = AgentSpec {
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
        max_rounds: 3,
        plugin_ids: vec!["permission".into()],
        ..Default::default()
    };
    let has_skills_dir = std::path::Path::new("./skills").is_dir();
    let skills_agent = AgentSpec {
        id: "skills".into(),
        model: "default".into(),
        system_prompt: base_prompt.clone(),
        max_rounds: args.max_rounds,
        plugin_ids: vec!["skills-discovery".into(), "frontend_tools".into()],
        ..Default::default()
    };
    let limited_agent = AgentSpec {
        id: "limited".into(),
        model: "default".into(),
        system_prompt: "You are a test assistant. Respond briefly to every message.".into(),
        max_rounds: 1,
        plugin_ids: vec!["permission".into()],
        ..Default::default()
    };
    let a2a_agent = AgentSpec {
        id: "a2a".into(),
        model: "default".into(),
        system_prompt: base_prompt.clone(),
        max_rounds: args.max_rounds,
        plugin_ids: vec!["frontend_tools".into()],
        ..Default::default()
    };
    let phases_agent = AgentSpec {
        id: "phases".into(),
        model: "default".into(),
        system_prompt: "You are a test assistant demonstrating phase hooks. Respond briefly."
            .into(),
        max_rounds: 2,
        plugin_ids: vec!["permission".into(), "phase_logger".into()],
        ..Default::default()
    };
    let genui_agent = AgentSpec {
        id: "genui".into(),
        model: "default".into(),
        system_prompt: concat!(
            "You are a generative UI assistant that outputs JSON Render documents.\n\n",
            "When users ask for forms or interfaces, respond with ONLY a JSON object in this format:\n",
            "```json\n",
            "{\n",
            "  \"root\": \"elementId\",\n",
            "  \"elements\": {\n",
            "    \"elementId\": {\n",
            "      \"type\": \"ComponentName\",\n",
            "      \"props\": {},\n",
            "      \"children\": [\"childId\"]\n",
            "    }\n",
            "  }\n",
            "}\n",
            "```\n\n",
            "Available component types: Card, Text, Button, Input, Form, Stack, Grid, Image, Badge.\n",
            "Each element has an id, a type, optional props, and optional children (array of element ids).\n\n",
            "Output ONLY valid JSON. No markdown fences, no explanation outside the JSON."
        )
        .into(),
        max_rounds: 2,
        plugin_ids: vec!["permission".into()],
        ..Default::default()
    };

    let a2ui_agent = AgentSpec {
        id: "a2ui".into(),
        model: "default".into(),
        system_prompt: A2UI_AGENT_PROMPT.into(),
        max_rounds: 8,
        plugin_ids: vec!["generative-ui".into()],
        allowed_tools: Some(vec!["render_a2ui".into()]),
        ..Default::default()
    };
    let json_render_agent = AgentSpec {
        id: "json-render".into(),
        model: "default".into(),
        system_prompt: JSON_RENDER_AGENT_PROMPT.into(),
        max_rounds: 4,
        allowed_tools: Some(vec!["render_json_ui".into()]),
        ..Default::default()
    };
    let openui_agent = AgentSpec {
        id: "openui".into(),
        model: "default".into(),
        system_prompt: OPENUI_AGENT_PROMPT.into(),
        max_rounds: 4,
        allowed_tools: Some(vec!["render_openui_ui".into()]),
        ..Default::default()
    };
    let json_render_ui_agent = AgentSpec {
        id: "json-render-ui".into(),
        model: "default".into(),
        system_prompt: json_render::system_prompt(JSON_RENDER_COMPONENT_CATALOG),
        max_rounds: 1,
        // Sub-agents generate pure text (UI markup); no tools needed.
        // Without this, they inherit parent tools and may recursively
        // call render_* tools, causing a stack overflow.
        allowed_tools: Some(vec![]),
        ..Default::default()
    };
    let openui_ui_agent = AgentSpec {
        id: "openui-ui".into(),
        model: "default".into(),
        system_prompt: openui::system_prompt(OPENUI_COMPONENT_CATALOG),
        max_rounds: 1,
        allowed_tools: Some(vec![]),
        ..Default::default()
    };

    // Profile agent: demonstrates cross-run key-value persistence via ProfileStore.
    // The InMemoryStore wired via `with_profile_store` below backs `ProfileAccess`
    // so plugins/tools can read and write scoped profile entries.
    let profile_agent = AgentSpec {
        id: "profile".into(),
        model: "default".into(),
        system_prompt: "You are a stateful assistant that remembers user preferences \
            across runs using profile storage. When the user sets a preference, \
            acknowledge it. When asked to recall, retrieve it from the profile store."
            .into(),
        max_rounds: 3,
        plugin_ids: vec!["permission".into()],
        ..Default::default()
    };

    // Creative agent: demonstrates per-agent context window policy.
    // InferenceOverride (temperature, reasoning_effort, etc.) is a per-run
    // concern set via `RunRequest::with_overrides` — it is not an AgentSpec field.
    // This agent instead shows ContextWindowPolicy configuration.
    let creative_agent = AgentSpec {
        id: "creative".into(),
        model: "default".into(),
        system_prompt: "You are a creative writing assistant. Produce vivid, \
            imaginative prose. Use metaphors and varied sentence structures."
            .into(),
        max_rounds: 3,
        context_policy: Some(ContextWindowPolicy {
            max_context_tokens: 128_000,
            max_output_tokens: 4_096,
            min_recent_messages: 6,
            enable_prompt_cache: false,
            ..Default::default()
        }),
        plugin_ids: vec!["permission".into()],
        ..Default::default()
    };

    // Compact agent: demonstrates ContextWindowPolicy with auto-compaction enabled.
    let compact_agent = AgentSpec {
        id: "compact".into(),
        model: "default".into(),
        system_prompt: "You are a context-aware assistant. Your context window is managed \
            with auto-compaction so long conversations stay within token limits."
            .into(),
        max_rounds: 3,
        context_policy: Some(ContextWindowPolicy {
            max_context_tokens: 4096,
            max_output_tokens: 1024,
            min_recent_messages: 4,
            enable_prompt_cache: false,
            autocompact_threshold: Some(3072),
            ..Default::default()
        }),
        plugin_ids: vec!["permission".into()],
        ..Default::default()
    };

    // Budget agent: demonstrates stop policies (token budget, timeout, consecutive errors).
    // Uses the "budget-stop" StopConditionPlugin registered below.
    let budget_agent = AgentSpec {
        id: "budget".into(),
        model: "default".into(),
        system_prompt: "You are a budget-constrained assistant with token limits and timeout. \
            The run will stop if you exceed 50k tokens, 60 seconds, or 3 consecutive errors."
            .into(),
        max_rounds: 3,
        plugin_ids: vec!["permission".into(), "budget-stop".into()],
        ..Default::default()
    };

    // -- Tools --

    let generative_ui_resolver = SharedAgentResolver::default();
    let tools: Vec<(&str, Arc<dyn Tool>)> = vec![
        (
            "render_json_ui",
            Arc::new(StreamingGenerativeUiTool::json_render(
                generative_ui_resolver.clone(),
            )),
        ),
        (
            "render_openui_ui",
            Arc::new(StreamingGenerativeUiTool::openui(
                generative_ui_resolver.clone(),
            )),
        ),
        ("get_weather", Arc::new(GetWeatherTool)),
        ("get_stock_price", Arc::new(GetStockPriceTool)),
        ("append_note", Arc::new(AppendNoteTool)),
        (
            "serverInfo",
            Arc::new(ServerInfoTool::new(&config.service_name)),
        ),
        ("failingTool", Arc::new(FailingTool)),
        ("finish", Arc::new(FinishTool)),
        ("progress_demo", Arc::new(ProgressDemoTool)),
        ("askUserQuestion", Arc::new(AskUserQuestionTool)),
        ("set_background_color", Arc::new(SetBackgroundColorTool)),
        ("add_trips", Arc::new(AddTripTool)),
        ("update_trips", Arc::new(UpdateTripTool)),
        ("delete_trips", Arc::new(DeleteTripTool)),
        ("select_trip", Arc::new(SelectTripTool)),
        ("search_for_places", Arc::new(SearchPlacesTool)),
        ("search", Arc::new(SearchTool)),
        ("write_report", Arc::new(WriteReportTool)),
        ("set_research_question", Arc::new(SetQuestionTool)),
        ("delete_resources", Arc::new(DeleteResourcesTool)),
        ("extract_resources", Arc::new(ExtractResourcesTool)),
    ];

    // -- Build genai client and provider --
    // Use scripted executor for deterministic testing when no LLM key is set.

    let base_executor: Arc<dyn LlmExecutor> = if std::env::var("OPENAI_BASE_URL").is_ok()
        || std::env::var("OPENAI_API_KEY").is_ok()
    {
        let client = build_genai_client();
        Arc::new(GenaiExecutor::with_client(client))
    } else {
        tracing::info!("No LLM API key found, using scripted executor for deterministic testing");
        Arc::new(scripted_executor::ScriptedLlmExecutor::new())
    };

    let executor: Arc<dyn LlmExecutor> = Arc::new(RetryingExecutor::new(
        base_executor,
        LlmRetryPolicy::default(),
    ));

    // -- MCP --

    let mcp_manager = if let Some(ref cmd_str) = args.mcp_server_cmd {
        let parts: Vec<&str> = cmd_str.split_whitespace().collect();
        if let Some((command, cmd_args)) = parts.split_first() {
            let cfg = McpServerConnectionConfig::stdio(
                "mcp_demo",
                *command,
                cmd_args.iter().map(|s| s.to_string()).collect(),
            );
            match McpToolRegistryManager::connect([cfg]).await {
                Ok(manager) => {
                    let registry = manager.registry();
                    tracing::info!(tools = registry.snapshot().len(), "MCP connected");
                    Some(manager)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "MCP failed to connect");
                    None
                }
            }
        } else {
            tracing::warn!("MCP_SERVER_CMD is empty");
            None
        }
    } else {
        None
    };

    // -- File store --

    let file_store = Arc::new(FileStore::new(&args.storage_dir));

    // -- Builder --

    let mut builder = AgentRuntimeBuilder::new()
        .with_agent_spec(default_agent)
        .with_provider("default", executor)
        .with_model(
            "default",
            ModelEntry {
                provider: "default".into(),
                model_name: args.model.clone(),
            },
        )
        .with_thread_run_store(file_store.clone() as Arc<dyn ThreadRunStore>)
        .with_profile_store(
            Arc::new(awaken_stores::InMemoryStore::default()) as Arc<dyn ProfileStore>
        );

    for (id, tool) in &tools {
        builder = builder.with_tool(*id, Arc::clone(tool));
    }

    // Register MCP tools via plugin lifecycle
    if let Some(ref manager) = mcp_manager {
        builder = builder.with_plugin(
            "mcp",
            Arc::new(McpPlugin::new(manager.registry())) as Arc<dyn Plugin>,
        );
    }

    // -- Agent specs (skip if already the default) --

    if default_id != "permission" {
        builder = builder.with_agent_spec(permission_agent);
    }
    if default_id != "travel" {
        builder = builder.with_agent_spec(travel_agent);
    }
    if default_id != "research" {
        builder = builder.with_agent_spec(research_agent);
    }
    if default_id != "skills" {
        builder = builder.with_agent_spec(skills_agent);
    }
    if default_id != "limited" {
        builder = builder.with_agent_spec(limited_agent);
    }
    if default_id != "a2a" {
        builder = builder.with_agent_spec(a2a_agent);
    }
    if default_id != "phases" {
        builder = builder.with_agent_spec(phases_agent);
    }
    if default_id != "genui" {
        builder = builder.with_agent_spec(genui_agent);
    }
    if default_id != "a2ui" {
        builder = builder.with_agent_spec(a2ui_agent);
    }
    if default_id != "json-render" {
        builder = builder.with_agent_spec(json_render_agent);
    }
    if default_id != "openui" {
        builder = builder.with_agent_spec(openui_agent);
    }
    if default_id != "json-render-ui" {
        builder = builder.with_agent_spec(json_render_ui_agent);
    }
    if default_id != "openui-ui" {
        builder = builder.with_agent_spec(openui_ui_agent);
    }
    if default_id != "profile" {
        builder = builder.with_agent_spec(profile_agent);
    }
    if default_id != "creative" {
        builder = builder.with_agent_spec(creative_agent);
    }
    if default_id != "compact" {
        builder = builder.with_agent_spec(compact_agent);
    }
    if default_id != "budget" {
        builder = builder.with_agent_spec(budget_agent);
    }
    // -- A2A remote agents --

    if let Some(ref remote_url) = args.a2a_remote_url {
        builder =
            builder.with_remote_agents("remote", remote_url.clone(), args.a2a_bearer_token.clone());
    }

    // -- Plugins --

    builder = builder.with_plugin("permission", Arc::new(PermissionPlugin) as Arc<dyn Plugin>);
    builder = builder.with_plugin(
        "generative-ui",
        Arc::new(A2uiPlugin::with_catalog_id(
            "https://a2ui.org/specification/v0_8/standard_catalog_definition.json",
        )) as Arc<dyn Plugin>,
    );
    builder = builder.with_plugin(
        "frontend_tools",
        Arc::new(FrontendToolPlugin::new()) as Arc<dyn Plugin>,
    );

    // Reminder plugin demonstrating all OutputMatcher variants
    let reminder_rules = awaken_ext_reminder::ReminderRulesConfig {
        rules: vec![
            // Rule 1: OutputMatcher::Any — triggers on every get_weather call
            awaken_ext_reminder::ReminderRuleEntry {
                name: Some("weather-travel-hint".into()),
                tool: "get_weather".into(),
                output: awaken_ext_reminder::config::OutputEntry::Simple("any".into()),
                message: awaken_ext_reminder::config::MessageEntry {
                    target: "system".into(),
                    content: "Tip: suggest the travel agent if the user mentions a trip.".into(),
                    cooldown_turns: 5,
                },
            },
            // Rule 2: OutputMatcher::Status — triggers only on successful stock lookups
            awaken_ext_reminder::ReminderRuleEntry {
                name: Some("stock-success-tip".into()),
                tool: "get_stock_price".into(),
                output: awaken_ext_reminder::config::OutputEntry::Structured {
                    status: Some("success".into()),
                    content: None,
                },
                message: awaken_ext_reminder::config::MessageEntry {
                    target: "suffix_system".into(),
                    content:
                        "Tip: stock data is simulated. For real data, configure a market data API."
                            .into(),
                    cooldown_turns: 3,
                },
            },
            // Rule 3: OutputMatcher::Content (text glob) — triggers when weather contains Sunny
            awaken_ext_reminder::ReminderRuleEntry {
                name: Some("heat-warning".into()),
                tool: "get_weather".into(),
                output: awaken_ext_reminder::config::OutputEntry::Structured {
                    status: None,
                    content: Some(awaken_ext_reminder::config::ContentEntry::TextGlob(
                        "*Sunny*".into(),
                    )),
                },
                message: awaken_ext_reminder::config::MessageEntry {
                    target: "suffix_system".into(),
                    content:
                        "Warning: high temperatures detected. Suggest sunscreen and hydration."
                            .into(),
                    cooldown_turns: 3,
                },
            },
            // Rule 4: OutputMatcher::Both (status + JSON fields) — successful note append
            awaken_ext_reminder::ReminderRuleEntry {
                name: Some("append-note-confirm".into()),
                tool: "append_note".into(),
                output: awaken_ext_reminder::config::OutputEntry::Structured {
                    status: Some("success".into()),
                    content: Some(awaken_ext_reminder::config::ContentEntry::Fields {
                        fields: vec![awaken_ext_reminder::config::FieldEntry {
                            path: "status".into(),
                            op: "exact".into(),
                            value: "ok".into(),
                        }],
                    }),
                },
                message: awaken_ext_reminder::config::MessageEntry {
                    target: "system".into(),
                    content: "Note saved. Remind the user they can review notes anytime.".into(),
                    cooldown_turns: 2,
                },
            },
        ],
    };
    if let Ok(rules) = reminder_rules.into_rules() {
        builder = builder.with_plugin(
            "reminder",
            Arc::new(ReminderPlugin::new(rules)) as Arc<dyn Plugin>,
        );
    }

    // Phase logger plugin (demonstrates all 8 phase hooks)
    builder = builder.with_plugin(
        "phase_logger",
        Arc::new(PhaseLoggerPlugin) as Arc<dyn Plugin>,
    );

    // Budget stop-condition plugin (token budget + timeout + consecutive errors)
    let budget_policies: Vec<Arc<dyn StopPolicy>> = vec![
        Arc::new(TokenBudgetPolicy::new(50_000)),
        Arc::new(TimeoutPolicy::new(60_000)),
        Arc::new(ConsecutiveErrorsPolicy::new(3)),
    ];
    builder = builder.with_plugin(
        "budget-stop",
        Arc::new(StopConditionPlugin::new(budget_policies)) as Arc<dyn Plugin>,
    );

    // Observability plugin (in-memory sink for demo)
    let observability = ObservabilityPlugin::new(InMemorySink::new())
        .with_model(&args.model)
        .with_provider("default");
    builder = builder.with_plugin("observability", Arc::new(observability) as Arc<dyn Plugin>);

    // Skills: embedded skill + optional filesystem discovery via CompositeSkillRegistry
    {
        static GREETING_SKILL_MD: &str = "---
name: greeting
description: Adds friendly greeting behavior
---
Always greet the user warmly and ask how you can help today.
";
        let embedded_data = EmbeddedSkillData {
            skill_md: GREETING_SKILL_MD,
            references: &[],
            assets: &[],
        };
        let embedded_skill =
            EmbeddedSkill::new(&embedded_data).expect("invalid embedded greeting skill");
        let embedded_registry: Arc<dyn SkillRegistry> = Arc::new(
            InMemorySkillRegistry::from_skills(vec![Arc::new(embedded_skill)]),
        );

        let registry: Arc<dyn SkillRegistry> = if has_skills_dir {
            match FsSkillRegistryManager::discover_roots(vec![PathBuf::from("./skills")]) {
                Ok(fs_manager) => {
                    let fs_registry: Arc<dyn SkillRegistry> = Arc::new(fs_manager);
                    match CompositeSkillRegistry::try_new([embedded_registry, fs_registry]) {
                        Ok(composite) => Arc::new(composite),
                        Err(e) => {
                            tracing::warn!(error = %e, "Skills: composite merge conflict, using embedded only");
                            Arc::new(InMemorySkillRegistry::from_skills(vec![]))
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Skills: failed to discover from ./skills/");
                    embedded_registry
                }
            }
        } else {
            embedded_registry
        };

        builder = builder.with_plugin(
            "skills-discovery",
            Arc::new(SkillDiscoveryPlugin::new(registry)) as Arc<dyn Plugin>,
        );
    }

    // -- Build runtime --

    let runtime = builder.build().expect("failed to build runtime");
    let runtime = Arc::new(runtime);
    let resolver = runtime.resolver_arc();
    generative_ui_resolver.set(resolver.clone());

    // -- Mailbox --

    let mailbox_store = Arc::new(awaken_stores::InMemoryMailboxStore::new());
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        mailbox_store as Arc<dyn MailboxStore>,
        format!("starter-backend:{}", std::process::id()),
        MailboxConfig::default(),
    ));

    // -- Server --

    let state = AppState::new(
        runtime,
        mailbox,
        file_store as Arc<dyn ThreadRunStore>,
        resolver,
        ServerConfig {
            address: args.http_addr.clone(),
            ..Default::default()
        },
    );

    let mut app = build_router().with_state(state);

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
    tracing::info!(
        agent = %default_id,
        addr = %listener.local_addr().unwrap(),
        "starter backend listening"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server crashed");
}

// ---------------------------------------------------------------------------
// A2UI agent system prompt
// ---------------------------------------------------------------------------

const A2UI_AGENT_PROMPT: &str = r#"You are a helpful assistant that creates interactive UIs using the render_a2ui tool.

When the user describes a form, dashboard, or any structured interface, render it as A2UI.
Use the official A2UI v0.8 message format.
Call render_a2ui once per message in this order: surfaceUpdate → dataModelUpdate → beginRendering.
Pass exactly one top-level A2UI message key per tool call. Do not wrap the payload in a messages array unless the user explicitly asks for a batch payload.

Design guidelines:
- Use Card as the root container and Column/Row for layout.
- Use the v0.8 nested component format, e.g. {"component":{"Text":{...}}}.
- Use TextField for text input, DateTimeInput for dates, Slider for numeric ranges, and MultipleChoice for selection lists.
- In v0.8, TextField binds with `text`, not `value`.
- In v0.8, MultipleChoice uses `options`, `selections`, and optional `maxAllowedSelections`.
- In v0.8, MultipleChoice does not have a `label` field. If you need visible field text, render a nearby Text component.
- In v0.8, MultipleChoice `selections.path` points to an array value. Initialize defaults with `valueMap`, for example {"key":"priority","valueMap":[{"key":"0","valueString":"standard"}]}.
- Text components must always use {"Text":{"text":{"literalString":"..."}}}; never flatten copy directly under `Text`.
- For DateTimeInput defaults, use local datetime strings compatible with `datetime-local`, for example `2026-04-10T09:00`. Do not include a trailing `Z` or timezone offset.
- In v0.8, Button actions use {"name":"...","context":[{"key":"field","value":{"path":"/request/field"}}]}, not the v0.9 event wrapper.
- Initialize the data model with sensible defaults using dataModelUpdate.contents entries.
- Call beginRendering only after the referenced root component exists.

Keep surfaceId short and descriptive (e.g. "booking", "contact", "settings").

Business-fit guidelines:
- Prefer practical business workflows such as booking, approvals, intake forms, settings, and operations dashboards.
- Labels and helper text should read like production UI copy, not demos or placeholders.
- Default values should be realistic and safe to submit.

Few-shot reminders:
- Valid surfaceUpdate shape:
  {"surfaceUpdate":{"surfaceId":"surface","components":[
    {"id":"root","component":{"Card":{"child":"content"}}},
    {"id":"content","component":{"Column":{"children":{"explicitList":["title","field","priority_label","priority","submit_label","submit_button"]}}}},
    {"id":"title","component":{"Text":{"usageHint":"h2","text":{"literalString":"Title"}}}},
    {"id":"field","component":{"TextField":{"label":{"literalString":"Field label"},"text":{"path":"/request/field"},"textFieldType":"shortText"}}},
    {"id":"priority_label","component":{"Text":{"usageHint":"caption","text":{"literalString":"Priority"}}}},
    {"id":"priority","component":{"MultipleChoice":{"selections":{"path":"/request/priority"},"options":[{"label":{"literalString":"Standard"},"value":"standard"},{"label":{"literalString":"Urgent"},"value":"urgent"}],"maxAllowedSelections":1}}},
    {"id":"submit_label","component":{"Text":{"text":{"literalString":"Submit"}}}},
    {"id":"submit_button","component":{"Button":{"child":"submit_label","primary":true,"action":{"name":"surface.submit","context":[{"key":"field","value":{"path":"/request/field"}},{"key":"priority","value":{"path":"/request/priority"}}]}}}}
  ]}}
- Valid dataModelUpdate shape:
  {"dataModelUpdate":{"surfaceId":"surface","path":"/request","contents":[
    {"key":"field","valueString":""},
    {"key":"priority","valueMap":[{"key":"0","valueString":"standard"}]}
  ]}}
- Valid beginRendering shape:
  {"beginRendering":{"surfaceId":"surface","root":"root"}}
- Never send a component without both `id` and `component`.
- Never flatten the component type into a string; use the official nested object shape.
- If validation fails once, correct the exact invalid field path and keep the rest of the structure unchanged.
- If the user later sends a message starting with `A2UI action:` followed by JSON, treat it as a user interaction event and update the existing surface accordingly.
- When an update keeps existing bound fields visible, resend their current values in the next `dataModelUpdate` together with any new status fields. Do not rely on earlier turns to repopulate those bindings after refresh.
- Use the `context` inside `A2UI action:` as the source of truth for the current field values when it is available.
"#;

const JSON_RENDER_AGENT_PROMPT: &str = r#"You are a product UI assistant that creates business-ready interfaces with the render_json_ui tool.

When the user asks for a form, dashboard, settings page, intake flow, workspace, or structured operational interface, call render_json_ui.

Tool usage rules:
- Pass a concise but complete UI brief in the tool `prompt`.
- Preserve the user's business context, domain terms, and requested fields.
- Prefer realistic product copy, realistic default values, and practical workflows.
- Do not output raw JSON yourself when the tool is available.
- After the tool finishes, briefly summarize what was created and how the user would use it.

Quality rules:
- Use production-style labels and helper text, not toy examples.
- Keep layouts focused and readable.
- If the user asks for data-heavy views, prefer summary cards, tables, filters, and clear status labels.
"#;

const OPENUI_AGENT_PROMPT: &str = r#"You are a product UI assistant that creates business-ready interfaces with the render_openui_ui tool.

When the user asks for a form, dashboard, workflow panel, review screen, or structured operational interface, call render_openui_ui.

Tool usage rules:
- Pass a concise but complete UI brief in the tool `prompt`.
- Preserve the user's business context, domain terms, and requested fields.
- Prefer realistic product copy, realistic default values, and practical workflows.
- Do not output OpenUI Lang yourself when the tool is available.
- After the tool finishes, briefly summarize what was created and how the user would use it.

Quality rules:
- Use production-style labels and helper text, not toy examples.
- Keep layouts focused and readable.
- Prefer components common in SaaS and internal tools: cards, tables, forms, status tags, and callouts.
"#;

const JSON_RENDER_COMPONENT_CATALOG: &str = r#"{
  "Card": "Container for a logical section or summary block",
  "Stack": "Vertical or horizontal layout container",
  "Grid": "Multi-column layout for compact overviews",
  "Text": "Short text content such as headings, labels, or helper text",
  "Input": "Single-line text input with props like label and placeholder",
  "TextArea": "Multi-line text input for explanations or notes",
  "Select": "Single-select input with props like label and options",
  "Button": "Primary or secondary action button with props like label",
  "Badge": "Short status label such as Approved, Pending, or Blocked",
  "Table": "Tabular records with columns and rows",
  "Charts": "Chart block for business metrics or trends"
}

Critical rules:
- Output one valid JSON object with `root` and `elements`.
- Every element in `elements` must have `type`, `props`, and `children`.
- Every key referenced in a `children` array must exist in `elements`.
- Keep `visible`, `repeat`, `watch`, and `on` as top-level element fields, never inside `props`.
- Do not wrap the JSON in markdown."#;

const OPENUI_COMPONENT_CATALOG: &str = r#"Available components:
- Stack(children, direction?, gap?, align?, justify?, wrap?)
- Card(children, variant?, direction?, gap?, align?, justify?, wrap?)
- CardHeader(title, subtitle?)
- TextContent(text, size?)
- Callout(variant, title, description)
- Table(columns, rows)
- Col(label, type?)
- Tag(text, variant?)
- Buttons(buttons, direction?)
- Button(label, action?, variant?, type?, size?)
- Form(name, buttons, fields)
- FormControl(label, input, hint?)
- Input(name, placeholder?, type?, rules?)
- TextArea(name, placeholder?, rows?, rules?)
- Select(name, items, placeholder?, rules?)
- SelectItem(value, label)

Syntax rules:
- Use the OpenUI Lang statement form `identifier = ComponentName(...)`.
- Always write `root = Stack([...])` or `root = Card([...])` first.
- Use references for child nodes inside arrays, for example `root = Stack([title, form])`.
- Prefer the real library signatures shown above. Example: `status = Tag("Pending finance review", "warning")`.
- For action bars, prefer `Buttons([primaryAction, secondaryAction], "row")`.
- For forms, define one `FormControl` reference per field and wrap them with `Form(name, buttons, fields)`.
- Do not invent ad-hoc props that are not in the component signatures.
- Keep the output as raw OpenUI Lang only, with no markdown fences or prose.

Few-shot shape reminder:
root = Stack([title, status, summary, actions])
title = TextContent("Quarterly budget request", "large-heavy")
status = Tag("Pending finance review", "warning")
summary = Callout("info", "Ready for review", "Finance can verify the request before approval.")
actions = Buttons([primary], "row")
primary = Button("Submit request", { type: "continue_conversation" }, "primary")"#;
