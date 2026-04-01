//! Prometheus metrics endpoint and metric definitions.
//!
//! Installs a `metrics-exporter-prometheus` recorder and exposes a `/metrics`
//! route that renders the Prometheus text exposition format.

use std::sync::OnceLock;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// Global handle to the Prometheus recorder for rendering output.
static PROM_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the Prometheus metrics recorder.
///
/// Must be called once at startup, before any metrics are recorded.
/// Subsequent calls are no-ops.
pub fn install_recorder() {
    PROM_HANDLE.get_or_init(|| {
        PrometheusBuilder::new()
            .install_recorder()
            .expect("failed to install Prometheus recorder")
    });
}

/// Render Prometheus text exposition format.
///
/// Returns `None` if the recorder has not been installed.
pub fn render() -> Option<String> {
    PROM_HANDLE.get().map(|h| h.render())
}

// ── Metric helpers ──────────────────────────────────────────────────

/// Increment the active runs gauge.
pub fn inc_active_runs() {
    gauge!("awaken_active_runs").increment(1.0);
}

/// Decrement the active runs gauge.
pub fn dec_active_runs() {
    gauge!("awaken_active_runs").decrement(1.0);
}

/// Set the mailbox queue depth gauge for a given thread.
pub fn set_mailbox_queue_depth(depth: f64) {
    gauge!("awaken_mailbox_queue_depth").set(depth);
}

/// Record a run duration in seconds.
pub fn record_run_duration(seconds: f64) {
    histogram!("awaken_run_duration_seconds").record(seconds);
}

/// Increment the inference requests counter.
pub fn inc_inference_requests(model: &str, status: &str) {
    counter!("awaken_inference_requests_total", "model" => model.to_string(), "status" => status.to_string())
        .increment(1);
}

/// Record an inference call duration in seconds.
pub fn record_inference_duration(seconds: f64) {
    histogram!("awaken_inference_duration_seconds").record(seconds);
}

/// Increment the errors counter by error class.
pub fn inc_errors(class: &str) {
    counter!("awaken_errors_total", "class" => class.to_string()).increment(1);
}

/// Increment the active SSE connections gauge.
pub fn inc_sse_connections() {
    gauge!("awaken_sse_connections").increment(1.0);
}

/// Decrement the active SSE connections gauge.
pub fn dec_sse_connections() {
    gauge!("awaken_sse_connections").decrement(1.0);
}

// ── Route handler ───────────────────────────────────────────────────

/// GET /metrics — Prometheus scrape endpoint.
pub async fn metrics_handler() -> impl IntoResponse {
    match render() {
        Some(body) => (
            StatusCode::OK,
            [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
            body,
        )
            .into_response(),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "metrics recorder not installed",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_recorder_is_idempotent() {
        install_recorder();
        install_recorder(); // should not panic
    }

    #[test]
    fn render_returns_some_after_install() {
        install_recorder();
        let output = render();
        assert!(output.is_some());
    }

    #[test]
    fn metric_helpers_do_not_panic() {
        install_recorder();
        inc_active_runs();
        dec_active_runs();
        set_mailbox_queue_depth(5.0);
        record_run_duration(1.23);
        inc_inference_requests("gpt-4", "ok");
        record_inference_duration(0.5);
        inc_errors("timeout");
        inc_sse_connections();
        dec_sse_connections();
    }

    #[test]
    fn render_contains_recorded_metrics() {
        install_recorder();
        inc_errors("rate_limit");
        let output = render().unwrap();
        // The prometheus exporter should include our metric name
        assert!(
            output.contains("awaken_errors_total") || output.contains("awaken_active_runs"),
            "expected metric names in output"
        );
    }
}
