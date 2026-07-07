//! Axum ingress middleware: root one span per HTTP request, parented to the inbound
//! W3C `traceparent` when present.
//!
//! Mounted once on the assembled router (`.layer(from_fn(trace_http))`), this is the
//! top of every server-side trace. It extracts the upstream trace context from the
//! request headers via the globally-installed `TraceContextPropagator`, creates the
//! `http.request` span, sets that context as the span's remote parent, and runs the
//! rest of the stack inside the span. Since the direct request→inference path is
//! spawn-free, all deeper `#[instrument]` spans nest under this one automatically.

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::propagation::Extractor;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Read-only view of the request headers for the OTel text-map propagator.
struct HeaderExtractor<'a>(&'a axum::http::HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// Ingress middleware that roots the per-request span. Use as
/// `router.layer(axum::middleware::from_fn(trace_http))`.
pub async fn trace_http(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let route = request.uri().path().to_string();

    // Extract the inbound trace context before the request is moved into the span.
    let parent_cx = opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(request.headers()))
    });

    // Span name is static (`http.request`); the route lives in an attribute so the
    // trace validator can key on it (ids templatized downstream).
    let span = tracing::info_span!(
        "http.request",
        otel.kind = "server",
        http.request.method = %method,
        http.route = %route,
        http.response.status_code = tracing::field::Empty,
    );
    span.set_parent(parent_cx);

    let response = async move { next.run(request).await }
        .instrument(span.clone())
        .await;

    span.record(
        "http.response.status_code",
        response.status().as_u16() as i64,
    );
    response
}
