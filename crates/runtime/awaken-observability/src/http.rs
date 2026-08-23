//! Axum ingress middleware: root one span per HTTP request, parented to the inbound
//! W3C `traceparent` when present.
//!
//! Mounted once on the assembled router (`.layer(from_fn(trace_http))`), this is the
//! top of every server-side trace. It extracts the upstream trace context from the
//! request headers via the globally-installed `TraceContextPropagator`, creates the
//! `http.request` span, sets that context as the span's remote parent, and runs the
//! rest of the stack inside the span. Since the direct request→inference path is
//! spawn-free, all deeper `#[instrument]` spans nest under this one automatically.

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::propagation::Extractor;
use tracing::Instrument;

use crate::propagation::set_remote_parent_from_extractor;

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
    // Cause/effect rule: a matched request records the router-owned template so
    // opaque ids never create high-cardinality telemetry; an unmatched/fallback
    // request has no MatchedPath and retains its literal path for diagnostics.
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|matched| matched.as_str().to_string())
        .unwrap_or_else(|| request.uri().path().to_string());

    // Extract the inbound trace context before the request is moved into the span.
    let parent = HeaderExtractor(request.headers());

    // Span name is static (`http.request`); the route lives in an attribute so the
    // trace validator can key on it (ids templatized downstream).
    let span = tracing::info_span!(
        "http.request",
        otel.kind = "server",
        http.request.method = %method,
        http.route = %route,
        http.response.status_code = tracing::field::Empty,
    );
    set_remote_parent_from_extractor(&span, &parent);

    let response = async move { next.run(request).await }
        .instrument(span.clone())
        .await;

    span.record(
        "http.response.status_code",
        response.status().as_u16() as i64,
    );
    response
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    use crate::propagation::test_support::{CapturedSpan, capture_spans};

    /// Drive one request through a `Router` carrying the real `trace_http` layer,
    /// under a scoped subscriber whose OTel layer exports finished spans into a
    /// capturing sink. Returns the response status and every captured span.
    ///
    /// Spawn-free: the whole stack is polled to completion on this thread via the
    /// `futures` executor, so the thread-local subscriber applies and the emitted
    /// span tree is deterministic (no ordering sleeps).
    fn run_through_ingress(
        request: Request<Body>,
        status: StatusCode,
    ) -> (StatusCode, Vec<CapturedSpan>) {
        run_through_route(request, status, "/x")
    }

    fn run_through_route(
        request: Request<Body>,
        status: StatusCode,
        route: &str,
    ) -> (StatusCode, Vec<CapturedSpan>) {
        let handler = move || async move { status };
        let app = Router::new()
            .route(route, get(handler))
            .layer(axum::middleware::from_fn(super::trace_http));

        // A current-thread tokio runtime drives the request synchronously on THIS
        // thread, so the thread-local subscriber applies. (A `futures::block_on`
        // outer would panic: the SDK's simple span exporter flushes via its own
        // `futures` executor when the span ends, and nesting the two is forbidden.)
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build a current-thread runtime");
        let (response, spans) = capture_spans("awaken-observability-http-oracle", || {
            rt.block_on(app.oneshot(request))
                .expect("router is infallible")
        });
        let out_status = response.status();
        (out_status, spans)
    }

    fn http_request_span(spans: &[CapturedSpan]) -> &CapturedSpan {
        spans
            .iter()
            .find(|s| s.name == "http.request")
            .expect("the ingress middleware rooted an http.request span")
    }

    #[test]
    fn inbound_traceparent_roots_the_request_span_under_the_incoming_trace() {
        // The upstream trace id + span id presented on the wire.
        let wire_trace = "0af7651916cd43dd8448eb211c80319c";
        let wire_span = "b7ad6b7169203331";
        let tp = format!("00-{wire_trace}-{wire_span}-01");
        let request = Request::builder()
            .uri("/x")
            .method("GET")
            .header("traceparent", &tp)
            .body(Body::empty())
            .unwrap();

        let (status, spans) = run_through_ingress(request, StatusCode::OK);
        assert_eq!(status, StatusCode::OK, "the handler still ran");

        let span = http_request_span(&spans);
        // Oracle: the request span continues the INCOMING trace (continuity)…
        assert_eq!(
            span.trace_id, wire_trace,
            "the request span joins the inbound trace: {span:?}"
        );
        // …and its parent IS the wire span id — real remote-parent linkage, not a
        // fresh root that merely shares a trace id.
        assert_eq!(
            span.parent_span_id.as_deref(),
            Some(wire_span),
            "the request span's remote parent is the inbound span id: {span:?}"
        );
    }

    #[test]
    fn response_status_code_is_recorded_on_the_span() {
        let request = Request::builder()
            .uri("/x")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        // A non-2xx code so the assertion cannot pass by coincidence of a default.
        let (status, spans) = run_through_ingress(request, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

        let span = http_request_span(&spans);
        assert_eq!(
            span.attributes
                .get("http.response.status_code")
                .map(String::as_str),
            Some("503"),
            "the response status code is recorded on the exported span: {span:?}"
        );
        // The route is carried as an attribute (span name stays static).
        assert_eq!(
            span.attributes.get("http.route").map(String::as_str),
            Some("/x"),
            "the route lives in an attribute: {span:?}"
        );
    }

    #[test]
    fn matched_dynamic_route_records_the_template_not_the_opaque_id() {
        // Decision table: R1 matched dynamic path -> canonical MatchedPath;
        // R2 unmatched path -> literal fallback (covered by middleware behavior).
        // This rule prevents Session ids containing ':' from leaking into the
        // metric/trace cardinality and makes route aggregation exact.
        let request = Request::builder()
            .uri("/v1/sessions/sesn_fnv1a64:abc/events")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let (status, spans) =
            run_through_route(request, StatusCode::OK, "/v1/sessions/{id}/events");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            http_request_span(&spans)
                .attributes
                .get("http.route")
                .map(String::as_str),
            Some("/v1/sessions/{id}/events"),
            "R1"
        );
    }

    #[test]
    fn malformed_traceparent_does_not_fail_open_and_roots_a_fresh_span() {
        // Garbage in the header must not panic, must not be accepted as a parent,
        // and must not stop the request being handled.
        let request = Request::builder()
            .uri("/x")
            .method("GET")
            .header("traceparent", "this-is-not-a-traceparent")
            .body(Body::empty())
            .unwrap();

        let (status, spans) = run_through_ingress(request, StatusCode::OK);
        // No fail-open: the request is still handled normally.
        assert_eq!(
            status,
            StatusCode::OK,
            "a malformed header still serves the request"
        );

        let span = http_request_span(&spans);
        // A fresh root: no remote parent was adopted from the garbage header.
        assert_eq!(
            span.parent_span_id, None,
            "an unparseable traceparent yields a fresh root, not an adopted parent: {span:?}"
        );
        // The fresh trace id is real (32 hex, non-zero) — never the invalid all-zero id.
        assert_eq!(span.trace_id.len(), 32, "a 32-hex trace id: {span:?}");
        assert!(
            span.trace_id.chars().all(|c| c.is_ascii_hexdigit()),
            "the trace id is hex: {span:?}"
        );
        assert_ne!(
            span.trace_id, "00000000000000000000000000000000",
            "a fresh, valid (non-zero) trace id is minted: {span:?}"
        );
    }
}
