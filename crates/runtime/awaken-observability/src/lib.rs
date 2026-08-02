//! Cross-cutting telemetry infrastructure for the Awaken service binaries.
//!
//! This is **not** a runtime `ext-*`: it plugs into no agent-runtime seam and carries
//! no domain capability. Its whole job is the process-global observability plumbing:
//!
//!   * [`init`] installs the configured filter and text/JSON formatter, plus an
//!     OpenTelemetry layer when the typed deployment policy names a trace sink.
//!     It also installs the W3C `traceparent` propagator so an inbound trace continues
//!     through this process.
//!   * [`trace_http`] is the axum ingress middleware that extracts the inbound
//!     `traceparent` and roots one `http.request` span per request; because the direct
//!     request→inference path is spawn-free, every `#[instrument]` span deeper in the
//!     stack (managed handler → host → runtime → provider → tool) nests under it and is
//!     exported on the same trace.
//!   * [`shutdown`] flushes buffered spans before the process exits.
//!
//! The spans themselves live in the crates they describe (via `#[instrument]`); this
//! crate never emits domain spans, only owns the exporter and propagation wiring.

mod config;
mod http;
mod metrics;
mod otel;
mod propagation;

pub use config::{LogFormat, ObservabilityConfig, OtelConfig, OtelConfigBuilder, OtelProtocol};
pub use http::trace_http;
pub use metrics::{
    OtelMetricsRecorder, add_live_hand, init_meters, record_hand_lifecycle, render_prometheus,
};
pub use otel::init_otlp_tracer;
pub use propagation::{current_traceparent, dispatch_span};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

/// Install the process-global tracing subscriber from typed deployment policy.
///
/// Idempotent: a second call (e.g. a test that already installed a subscriber) is
/// ignored rather than panicking. When a trace sink is configured the OpenTelemetry
/// export layer is appended; otherwise the process keeps the fmt-only path.
pub fn init(config: &ObservabilityConfig) {
    let env_filter = EnvFilter::try_new(&config.filter).unwrap_or_else(|_| EnvFilter::new("info"));

    // Box the formatter so the JSON/text choice unifies to one layer type.
    let fmt_layer: Box<dyn Layer<_> + Send + Sync> = if config.log_format == LogFormat::Json {
        fmt::layer().json().boxed()
    } else {
        fmt::layer().boxed()
    };

    let subscriber = tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer);

    if let Some(layer) = otel::build_layer(config) {
        subscriber.with(layer).try_init().ok();
    } else {
        subscriber.try_init().ok();
    }

    // Install the global meter provider ALWAYS (a Prometheus scrape reader + an OTLP
    // push reader when configured), so `/metrics` works with or without a collector
    // and every instrument is exposed both ways (#2). Best-effort: a meter-init
    // failure must never stop the process.
    if let Err(error) = metrics::init_meters(&config.otel) {
        tracing::warn!(%error, "meter init failed; continuing without metrics");
    }
}

/// Flush and shut down the trace exporter, if one was installed, so buffered spans
/// are delivered before the process exits. A no-op when no exporter was configured.
pub fn shutdown() {
    otel::shutdown();
    metrics::shutdown_meter();
}
