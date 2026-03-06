# Enable LLMMetry Observability

Use this when you need per-run inference/tool metrics and OpenTelemetry GenAI-aligned spans.

## Prerequisites

- `tirea-extension-observability` dependency is enabled.
- Optional: tracing/OTel exporter configured in your runtime.

## Steps

1. Implement a metrics sink.

```rust,ignore
use tirea_extension_observability::{AgentMetrics, GenAISpan, MetricsSink, ToolSpan};

struct LoggingSink;

impl MetricsSink for LoggingSink {
    fn on_inference(&self, span: &GenAISpan) {
        eprintln!("inference model={} input={:?} output={:?}", span.model, span.input_tokens, span.output_tokens);
    }

    fn on_tool(&self, span: &ToolSpan) {
        eprintln!("tool={} error={:?}", span.name, span.error_type);
    }

    fn on_run_end(&self, metrics: &AgentMetrics) {
        eprintln!("run tokens={}", metrics.total_tokens());
    }
}
```

2. Register `LLMMetryPlugin` and attach to agent.

```rust,ignore
use std::sync::Arc;
use genai::chat::ChatOptions;
use tirea_extension_observability::LLMMetryPlugin;

let chat_options = ChatOptions::default().with_temperature(0.7);
let llmmetry = LLMMetryPlugin::new(LoggingSink)
    .with_model("deepseek-chat")
    .with_provider("deepseek")
    .with_chat_options(&chat_options);

let os = AgentOsBuilder::new()
    .with_registered_behavior("llmmetry", Arc::new(llmmetry))
    .with_agent(
        "assistant",
        AgentDefinition::new("deepseek-chat").with_behavior_id("llmmetry"),
    )
    .build()?;
```

## Verify

- Inference spans include token counts and duration.
- Tool spans include call id, duration, and error type on failures.
- Run end callback receives aggregated `AgentMetrics`.

## Common Errors

- Registering plugin but not adding behavior id to agent.
- Setting wrong model/provider labels, causing misleading metrics dimensions.

## Related

- [Add a Plugin](./add-a-plugin.md)
- `examples/src/travel.rs`
