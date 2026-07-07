//! W3C `traceparent` capture/relay across a durable queue boundary.
//!
//! A run admitted in one task (under the ingress span) is executed later by a
//! dispatch worker — possibly in another process — so the `tracing` span context
//! does not carry across. To keep it one trace, the admitting side captures the
//! current context as a `traceparent` string ([`current_traceparent`]) and
//! persists it on the durable instruction; the worker rebuilds a `wake.dispatch`
//! span whose remote parent is that context ([`dispatch_span`]) and runs the
//! execution inside it, so `runtime.run` nests under the admitting request's trace.
//!
//! Both use the globally-installed `TraceContextPropagator`, so the wire form is
//! exactly the standard `00-<32hex>-<16hex>-<2hex>` header.

use std::collections::HashMap;

use opentelemetry::propagation::{Extractor, Injector};
use tracing_opentelemetry::OpenTelemetrySpanExt;

struct MapInjector<'a>(&'a mut HashMap<String, String>);
impl Injector for MapInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        self.0.insert(key.to_string(), value);
    }
}

struct MapExtractor<'a>(&'a HashMap<String, String>);
impl Extractor for MapExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
}

/// Capture the current span's trace context as a W3C `traceparent` string, to be
/// persisted on a durable instruction. Returns `None` when there is no valid
/// context to propagate (e.g. tracing disabled or an unsampled root).
pub fn current_traceparent() -> Option<String> {
    let cx = tracing::Span::current().context();
    let mut carrier = HashMap::new();
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&cx, &mut MapInjector(&mut carrier));
    });
    carrier.remove("traceparent")
}

/// Build the `wake.dispatch` span for a durably-dispatched run. When `traceparent`
/// is present it becomes the span's remote parent, so the execution driven inside
/// this span (`runtime.run` → …) continues the admitting request's trace across
/// the queue boundary. `SpanKind::Consumer` marks the queue-consuming side.
pub fn dispatch_span(traceparent: Option<&str>) -> tracing::Span {
    let span = tracing::info_span!("wake.dispatch", otel.kind = "consumer");
    if let Some(traceparent) = traceparent {
        let mut carrier = HashMap::new();
        carrier.insert("traceparent".to_string(), traceparent.to_string());
        let cx = opentelemetry::global::get_text_map_propagator(|propagator| {
            propagator.extract(&MapExtractor(&carrier))
        });
        span.set_parent(cx);
    }
    span
}
