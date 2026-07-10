//! OpenTelemetry-backed implementation of the runtime's [`MetricsRecorder`] port
//! (#2). The recorder feeds structure-only instruments on the global `Meter`; the
//! engine records at the same `chat`/tool chokepoints as the spans, so metrics and
//! traces share one instrumentation point. Export is an OTLP metric pipeline
//! installed by [`init_otlp_meter`], mirroring the tracer wiring in `otel.rs`.

use std::sync::OnceLock;
use std::time::Duration;

use awaken_runtime_contract::metrics::{InferenceMetric, MetricsRecorder};
use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry_sdk::metrics::SdkMeterProvider;

use crate::config::{OtelConfig, OtelProtocol};

static METER_PROVIDER: OnceLock<SdkMeterProvider> = OnceLock::new();

/// Records the runtime's structure-only metrics onto OpenTelemetry instruments
/// bound to the global `Meter`. Construct it at the composition root **after**
/// [`init_otlp_meter`] has installed the provider, so the instruments bind to the
/// exporting provider rather than the no-op default.
pub struct OtelMetricsRecorder {
    op_count: Counter<u64>,
    op_duration: Histogram<f64>,
    input_tokens: Counter<u64>,
    output_tokens: Counter<u64>,
    tool_count: Counter<u64>,
    tool_duration: Histogram<f64>,
}

impl OtelMetricsRecorder {
    /// Build the instruments on the global meter. Following the OTel GenAI
    /// semantic conventions for the inference metrics; tool metrics use an
    /// `awaken.tool.*` namespace.
    #[must_use]
    pub fn new() -> Self {
        let meter = global::meter("awaken-observability");
        Self {
            op_count: meter
                .u64_counter("gen_ai.client.operation.count")
                .with_description("Completed model-inference calls, by model and outcome.")
                .build(),
            op_duration: meter
                .f64_histogram("gen_ai.client.operation.duration")
                .with_unit("s")
                .with_description("Wall-clock duration of a model-inference call.")
                .build(),
            input_tokens: meter
                .u64_counter("gen_ai.client.input.tokens")
                .with_description("Prompt tokens reported by the provider.")
                .build(),
            output_tokens: meter
                .u64_counter("gen_ai.client.output.tokens")
                .with_description("Completion tokens reported by the provider.")
                .build(),
            tool_count: meter
                .u64_counter("awaken.tool.execution.count")
                .with_description("Completed tool executions, by tool id and outcome.")
                .build(),
            tool_duration: meter
                .f64_histogram("awaken.tool.execution.duration")
                .with_unit("s")
                .with_description("Wall-clock duration of a tool execution.")
                .build(),
        }
    }
}

impl Default for OtelMetricsRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsRecorder for OtelMetricsRecorder {
    fn record_inference(&self, metric: InferenceMetric<'_>) {
        // Structure-only labels: model id + outcome class. Never content/PII.
        let labels = [
            KeyValue::new("gen_ai.request.model", metric.model.to_owned()),
            KeyValue::new("outcome", metric.outcome.to_owned()),
        ];
        self.op_count.add(1, &labels);
        self.op_duration
            .record(metric.duration.as_secs_f64(), &labels);
        if let Some(t) = metric.input_tokens {
            self.input_tokens.add(t, &labels);
        }
        if let Some(t) = metric.output_tokens {
            self.output_tokens.add(t, &labels);
        }
    }

    fn record_tool(&self, tool: &str, outcome: &str, duration: Duration) {
        let labels = [
            KeyValue::new("tool.id", tool.to_owned()),
            KeyValue::new("outcome", outcome.to_owned()),
        ];
        self.tool_count.add(1, &labels);
        self.tool_duration.record(duration.as_secs_f64(), &labels);
    }
}

/// Install an OTLP/HTTP metric pipeline as the global meter provider, so
/// instruments built afterward export. Shares the OTLP endpoint with traces
/// (single-collector setups). A no-op-safe error is returned if no endpoint is
/// configured; the caller logs and continues without metric export.
pub fn init_otlp_meter(
    config: &OtelConfig,
) -> Result<SdkMeterProvider, Box<dyn std::error::Error + Send + Sync>> {
    use opentelemetry_otlp::{MetricExporter, WithExportConfig};
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::metrics::PeriodicReader;

    // Idempotent: `init()` may run more than once (e.g. under tests); install one
    // provider only, and hand back the existing one on a repeat call.
    if let Some(existing) = METER_PROVIDER.get() {
        return Ok(existing.clone());
    }

    // Only HTTP/protobuf is compiled in; a grpc request would need tonic, so use
    // HTTP either way (mirrors the tracer path).
    let _ = matches!(config.effective_traces_protocol(), OtelProtocol::Grpc);

    let endpoint = config
        .effective_traces_endpoint()
        .ok_or("No OTLP endpoint configured")?;

    let exporter = MetricExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .build()?;

    let mut resource_attrs = vec![];
    if let Some(name) = &config.service_name {
        resource_attrs.push(KeyValue::new("service.name", name.clone()));
    }
    if let Some(version) = &config.service_version {
        resource_attrs.push(KeyValue::new("service.version", version.clone()));
    }

    let reader = PeriodicReader::builder(exporter).build();
    let provider = SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(Resource::builder().with_attributes(resource_attrs).build())
        .build();

    global::set_meter_provider(provider.clone());
    let _ = METER_PROVIDER.set(provider.clone());
    Ok(provider)
}

/// Flush and shut down the meter provider, if one was installed, so buffered
/// metrics are delivered before the process exits.
pub(crate) fn shutdown_meter() {
    if let Some(provider) = METER_PROVIDER.get() {
        let _ = provider.force_flush();
        let _ = provider.shutdown();
    }
}
