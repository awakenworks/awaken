# Enable Observability

Use this when you need to trace LLM inference calls and tool executions with OpenTelemetry-compatible telemetry.

## Prerequisites

- A working awaken agent runtime (see [First Agent](../tutorials/first-agent.md))
- Feature `observability` enabled on the `awaken` crate (enabled by default)
- For OTel export: feature `otel` enabled on `awaken-ext-observability`, plus a configured OTel collector

```toml
[dependencies]
awaken = { version = "0.1", features = ["observability"] }
tokio = { version = "1", features = ["full"] }
```

## Steps

1. Register with the in-memory sink (development).

```rust,ignore
use std::sync::Arc;
use awaken::{AgentRuntimeBuilder, Plugin};
use awaken::ext_observability::{ObservabilityPlugin, InMemorySink};

let sink = InMemorySink::new();
let obs_plugin = ObservabilityPlugin::new(sink.clone());

let runtime = AgentRuntimeBuilder::new()
    .with_agent_spec(agent_spec)
    .with_plugin("observability", Arc::new(obs_plugin) as Arc<dyn Plugin>)
    .build()
    .expect("failed to build runtime");
```

After a run completes, inspect collected metrics:

```rust,ignore
let metrics = sink.metrics();
println!("inferences: {}", metrics.inferences.len());
println!("tool calls: {}", metrics.tools.len());
println!("total input tokens: {}", metrics.total_input_tokens());
println!("total output tokens: {}", metrics.total_output_tokens());
println!("tool failures: {}", metrics.tool_failures());
println!("session duration: {}ms", metrics.session_duration_ms);

for stat in metrics.stats_by_model() {
    println!("{}: {} calls, {} in / {} out tokens",
        stat.model, stat.inference_count, stat.input_tokens, stat.output_tokens);
}

for stat in metrics.stats_by_tool() {
    println!("{}: {} calls, {} failures",
        stat.name, stat.call_count, stat.failure_count);
}
```

2. Register with the OTel sink (production).

```rust,ignore
use std::sync::Arc;
use awaken::{AgentRuntimeBuilder, Plugin};
use awaken::ext_observability::{ObservabilityPlugin, OtelMetricsSink};
use opentelemetry_sdk::trace::SdkTracerProvider;

let provider = SdkTracerProvider::builder()
    // configure your exporter (OTLP, Jaeger, etc.)
    .build();
let tracer = provider.tracer("awaken");

let obs_plugin = ObservabilityPlugin::new(OtelMetricsSink::new(tracer));

let runtime = AgentRuntimeBuilder::new()
    .with_agent_spec(agent_spec)
    .with_plugin("observability", Arc::new(obs_plugin) as Arc<dyn Plugin>)
    .build()
    .expect("failed to build runtime");
```

3. Implement a custom sink (optional).

```rust,ignore
use awaken::ext_observability::{MetricsSink, GenAISpan, ToolSpan, AgentMetrics};

struct MySink;

impl MetricsSink for MySink {
    fn on_inference(&self, span: &GenAISpan) {
        // forward to your metrics system
    }

    fn on_tool(&self, span: &ToolSpan) {
        // forward to your metrics system
    }

    fn on_run_end(&self, metrics: &AgentMetrics) {
        // emit summary metrics
    }
}
```

4. Captured telemetry.

   The plugin hooks into the following phases:

| Phase | Data Captured |
|-------|---------------|
| `RunStart` | Session start timestamp |
| `BeforeInference` | Inference start timestamp, model, provider |
| `AfterInference` | Token usage, finish reasons, duration, cache tokens |
| `BeforeToolExecute` | Tool call start timestamp |
| `AfterToolExecute` | Tool duration, error status |
| `RunEnd` | Session duration |

OTel spans follow GenAI semantic conventions with attributes such as `gen_ai.system`, `gen_ai.request.model`, `gen_ai.usage.input_tokens`, and `gen_ai.usage.output_tokens`.

## Verify

1. Run an agent with the `InMemorySink`.
2. After `run()` completes, call `sink.metrics()`.
3. Confirm `inferences` is non-empty and token counts are populated.
4. For OTel, check your collector or Jaeger UI for spans named with the `awaken` tracer.

## Common Errors

| Symptom | Cause | Fix |
|---------|-------|-----|
| Metrics are all zero | Plugin not registered | Register `ObservabilityPlugin` with the runtime builder |
| `OtelMetricsSink` not found | Missing `otel` feature | Enable the `otel` feature on `awaken-ext-observability` |
| No spans in collector | Exporter not configured | Verify `SdkTracerProvider` has an exporter and is not dropped |
| Token counts missing | LLM provider does not report usage | Check that your `LlmExecutor` returns `TokenUsage` in `LLMResponse` |

## Related Example

- `crates/awaken-ext-observability/tests/`

## Key Files

| Path | Purpose |
|------|---------|
| `crates/awaken-ext-observability/src/lib.rs` | Module root and public re-exports |
| `crates/awaken-ext-observability/src/plugin/plugin.rs` | `ObservabilityPlugin` registration |
| `crates/awaken-ext-observability/src/plugin/hooks.rs` | Phase hooks for each telemetry point |
| `crates/awaken-ext-observability/src/metrics.rs` | `AgentMetrics`, `GenAISpan`, `ToolSpan` types |
| `crates/awaken-ext-observability/src/sink.rs` | `MetricsSink` trait and `InMemorySink` |
| `crates/awaken-ext-observability/src/otel.rs` | `OtelMetricsSink` with GenAI semantic conventions |

## Related

- [Add a Plugin](./add-a-plugin.md)
- [Events](../reference/events.md)
- [Architecture](../explanation/architecture.md)
