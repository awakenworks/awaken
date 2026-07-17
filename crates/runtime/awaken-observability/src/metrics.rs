//! OpenTelemetry-backed implementation of the runtime's [`MetricsRecorder`] port
//! (#2). The recorder feeds structure-only instruments on the global `Meter`; the
//! engine records at the same `chat`/tool chokepoints as the spans, so metrics and
//! traces share one instrumentation point. Export is an OTLP metric pipeline
//! installed by [`init_meters`], mirroring the tracer wiring in `otel.rs`.

use std::sync::OnceLock;
use std::time::Duration;

use awaken_runtime_contract::metrics::{InferenceMetric, MetricsRecorder};
use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry_sdk::metrics::SdkMeterProvider;

use crate::config::{OtelConfig, OtelProtocol};

static METER_PROVIDER: OnceLock<SdkMeterProvider> = OnceLock::new();
/// The Prometheus registry the global meter provider collects into, so
/// [`render_prometheus`] can encode a scrape of the whole process's metrics.
static PROM_REGISTRY: OnceLock<prometheus::Registry> = OnceLock::new();

/// Records the runtime's structure-only metrics onto OpenTelemetry instruments
/// bound to the global `Meter`. Construct it at the composition root **after**
/// [`init_meters`] has installed the provider, so the instruments bind to the
/// exporting provider rather than the no-op default.
pub struct OtelMetricsRecorder {
    op_count: Counter<u64>,
    op_duration: Histogram<f64>,
    input_tokens: Counter<u64>,
    output_tokens: Counter<u64>,
    tool_count: Counter<u64>,
    tool_duration: Histogram<f64>,
    dispatch_claimed: Counter<u64>,
    dispatch_settled: Counter<u64>,
    dispatch_drive: Histogram<f64>,
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
            dispatch_claimed: meter
                .u64_counter("awaken.dispatch.runs.claimed")
                .with_description("Durable dispatches claimed for execution by a worker.")
                .build(),
            dispatch_settled: meter
                .u64_counter("awaken.dispatch.runs.settled")
                .with_description("Durable dispatches settled by a worker, by outcome.")
                .build(),
            dispatch_drive: meter
                .f64_histogram("awaken.dispatch.drive.duration")
                .with_unit("s")
                .with_description("Wall-clock time a worker spent driving one claimed dispatch.")
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

    fn record_dispatch_claimed(&self) {
        self.dispatch_claimed.add(1, &[]);
    }

    fn record_dispatch_settled(&self, outcome: &str) {
        // Structure-only label: the settle outcome class (`done`/`parked`).
        let labels = [KeyValue::new("outcome", outcome.to_owned())];
        self.dispatch_settled.add(1, &labels);
    }

    fn record_dispatch_drive(&self, duration: Duration) {
        self.dispatch_drive.record(duration.as_secs_f64(), &[]);
    }
}

/// Install the process-global meter provider with BOTH a Prometheus *scrape* reader
/// (always) and an OTLP *push* reader (when an endpoint is configured). One provider,
/// one meter: every instrument — the business `gen_ai.*`/`awaken.*` metrics AND a
/// role's admin gauges — is both scrapeable at `/metrics` (via [`render_prometheus`])
/// and pushed over OTLP when a collector is set. Idempotent; call once at startup.
///
/// Unlike the old OTLP-only path, this ALWAYS installs a provider, so a
/// Prometheus-only deployment (no OTLP collector) still has working metrics.
pub fn init_meters(
    config: &OtelConfig,
) -> Result<SdkMeterProvider, Box<dyn std::error::Error + Send + Sync>> {
    use opentelemetry_otlp::{MetricExporter, WithExportConfig};
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader;
    use opentelemetry_sdk::runtime;

    // Idempotent: `init()` may run more than once (e.g. under tests); install one
    // provider only, and hand back the existing one on a repeat call.
    if let Some(existing) = METER_PROVIDER.get() {
        return Ok(existing.clone());
    }

    let mut resource_attrs = vec![];
    if let Some(name) = &config.service_name {
        resource_attrs.push(KeyValue::new("service.name", name.clone()));
    }
    if let Some(version) = &config.service_version {
        resource_attrs.push(KeyValue::new("service.version", version.clone()));
    }

    // The Prometheus scrape reader (always): the OSS OTel→Prometheus bridge collects
    // every instrument into this registry, which `/metrics` encodes on demand.
    let registry = prometheus::Registry::new();
    let prom_reader = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        .build()?;
    let mut builder = SdkMeterProvider::builder()
        .with_reader(prom_reader)
        .with_resource(Resource::builder().with_attributes(resource_attrs).build());

    // The OTLP push reader (only when an endpoint is configured). Only HTTP/protobuf
    // is compiled in; a grpc request would need tonic, so use HTTP. Surface the
    // downgrade so a `grpc` misconfiguration is observable instead of silent.
    if matches!(config.effective_traces_protocol(), OtelProtocol::Grpc) {
        tracing::warn!(
            "OTLP protocol 'grpc' requested but only http/protobuf is compiled in; \
             exporting metrics over HTTP"
        );
    }
    if let Some(endpoint) = config.effective_traces_endpoint() {
        let exporter = MetricExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .build()?;
        // Export interval: default 60s, overridable via the standard
        // `OTEL_METRIC_EXPORT_INTERVAL` (ms) so a test/dev can flush quickly.
        let interval_ms = std::env::var("OTEL_METRIC_EXPORT_INTERVAL")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(60_000);
        // The async-runtime reader drives the reqwest-based OTLP exporter on the
        // Tokio runtime; the plain std-thread reader cannot run the async export.
        let reader = PeriodicReader::builder(exporter, runtime::Tokio)
            .with_interval(std::time::Duration::from_millis(interval_ms))
            .build();
        builder = builder.with_reader(reader);
    }

    let provider = builder.build();
    global::set_meter_provider(provider.clone());
    let _ = METER_PROVIDER.set(provider.clone());
    let _ = PROM_REGISTRY.set(registry);
    Ok(provider)
}

