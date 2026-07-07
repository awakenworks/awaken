//! Exporter wiring: build the `tracing_opentelemetry` layer from the environment
//! and flush it on shutdown.
//!
//! Two collector sinks are supported, in priority order:
//!   1. `AWAKEN_TRACE_FILE` — every finished span appended as one JSON line. This
//!      is the collector-free sink the e2e trace validator reads back, so span
//!      trees can be asserted without a running OTLP collector.
//!   2. `OTEL_EXPORTER_OTLP_ENDPOINT` (or `…_TRACES_ENDPOINT`) — standard OTLP/HTTP
//!      export over a Tokio batch span processor (Phoenix / Jaeger / collector).
//!
//! When neither is configured the layer is `None` and the process keeps the
//! fmt-only logging path. The W3C `traceparent` propagator is installed
//! unconditionally so an inbound trace continues across plane boundaries.

use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
use tracing_subscriber::registry::LookupSpan;

use crate::config::{OtelConfig, OtelProtocol};

/// Kept for the process lifetime so the batch span processor keeps exporting;
/// also the handle [`shutdown`] uses to flush buffered spans on exit.
static PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

const TRACE_FILE_ENV: &str = "AWAKEN_TRACE_FILE";

/// Build the OpenTelemetry layer when a trace sink is configured. Generic over the
/// subscriber `S` so the concrete layered type is inferred at the call site.
/// Returns `None` when no sink is configured (fmt-only path).
pub(crate) fn build_layer<S>() -> Option<tracing_opentelemetry::OpenTelemetryLayer<S, SdkTracer>>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    // Install the W3C propagator regardless of sink so inbound/outbound context is
    // carried across plane boundaries (ingress → runtime → provider → tool).
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    if let Some(path) = std::env::var_os(TRACE_FILE_ENV) {
        let exporter = file_exporter::JsonFileSpanExporter::new(path);
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter)
            .build();
        let tracer = provider.tracer("awaken-observability");
        opentelemetry::global::set_tracer_provider(provider.clone());
        let _ = PROVIDER.set(provider);
        return Some(tracing_opentelemetry::layer().with_tracer(tracer));
    }

    let config = OtelConfig::from_env();
    if !config.is_configured() {
        return None;
    }
    let (provider, tracer) = match init_otlp_tracer(&config) {
        Ok(pair) => pair,
        Err(error) => {
            tracing::warn!(%error, "OTLP tracer init failed; continuing without trace export");
            return None;
        }
    };
    opentelemetry::global::set_tracer_provider(provider.clone());
    let _ = PROVIDER.set(provider);
    Some(tracing_opentelemetry::layer().with_tracer(tracer))
}

/// Initialise an OTLP/HTTP tracer from the given configuration. Returns the
/// provider (kept alive by the caller) and a tracer for the subscriber layer.
pub fn init_otlp_tracer(
    config: &OtelConfig,
) -> Result<(SdkTracerProvider, SdkTracer), Box<dyn std::error::Error + Send + Sync>> {
    use opentelemetry_otlp::{SpanExporter, WithExportConfig};
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::runtime;
    use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;

    // Only HTTP/protobuf is compiled in (the `http-proto` exporter feature); a grpc
    // request would need the tonic transport, so fall back to HTTP either way.
    let _ = matches!(config.effective_traces_protocol(), OtelProtocol::Grpc);

    let endpoint = config
        .effective_traces_endpoint()
        .ok_or("No OTLP endpoint configured")?;

    let exporter = SpanExporter::builder()
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

    let batch = BatchSpanProcessor::builder(exporter, runtime::Tokio).build();
    let provider = SdkTracerProvider::builder()
        .with_span_processor(batch)
        .with_resource(Resource::builder().with_attributes(resource_attrs).build())
        .build();

    let tracer = provider.tracer("awaken-observability");
    Ok((provider, tracer))
}

/// Flush and shut down the exporter, if one was installed, so buffered spans are
/// delivered before the process exits.
pub(crate) fn shutdown() {
    if let Some(provider) = PROVIDER.get() {
        let _ = provider.force_flush();
        let _ = provider.shutdown();
    }
}

/// A collector-free `SpanExporter` that appends each finished span to a file as one
/// JSON line (`{name, trace_id, span_id, parent_span_id, attributes}`).
mod file_exporter {
    use std::ffi::OsString;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::sync::Mutex;

    use opentelemetry::trace::SpanId;
    use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
    use opentelemetry_sdk::trace::{SpanData, SpanExporter};

    #[derive(Debug)]
    pub(crate) struct JsonFileSpanExporter {
        path: OsString,
        // Serialize concurrent batch writes so JSON lines never interleave.
        lock: Mutex<()>,
    }

    impl JsonFileSpanExporter {
        pub(crate) fn new(path: OsString) -> Self {
            Self {
                path,
                lock: Mutex::new(()),
            }
        }

        fn write_batch(&self, batch: &[SpanData]) -> std::io::Result<()> {
            let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            for span in batch {
                let attrs: serde_json::Map<String, serde_json::Value> = span
                    .attributes
                    .iter()
                    .map(|kv| {
                        (
                            kv.key.to_string(),
                            serde_json::Value::String(kv.value.to_string()),
                        )
                    })
                    .collect();
                let parent = span.parent_span_id;
                let line = serde_json::json!({
                    "name": span.name.as_ref(),
                    "trace_id": format!("{:032x}", u128::from_be_bytes(span.span_context.trace_id().to_bytes())),
                    "span_id": format!("{:016x}", u64::from_be_bytes(span.span_context.span_id().to_bytes())),
                    "parent_span_id": if parent == SpanId::INVALID {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::String(format!("{:016x}", u64::from_be_bytes(parent.to_bytes())))
                    },
                    "attributes": attrs,
                });
                writeln!(file, "{line}")?;
            }
            file.flush()
        }
    }

    impl SpanExporter for JsonFileSpanExporter {
        fn export(
            &mut self,
            batch: Vec<SpanData>,
        ) -> futures::future::BoxFuture<'static, OTelSdkResult> {
            let result = self
                .write_batch(&batch)
                .map_err(|e| OTelSdkError::InternalFailure(e.to_string()));
            Box::pin(async move { result })
        }
    }
}
