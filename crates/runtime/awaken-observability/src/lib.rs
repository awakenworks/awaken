//! Cross-cutting telemetry infrastructure for the Awaken service binaries.
//!
//! This is **not** a runtime `ext-*`: it plugs into no agent-runtime seam and carries
//! no domain capability. Its whole job is the process-global observability plumbing:
//!
//!   * [`init`] installs the `tracing` subscriber — an `EnvFilter` (`RUST_LOG`,
//!     default `info`) over a text/JSON formatter, plus an OpenTelemetry layer when a
//!     trace sink is configured (`AWAKEN_TRACE_FILE` or `OTEL_EXPORTER_OTLP_ENDPOINT`).
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
mod otel;
mod propagation;

pub use config::{OtelConfig, OtelConfigBuilder, OtelProtocol};
pub use http::trace_http;
pub use otel::init_otlp_tracer;
pub use propagation::{current_traceparent, dispatch_span};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

/// Install the process-global tracing subscriber from the environment.
///
/// Idempotent: a second call (e.g. a test that already installed a subscriber) is
/// ignored rather than panicking. `AWAKEN_LOG_FORMAT=json` switches the formatter to
/// structured JSON lines. When a trace sink is configured the OpenTelemetry export
/// layer is appended; otherwise the process keeps the fmt-only path.
pub fn init() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let json = log_format_is_json(std::env::var("AWAKEN_LOG_FORMAT").ok().as_deref());

    // Box the formatter so the JSON/text choice unifies to one layer type.
    let fmt_layer: Box<dyn Layer<_> + Send + Sync> = if json {
        fmt::layer().json().boxed()
    } else {
        fmt::layer().boxed()
    };

    let subscriber = tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer);

    if let Some(layer) = otel::build_layer() {
        subscriber.with(layer).try_init().ok();
    } else {
        subscriber.try_init().ok();
    }
}

/// Whether `AWAKEN_LOG_FORMAT` selects structured JSON lines (case-insensitive
/// `json`); any other value, or unset, keeps the text formatter.
fn log_format_is_json(value: Option<&str>) -> bool {
    value
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false)
}

/// Flush and shut down the trace exporter, if one was installed, so buffered spans
/// are delivered before the process exits. A no-op when no exporter was configured.
pub fn shutdown() {
    otel::shutdown();
}

#[cfg(test)]
mod tests {
    use super::log_format_is_json;

    #[test]
    fn log_format_knob_selects_json_case_insensitively() {
        assert!(log_format_is_json(Some("json")));
        assert!(log_format_is_json(Some("JSON")));
        assert!(log_format_is_json(Some("Json")));
        assert!(!log_format_is_json(None));
        assert!(!log_format_is_json(Some("")));
        assert!(!log_format_is_json(Some("text")));
    }
}