/// Render the whole process's metrics in Prometheus text exposition format — a scrape
/// of the global registry installed by [`init_meters`]. Any observable-gauge callback
/// fires here (at scrape time). Empty when no provider was installed. This is what a
/// role's admin `/metrics` endpoint returns.
#[must_use]
pub fn render_prometheus() -> String {
    let Some(registry) = PROM_REGISTRY.get() else {
        return String::new();
    };
    let metric_families = registry.gather();
    let mut buf = String::new();
    if prometheus::TextEncoder::new()
        .encode_utf8(&metric_families, &mut buf)
        .is_err()
    {
        return String::new();
    }
    buf
}

/// Flush and shut down the meter provider, if one was installed, so buffered
/// metrics are delivered before the process exits. The flush + shutdown run on a
/// detached OS thread with a bounded wait: a stalled OTLP export (or a periodic
/// reader whose export task contends with the shutting-down runtime) must never
/// hang process exit. The thread frees the runtime's workers to drive the async
/// export, so a reachable collector still receives the final batch.
pub(crate) fn shutdown_meter() {
    let Some(provider) = METER_PROVIDER.get().cloned() else {
        return;
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = provider.force_flush();
        let _ = provider.shutdown();
        let _ = tx.send(());
    });
    let _ = rx.recv_timeout(std::time::Duration::from_secs(3));
}

#[cfg(test)]
mod meter_tests {
    use super::*;
    use opentelemetry::global;

    // Installing a Prometheus-only provider (no OTLP endpoint) makes both an
    // observable gauge on the global meter AND the `OtelMetricsRecorder`'s business
    // instruments renderable as Prometheus text at scrape time.
    //
    // NOTE: this is deliberately ONE test, not two. `init_meters`/`render_prometheus`
    // are process-global (`OnceLock` provider + `OnceLock` registry) and `global::meter`
    // caches per-name; two meter tests running concurrently can race the global
    // install (the `set_meter_provider` "last wins" can diverge from the `PROM_REGISTRY`
    // "first wins"), scraping the wrong registry. Keeping a single meter test makes the
    // assertion deterministic without any src change or a `serial_test` dev-dep.
    #[test]
    fn init_meters_installs_a_prometheus_scrape_of_the_global_meter() {
        // No endpoint → Prometheus reader only (no OTLP push). Idempotent install.
        init_meters(&OtelConfig::default()).expect("install the global meter provider");

        let _gauge = global::meter("awaken-observability-test")
            .u64_observable_gauge("awaken_meter_test_gauge")
            .with_description("A test gauge.")
            .with_callback(|obs| obs.observe(7, &[]))
            .build();

        let scrape = render_prometheus();
        assert!(
            scrape
                .lines()
                .any(|l| l.starts_with("awaken_meter_test_gauge") && l.trim_end().ends_with(" 7")),
            "the global meter's gauge is scrapeable: {scrape}"
        );

        // --- OtelMetricsRecorder real emission ---
        // The recorder binds its instruments to the same global meter; recording
        // through the real `MetricsRecorder` port must surface those instruments in
        // the scrape — the same oracle the admin `/metrics` endpoint returns.
        use awaken_runtime_contract::metrics::{InferenceMetric, MetricsRecorder};
        use std::time::Duration;

        let recorder = OtelMetricsRecorder::new();

        recorder.record_inference(InferenceMetric {
            model: "oracle-model",
            outcome: "ok",
            duration: Duration::from_millis(12),
            input_tokens: Some(41),
            output_tokens: Some(7),
        });
        recorder.record_tool("oracle-tool", "ok", Duration::from_millis(3));
        recorder.record_dispatch_claimed();
        recorder.record_dispatch_settled("done");
        recorder.record_dispatch_drive(Duration::from_millis(9));

        let scrape = render_prometheus();

        // Every instrument the recorder feeds must show up in the scrape. Assert on
        // the sanitized metric-name stems (opentelemetry-prometheus maps `.`→`_` and
        // may add `_total`/unit suffixes), so the check is robust to suffix details.
        for stem in [
            "gen_ai_client_operation_count",
            "gen_ai_client_operation_duration",
            "gen_ai_client_input_tokens",
            "gen_ai_client_output_tokens",
            "awaken_tool_execution_count",
            "awaken_tool_execution_duration",
            "awaken_dispatch_runs_claimed",
            "awaken_dispatch_runs_settled",
            "awaken_dispatch_drive_duration",
        ] {
            assert!(
                scrape.contains(stem),
                "instrument `{stem}` must appear in the scrape:\n{scrape}"
            );
        }

        // Structure-only labels are carried through (model id + outcome), never content.
        assert!(
            scrape.contains("oracle-model") && scrape.contains("oracle-tool"),
            "the structure-only labels are present in the scrape:\n{scrape}"
        );
    }
}
