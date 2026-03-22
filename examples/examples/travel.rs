//! Trip planning agent demonstrating custom tools, state management,
//! observability, and the AgentRuntimeBuilder API.

use std::sync::Arc;

use awaken_contract::contract::tool::Tool;
use awaken_contract::registry_spec::AgentSpec;
use awaken_examples::travel::tools::*;
use awaken_ext_observability::{
    AgentMetrics, GenAISpan, MetricsSink, ObservabilityPlugin, ToolSpan,
};
use awaken_ext_permission::PermissionPlugin;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::plugins::Plugin;
use awaken_stores::InMemoryStore;

/// Logging sink that forwards metrics via tracing after each session.
struct LoggingSink;

impl MetricsSink for LoggingSink {
    fn on_inference(&self, span: &GenAISpan) {
        tracing::info!(
            model = %span.model,
            provider = %span.provider,
            input_tokens = ?span.input_tokens,
            output_tokens = ?span.output_tokens,
            duration_ms = span.duration_ms,
            "inference completed"
        );
    }

    fn on_tool(&self, span: &ToolSpan) {
        tracing::info!(
            name = %span.name,
            error_type = ?span.error_type,
            duration_ms = span.duration_ms,
            "tool executed"
        );
    }

    fn on_run_end(&self, metrics: &AgentMetrics) {
        tracing::info!(
            session_duration_s = format_args!("{:.1}", metrics.session_duration_ms as f64 / 1000.0),
            total_input = metrics.total_input_tokens(),
            total_output = metrics.total_output_tokens(),
            total_tokens = metrics.total_tokens(),
            "session metrics"
        );
        for stat in metrics.stats_by_model() {
            tracing::info!(
                model = %stat.model,
                provider = %stat.provider,
                inferences = stat.inference_count,
                input = stat.input_tokens,
                output = stat.output_tokens,
                "model stats"
            );
        }
        for stat in metrics.stats_by_tool() {
            tracing::info!(
                tool = %stat.name,
                calls = stat.call_count,
                failures = stat.failure_count,
                "tool stats"
            );
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(true).init();

    let agent_spec = AgentSpec {
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
        max_rounds: 10,
        plugin_ids: vec!["permission".into(), "observability".into()],
        ..Default::default()
    };

    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(AddTripTool),
        Arc::new(UpdateTripTool),
        Arc::new(DeleteTripTool),
        Arc::new(SelectTripTool),
        Arc::new(SearchPlacesTool),
    ];

    let observability = ObservabilityPlugin::new(LoggingSink)
        .with_model("default")
        .with_provider("default");

    let _store = Arc::new(InMemoryStore::new());

    let mut builder = AgentRuntimeBuilder::new().with_agent_spec(agent_spec);

    for tool in &tools {
        let id = tool.descriptor().id.clone();
        builder = builder.with_tool(id, Arc::clone(tool));
    }

    builder = builder.with_plugin("permission", Arc::new(PermissionPlugin) as Arc<dyn Plugin>);
    builder = builder.with_plugin("observability", Arc::new(observability) as Arc<dyn Plugin>);

    let runtime = builder.build().expect("failed to build runtime");

    tracing::info!(
        tool_count = tools.len(),
        agent_id = "travel",
        "runtime ready"
    );

    // The runtime is ready. In production you would wire it into an HTTP server.
    // For now we just verify it resolves the agent.
    let resolved = runtime.resolver().resolve("travel");
    match resolved {
        Ok(agent) => {
            tracing::info!(
                agent_id = %agent.config.id,
                tool_count = agent.config.tools.len(),
                "agent resolved"
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to resolve agent");
        }
    }
}
