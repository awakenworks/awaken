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

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;

    /// Run `f` under a scoped subscriber carrying a real (collector-free, in-memory)
    /// OpenTelemetry layer plus the globally-installed W3C propagator, so `set_parent`
    /// actually stores an otel context and `current_traceparent` can re-inject it.
    fn with_otel_subscriber<T>(f: impl FnOnce() -> T) -> T {
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
        let provider = SdkTracerProvider::builder().build();
        let tracer = provider.tracer("awaken-observability-test");
        let subscriber =
            Registry::default().with(tracing_opentelemetry::layer().with_tracer(tracer));
        tracing::subscriber::with_default(subscriber, f)
    }

    #[test]
    fn valid_traceparent_continues_the_same_trace() {
        let tp = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
        let out = with_otel_subscriber(|| {
            let span = dispatch_span(Some(tp));
            span.in_scope(current_traceparent)
        });
        let out = out.expect("a sampled remote parent yields a re-injectable traceparent");
        // The child span keeps the inbound trace id (its own span id differs).
        assert!(
            out.contains("0af7651916cd43dd8448eb211c80319c"),
            "trace id must continue across the queue boundary: {out}"
        );
        assert!(
            !out.contains("b7ad6b7169203331"),
            "the child must mint its own span id, not reuse the parent's: {out}"
        );
    }

    #[test]
    fn malformed_traceparent_does_not_fail_open() {
        // An all-zero trace id is invalid per W3C; a fail-open parser would accept and
        // propagate it. The standard propagator must reject it and start a fresh trace.
        let bogus = "00-00000000000000000000000000000000-b7ad6b7169203331-01";
        let out = with_otel_subscriber(|| {
            let span = dispatch_span(Some(bogus));
            span.in_scope(current_traceparent)
        });
        // Either no context, or a freshly-minted (non-zero) trace id — never the
        // invalid all-zero one presented on the wire.
        if let Some(out) = out {
            assert!(
                !out.contains("00000000000000000000000000000000"),
                "an invalid all-zero trace id must not be accepted (fail-open): {out}"
            );
        }
    }

    #[test]
    fn garbage_traceparent_does_not_panic_and_roots_fresh() {
        let out = with_otel_subscriber(|| {
            let span = dispatch_span(Some("this-is-not-a-traceparent"));
            span.in_scope(current_traceparent)
        });
        // Unparseable input is dropped; the worker still roots a valid fresh trace.
        assert!(
            out.is_some(),
            "a fresh root trace is still created: {out:?}"
        );
    }

    #[test]
    fn no_traceparent_starts_a_fresh_root_trace() {
        let out = with_otel_subscriber(|| {
            let span = dispatch_span(None);
            span.in_scope(current_traceparent)
        });
        assert!(out.is_some(), "no inbound context still roots one trace");
    }
}
