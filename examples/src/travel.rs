use clap::Parser;
use genai::chat::ChatOptions;
use std::sync::Arc;
use tirea_agentos::contracts::runtime::behavior::AgentBehavior;
use tirea_agentos::contracts::runtime::tool_call::Tool;
use tirea_agentos::extensions::observability::{
    AgentMetrics, GenAISpan, LLMMetryPlugin, MetricsSink, ToolSpan,
};
use tirea_agentos::extensions::permission::PermissionPlugin;
use tirea_agentos::orchestrator::AgentDefinition;
use tirea_examples::travel::tools::*;

/// Logging sink that prints metrics to stderr after each session.
struct LoggingSink;

impl MetricsSink for LoggingSink {
    fn on_inference(&self, span: &GenAISpan) {
        eprintln!(
            "[llmmetry] inference: model={} provider={} input={:?} output={:?} cache_read={:?} cache_create={:?} {}ms",
            span.model, span.provider,
            span.input_tokens, span.output_tokens,
            span.cache_read_input_tokens, span.cache_creation_input_tokens,
            span.duration_ms,
        );
    }

    fn on_tool(&self, span: &ToolSpan) {
        eprintln!(
            "[llmmetry] tool: name={} error={:?} {}ms",
            span.name, span.error_type, span.duration_ms,
        );
    }

    fn on_run_end(&self, metrics: &AgentMetrics) {
        eprintln!(
            "\n=== Session Metrics ({:.1}s) ===",
            metrics.session_duration_ms as f64 / 1000.0
        );
        for stat in metrics.stats_by_model() {
            eprintln!(
                "  [{}@{}] {} inferences | input: {} output: {} cache_read: {} cache_create: {} | {:.1}s",
                stat.model, stat.provider, stat.inference_count,
                stat.input_tokens, stat.output_tokens,
                stat.cache_read_input_tokens, stat.cache_creation_input_tokens,
                stat.total_duration_ms as f64 / 1000.0,
            );
        }
        for stat in metrics.stats_by_tool() {
            eprintln!(
                "  [tool:{}] {} calls ({} failures) | {:.1}s",
                stat.name,
                stat.call_count,
                stat.failure_count,
                stat.total_duration_ms as f64 / 1000.0,
            );
        }
        eprintln!(
            "  totals: {} input + {} output = {} tokens | inference {:.1}s tool {:.1}s",
            metrics.total_input_tokens(),
            metrics.total_output_tokens(),
            metrics.total_tokens(),
            metrics.total_inference_duration_ms() as f64 / 1000.0,
            metrics.total_tool_duration_ms() as f64 / 1000.0,
        );
        eprintln!("===\n");
    }
}

#[tokio::main]
async fn main() {
    let args = tirea_examples::Args::parse();

    let agent_def = AgentDefinition {
        id: "travel".into(),
        model: "deepseek-chat".into(),
        system_prompt: concat!(
            "You are a travel planning assistant. Help users plan trips by adding, ",
            "updating, and searching for places of interest. Use the provided tools ",
            "to manage trips and find destinations.\n\n",
            "When the user asks to plan a trip, create it with add_trips, then ",
            "search_for_places to find interesting locations. Always select the ",
            "active trip with select_trip after creating it."
        )
        .into(),
        max_rounds: 10,
        behavior_ids: vec!["permission".into(), "llmmetry".into()],
        ..Default::default()
    };

    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(AddTripTool),
        Arc::new(UpdateTripTool),
        Arc::new(DeleteTripTool),
        Arc::new(SelectTripTool),
        Arc::new(SearchPlacesTool),
    ];

    let chat_options = ChatOptions::default()
        .with_temperature(0.7)
        .with_max_tokens(2048);

    let llmmetry = LLMMetryPlugin::new(LoggingSink)
        .with_model("deepseek-chat")
        .with_provider("deepseek")
        .with_chat_options(&chat_options);

    let plugins: Vec<(String, Arc<dyn AgentBehavior>)> = vec![
        ("permission".into(), Arc::new(PermissionPlugin)),
        ("llmmetry".into(), Arc::new(llmmetry)),
    ];

    tirea_examples::serve(args, agent_def, tools, plugins).await;
}
