//! Prometheus metrics endpoint and metric definitions.
//!
//! Installs a `metrics-exporter-prometheus` recorder and exposes a `/metrics`
//! route that renders the Prometheus text exposition format.

use std::sync::OnceLock;
use std::time::Instant;

use axum::extract::MatchedPath;
use axum::http::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// Global handle to the Prometheus recorder for rendering output.
static PROM_HANDLE: OnceLock<Result<PrometheusHandle, String>> = OnceLock::new();

/// Install the Prometheus metrics recorder.
///
/// Must be called once at startup, before any metrics are recorded.
/// Subsequent calls are no-ops.
pub fn install_recorder() {
    if let Err(error) = try_install_recorder() {
        tracing::warn!(error, "failed to install Prometheus metrics recorder");
    }
}

/// Try to install the Prometheus metrics recorder.
///
/// This returns an error instead of panicking when another global metrics
/// recorder has already been installed by the embedding application.
pub fn try_install_recorder() -> Result<(), String> {
    PROM_HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map(|_| ())
        .map_err(Clone::clone)
}

/// Render Prometheus text exposition format.
///
/// Returns `None` if the recorder has not been installed.
pub fn render() -> Option<String> {
    PROM_HANDLE
        .get()
        .and_then(|result| result.as_ref().ok())
        .map(PrometheusHandle::render)
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

/// Set mailbox dispatch depth for a low-cardinality dispatch status.
pub fn set_mailbox_dispatch_depth(status: &str, depth: f64) {
    gauge!(
        "awaken_mailbox_dispatch_depth",
        "status" => status.to_string()
    )
    .set(depth);
}

/// Record a mailbox lifecycle/store operation.
pub fn record_mailbox_operation(operation: &str, result: &str, seconds: f64) {
    counter!(
        "awaken_mailbox_operations_total",
        "operation" => operation.to_string(),
        "result" => result.to_string()
    )
    .increment(1);
    histogram!(
        "awaken_mailbox_operation_duration_seconds",
        "operation" => operation.to_string(),
        "result" => result.to_string()
    )
    .record(seconds);
}

/// Increment a mailbox lifecycle/store operation by count.
pub fn inc_mailbox_operation_by(operation: &str, result: &str, count: u64) {
    if count == 0 {
        return;
    }
    counter!(
        "awaken_mailbox_operations_total",
        "operation" => operation.to_string(),
        "result" => result.to_string()
    )
    .increment(count);
}

/// Record mailbox enqueue → dispatch processing start latency in seconds.
pub fn record_mailbox_dispatch_enqueue_to_start(seconds: f64) {
    histogram!("awaken_mailbox_dispatch_enqueue_to_start_seconds").record(seconds);
}

/// Record mailbox eligible → dispatch processing start latency in seconds.
///
/// `available_at` can be later than `created_at` for retries or delayed
/// dispatches; this metric excludes intentional backoff/delay time.
pub fn record_mailbox_dispatch_eligible_to_start(seconds: f64) {
    histogram!("awaken_mailbox_dispatch_eligible_to_start_seconds").record(seconds);
}

/// Record mailbox claim → dispatch processing start latency in seconds.
pub fn record_mailbox_dispatch_claim_to_start(seconds: f64) {
    histogram!("awaken_mailbox_dispatch_claim_to_start_seconds").record(seconds);
}

/// Record mailbox enqueue → dispatch completion latency in seconds.
pub fn record_mailbox_dispatch_enqueue_to_complete(seconds: f64, outcome: &str) {
    histogram!(
        "awaken_mailbox_dispatch_enqueue_to_complete_seconds",
        "outcome" => outcome.to_string()
    )
    .record(seconds);
}

/// Record runtime execution duration for a mailbox dispatch in seconds.
pub fn record_mailbox_dispatch_runtime(seconds: f64, outcome: &str) {
    histogram!(
        "awaken_mailbox_dispatch_runtime_seconds",
        "outcome" => outcome.to_string()
    )
    .record(seconds);
}

/// Record a completed run and its duration in seconds.
pub fn record_run_completion(seconds: f64, outcome: &str) {
    counter!("awaken_runs_total", "outcome" => outcome.to_string()).increment(1);
    histogram!(
        "awaken_run_duration_seconds",
        "outcome" => outcome.to_string()
    )
    .record(seconds);
}

/// Increment the inference requests counter.
pub fn inc_inference_requests(model: &str, provider: &str, status: &str) {
    counter!(
        "awaken_inference_requests_total",
        "model" => model.to_string(),
        "provider" => provider.to_string(),
        "status" => status.to_string()
    )
    .increment(1);
}

/// Record an inference call duration in seconds.
pub fn record_inference_duration(seconds: f64, model: &str, provider: &str, status: &str) {
    histogram!(
        "awaken_inference_duration_seconds",
        "model" => model.to_string(),
        "provider" => provider.to_string(),
        "status" => status.to_string()
    )
    .record(seconds);
}

/// Increment an inference token counter.
pub fn inc_inference_tokens(model: &str, provider: &str, token_type: &str, count: u64) {
    if count == 0 {
        return;
    }
    counter!(
        "awaken_inference_tokens_total",
        "model" => model.to_string(),
        "provider" => provider.to_string(),
        "type" => token_type.to_string()
    )
    .increment(count);
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

/// Increment active HTTP requests.
pub fn inc_http_in_flight() {
    gauge!("awaken_http_requests_in_flight").increment(1.0);
}

/// Decrement active HTTP requests.
pub fn dec_http_in_flight() {
    gauge!("awaken_http_requests_in_flight").decrement(1.0);
}

/// Record an HTTP request.
pub fn record_http_request(method: &str, route: &str, status: u16, seconds: f64) {
    let status = status.to_string();
    counter!(
        "awaken_http_requests_total",
        "method" => method.to_string(),
        "route" => route.to_string(),
        "status" => status.clone()
    )
    .increment(1);
    histogram!(
        "awaken_http_request_duration_seconds",
        "method" => method.to_string(),
        "route" => route.to_string(),
        "status" => status
    )
    .record(seconds);
}

/// Axum middleware that records low-cardinality HTTP request metrics.
pub async fn http_metrics_middleware(request: Request<axum::body::Body>, next: Next) -> Response {
    let method = request.method().as_str().to_string();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|matched| matched.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string());
    let start = Instant::now();

    inc_http_in_flight();
    let response = next.run(request).await;
    let status = response.status().as_u16();
    record_http_request(&method, &route, status, start.elapsed().as_secs_f64());
    dec_http_in_flight();

    response
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
        record_mailbox_dispatch_enqueue_to_start(0.01);
        record_mailbox_dispatch_eligible_to_start(0.01);
        record_mailbox_dispatch_claim_to_start(0.001);
        record_mailbox_dispatch_enqueue_to_complete(0.5, "completed");
        record_mailbox_dispatch_runtime(0.49, "completed");
        record_mailbox_operation("enqueue", "ok", 0.001);
        inc_mailbox_operation_by("reclaim", "ok", 2);
        set_mailbox_dispatch_depth("queued", 3.0);
        record_run_completion(1.23, "completed");
        inc_inference_requests("gpt-4", "openai", "ok");
        record_inference_duration(0.5, "gpt-4", "openai", "ok");
        inc_inference_tokens("gpt-4", "openai", "input", 10);
        inc_errors("timeout");
        inc_sse_connections();
        dec_sse_connections();
        inc_http_in_flight();
        record_http_request("GET", "/health", 200, 0.001);
        dec_http_in_flight();
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

    #[test]
    fn active_runs_gauge_appears_in_output() {
        install_recorder();
        inc_active_runs();
        inc_active_runs();
        dec_active_runs();
        let output = render().unwrap_or_default();
        assert!(
            output.contains("awaken_active_runs"),
            "expected awaken_active_runs in metrics output"
        );
    }

    #[test]
    fn error_counter_multiple_classes_appear() {
        install_recorder();
        inc_errors("rate_limit");
        inc_errors("timeout");
        inc_errors("rate_limit"); // increment same class again
        let output = render().unwrap_or_default();
        assert!(
            output.contains("awaken_errors_total"),
            "expected awaken_errors_total in metrics output"
        );
    }

    #[test]
    fn sse_connections_gauge_appears_in_output() {
        install_recorder();
        inc_sse_connections();
        inc_sse_connections();
        dec_sse_connections();
        let output = render().unwrap_or_default();
        assert!(
            output.contains("awaken_sse_connections"),
            "expected awaken_sse_connections in metrics output"
        );
    }

    #[test]
    fn inference_metrics_appear_in_output() {
        install_recorder();
        inc_inference_requests("gpt-4", "openai", "ok");
        inc_inference_requests("gpt-4", "openai", "error");
        record_inference_duration(1.5, "gpt-4", "openai", "ok");
        inc_inference_tokens("gpt-4", "openai", "input", 100);
        let output = render().unwrap_or_default();
        assert!(
            output.contains("awaken_inference_requests_total"),
            "expected awaken_inference_requests_total in metrics output"
        );
        assert!(
            output.contains("awaken_inference_duration_seconds"),
            "expected awaken_inference_duration_seconds in metrics output"
        );
        assert!(
            output.contains("awaken_inference_tokens_total"),
            "expected awaken_inference_tokens_total in metrics output"
        );
    }

    #[test]
    fn run_duration_histogram_appears_in_output() {
        install_recorder();
        record_run_completion(0.5, "completed");
        record_run_completion(2.0, "transient_error");
        let output = render().unwrap_or_default();
        assert!(
            output.contains("awaken_runs_total"),
            "expected awaken_runs_total in metrics output"
        );
        assert!(
            output.contains("awaken_run_duration_seconds"),
            "expected awaken_run_duration_seconds in metrics output"
        );
    }

    #[test]
    fn mailbox_queue_depth_gauge_appears_in_output() {
        install_recorder();
        set_mailbox_queue_depth(42.0);
        set_mailbox_dispatch_depth("queued", 42.0);
        let output = render().unwrap_or_default();
        assert!(
            output.contains("awaken_mailbox_queue_depth"),
            "expected awaken_mailbox_queue_depth in metrics output"
        );
        assert!(
            output.contains("awaken_mailbox_dispatch_depth"),
            "expected awaken_mailbox_dispatch_depth in metrics output"
        );
    }

    #[test]
    fn mailbox_dispatch_latency_histograms_appear_in_output() {
        install_recorder();
        record_mailbox_dispatch_enqueue_to_start(0.01);
        record_mailbox_dispatch_eligible_to_start(0.01);
        record_mailbox_dispatch_claim_to_start(0.001);
        record_mailbox_dispatch_enqueue_to_complete(0.5, "completed");
        record_mailbox_dispatch_runtime(0.49, "completed");
        let output = render().unwrap_or_default();
        assert!(
            output.contains("awaken_mailbox_dispatch_enqueue_to_start_seconds"),
            "expected enqueue-to-start mailbox latency histogram in output"
        );
        assert!(
            output.contains("awaken_mailbox_dispatch_eligible_to_start_seconds"),
            "expected eligible-to-start mailbox latency histogram in output"
        );
        assert!(
            output.contains("awaken_mailbox_dispatch_claim_to_start_seconds"),
            "expected claim-to-start mailbox latency histogram in output"
        );
        assert!(
            output.contains("awaken_mailbox_dispatch_enqueue_to_complete_seconds"),
            "expected enqueue-to-complete mailbox latency histogram in output"
        );
        assert!(
            output.contains("awaken_mailbox_dispatch_runtime_seconds"),
            "expected mailbox runtime histogram in output"
        );
    }

    #[test]
    fn mailbox_operation_metrics_appear_in_output() {
        install_recorder();
        record_mailbox_operation("enqueue", "ok", 0.001);
        record_mailbox_operation("claim", "error", 0.002);
        inc_mailbox_operation_by("reclaim", "ok", 2);
        let output = render().unwrap_or_default();
        assert!(
            output.contains("awaken_mailbox_operations_total"),
            "expected mailbox operation counter in output"
        );
        assert!(
            output.contains("awaken_mailbox_operation_duration_seconds"),
            "expected mailbox operation duration histogram in output"
        );
    }

    #[test]
    fn http_metrics_appear_in_output() {
        install_recorder();
        inc_http_in_flight();
        record_http_request("GET", "/health", 200, 0.01);
        dec_http_in_flight();
        let output = render().unwrap_or_default();
        assert!(
            output.contains("awaken_http_requests_total"),
            "expected HTTP request counter in output"
        );
        assert!(
            output.contains("awaken_http_request_duration_seconds"),
            "expected HTTP request duration histogram in output"
        );
        assert!(
            output.contains("awaken_http_requests_in_flight"),
            "expected HTTP in-flight gauge in output"
        );
    }
}
