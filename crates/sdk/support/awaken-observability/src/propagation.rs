//! W3C `traceparent` capture/relay across a durable queue boundary.
//!
//! A run admitted in one task (under the ingress span) is executed later by a
//! dispatch worker — possibly in another process — so the `tracing` span context
//! does not carry across. To keep it one trace, the admitting side captures the
//! current context as a `traceparent` string ([`current_traceparent`]) and
//! persists it on the durable instruction. A worker rebuilds a `wake.dispatch`
//! span whose remote parent is that context ([`dispatch_span`]) and runs local
//! execution plus post-commit observation inside it. A registered Worker's
//! Coordinator instead rebuilds a separately named settlement-observer span from
//! the same persisted carrier. In both topologies, `runtime.run` and detached
//! auxiliary work remain under the admitting request's trace.
//!
//! Both use the globally-installed `TraceContextPropagator`, so the wire form is
//! exactly the standard `00-<32hex>-<16hex>-<2hex>` header.

use std::collections::HashMap;

use opentelemetry::propagation::Extractor;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Install the remote context extracted from one canonical carrier on `span`.
///
/// Ingress headers and persisted durable instructions are different carriers,
/// but parent installation must have one owner. Callers supply the carrier while
/// this function alone owns propagator extraction and best-effort span adoption.
pub(crate) fn set_remote_parent_from_extractor(span: &tracing::Span, extractor: &dyn Extractor) {
    let cx =
        opentelemetry::global::get_text_map_propagator(|propagator| propagator.extract(extractor));
    // A process without an installed OpenTelemetry layer legitimately has no
    // subscriber extension to receive the parent.
    let _ = span.set_parent(cx);
}

/// Apply a persisted W3C `traceparent` as the remote parent of an existing span.
///
/// This is the single durable-carrier adoption primitive. It lets each bounded
/// consumer retain an accurate span name and kind: [`dispatch_span`] names the
/// queue consumer `wake.dispatch`, while a downstream settlement observer can
/// name its own internal continuation without manufacturing a second queue wake.
/// With no carrier, the span keeps the parent it inherited when it was created.
#[must_use]
pub fn span_with_remote_parent(span: tracing::Span, traceparent: Option<&str>) -> tracing::Span {
    if let Some(traceparent) = traceparent {
        let mut carrier = HashMap::new();
        carrier.insert("traceparent".to_string(), traceparent.to_string());
        set_remote_parent_from_extractor(&span, &carrier);
    }
    span
}

/// Capture the current span's trace context as a W3C `traceparent` string, to be
/// persisted on a durable instruction. Returns `None` when there is no valid
/// context to propagate (e.g. tracing disabled or an unsampled root).
pub fn current_traceparent() -> Option<String> {
    let cx = tracing::Span::current().context();
    let mut carrier = HashMap::new();
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&cx, &mut carrier);
    });
    carrier.remove("traceparent")
}

/// Build the `wake.dispatch` span for a durably-dispatched run. When `traceparent`
/// is present it becomes the span's remote parent, so the execution driven inside
/// this span (`runtime.run` → …) continues the admitting request's trace across
/// the queue boundary. `SpanKind::Consumer` marks the queue-consuming side.
pub fn dispatch_span(traceparent: Option<&str>) -> tracing::Span {
    span_with_remote_parent(
        tracing::info_span!(parent: None, "wake.dispatch", otel.kind = "consumer"),
        traceparent,
    )
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Clone, Debug)]
    pub(crate) struct CapturedSpan {
        pub(crate) name: String,
        pub(crate) trace_id: String,
        pub(crate) span_id: String,
        pub(crate) parent_span_id: Option<String>,
        pub(crate) attributes: HashMap<String, String>,
    }

    #[derive(Clone, Debug)]
    struct CapturingExporter(Arc<Mutex<Vec<CapturedSpan>>>);

    impl SpanExporter for CapturingExporter {
        fn export(
            &self,
            batch: Vec<SpanData>,
        ) -> impl std::future::Future<Output = opentelemetry_sdk::error::OTelSdkResult> + Send
        {
            use opentelemetry::trace::SpanId;
            let sink = self.0.clone();
            let mut out = sink.lock().unwrap();
            for span in &batch {
                let parent = span.parent_span_id;
                out.push(CapturedSpan {
                    name: span.name.to_string(),
                    trace_id: format!(
                        "{:032x}",
                        u128::from_be_bytes(span.span_context.trace_id().to_bytes())
                    ),
                    span_id: format!(
                        "{:016x}",
                        u64::from_be_bytes(span.span_context.span_id().to_bytes())
                    ),
                    parent_span_id: (parent != SpanId::INVALID)
                        .then(|| format!("{:016x}", u64::from_be_bytes(parent.to_bytes()))),
                    attributes: span
                        .attributes
                        .iter()
                        .map(|kv| (kv.key.to_string(), kv.value.to_string()))
                        .collect(),
                });
            }
            async { Ok(()) }
        }
    }

    /// Run one synchronous oracle under the crate's sole collector-free OTel
    /// harness. Both propagation and HTTP middleware tests consume this helper,
    /// so exporter behavior cannot drift between the two carrier boundaries.
    pub(crate) fn capture_spans<T>(
        name: &'static str,
        f: impl FnOnce() -> T,
    ) -> (T, Vec<CapturedSpan>) {
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
        let captured = Arc::new(Mutex::new(Vec::new()));
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(CapturingExporter(captured.clone()))
            .build();
        let tracer = provider.tracer(name);
        let subscriber =
            Registry::default().with(tracing_opentelemetry::layer().with_tracer(tracer));
        let value = tracing::subscriber::with_default(subscriber, f);
        provider.force_flush().expect("flush captured spans");
        let spans = captured.lock().unwrap().clone();
        (value, spans)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::capture_spans;
    use super::*;

    fn with_otel_subscriber<T>(f: impl FnOnce() -> T) -> T {
        capture_spans("awaken-observability-test", f).0
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

    #[test]
    fn explicit_durable_parent_overrides_an_unrelated_ambient_trace() {
        // Cause/effect graph: C1 a settlement request has one ambient trace; C2
        // the guarded RunDispatch carries a different persisted traceparent; C3
        // the observer continuation adopts C2. E1 its trace id follows C2, not
        // C1; E2 it mints a child span id. K: the durable instruction is the sole
        // retry-stable provenance; ambient transport context is diagnostic only.
        // Decision D1: C1+C2+C3 => E1+E2.
        let ambient = "00-11111111111111111111111111111111-1111111111111111-01";
        let durable = "00-22222222222222222222222222222222-2222222222222222-01";
        let (_, spans) = capture_spans("awaken-observability-parent-priority", || {
            let ambient = span_with_remote_parent(
                tracing::info_span!(parent: None, "settle.request.ambient"),
                Some(ambient),
            );
            ambient.in_scope(|| {
                let settlement = span_with_remote_parent(
                    tracing::info_span!(
                        parent: None,
                        "dispatch.settlement.observe",
                        otel.kind = "internal"
                    ),
                    Some(durable),
                );
                settlement.in_scope(|| {});
            });
        });
        let settlement = spans
            .iter()
            .find(|span| span.name == "dispatch.settlement.observe")
            .expect("D1 settlement continuation");
        assert_eq!(
            settlement.trace_id, "22222222222222222222222222222222",
            "D1/E1"
        );
        assert_eq!(
            settlement.parent_span_id.as_deref(),
            Some("2222222222222222"),
            "D1/E1"
        );
        assert_ne!(settlement.span_id, "2222222222222222", "D1/E2");
        assert!(
            spans.iter().all(|span| span.name != "wake.dispatch"),
            "D1 does not manufacture a second queue-consumer span: {spans:?}"
        );
    }

    #[test]
    fn absent_durable_parent_does_not_adopt_an_unrelated_ambient_trace() {
        // Cause/effect rule D2: C1 an ambient settle request exists; C2 the
        // guarded dispatch has no traceparent; C3 the settlement boundary roots
        // its explicit span. E1 the settlement does not claim C1 as Run
        // provenance. K: absence never authorizes an ambient transport fallback.
        let ambient = "00-11111111111111111111111111111111-1111111111111111-01";
        let (_, spans) = capture_spans("awaken-observability-missing-parent", || {
            let ambient = span_with_remote_parent(
                tracing::info_span!(parent: None, "settle.request.ambient"),
                Some(ambient),
            );
            ambient.in_scope(|| {
                let settlement = span_with_remote_parent(
                    tracing::info_span!(parent: None, "dispatch.settlement.observe"),
                    None,
                );
                settlement.in_scope(|| {});
            });
        });
        let settlement = spans
            .iter()
            .find(|span| span.name == "dispatch.settlement.observe")
            .expect("D2 settlement continuation");
        assert_ne!(
            settlement.trace_id, "11111111111111111111111111111111",
            "D2/E1"
        );
        assert_eq!(settlement.parent_span_id, None, "D2/E1");
    }

    // --- S14: trace-as-oracle. The tests above prove trace-id continuity by
    // re-injecting a traceparent. This one is stronger: it CAPTURES the exported span
    // tree and asserts the real parent→child topology (span-id linkage), the way a
    // cluster e2e would read OTLP output — but in-process and deterministic. It is the
    // oracle that "the worker's dispatch span nests the execution under the admitting
    // request's trace", not merely "the trace id string matches".

    #[test]
    fn the_exported_span_tree_nests_execution_under_the_dispatch_span() {
        // The admitting request's traceparent (as persisted on the durable instruction).
        let admit_trace = "0af7651916cd43dd8448eb211c80319c";
        let tp = format!("00-{admit_trace}-b7ad6b7169203331-01");

        let (_, spans) = capture_spans("awaken-observability-trace-oracle", || {
            // The worker rebuilds the dispatch span from the persisted traceparent, then
            // drives the run (its inference) INSIDE that span — the exact nesting
            // `drive_claimed` performs via `.instrument(dispatch)`.
            let dispatch = dispatch_span(Some(&tp));
            dispatch.in_scope(|| {
                let run = tracing::info_span!("chat", model = "stub");
                run.in_scope(|| {});
            });
        });
        let dispatch = spans
            .iter()
            .find(|s| s.name == "wake.dispatch")
            .expect("the wake.dispatch span was exported");
        let chat = spans
            .iter()
            .find(|s| s.name == "chat")
            .expect("the execution (chat) span was exported");

        // Oracle 1: both spans belong to the ADMITTING request's trace (continuity).
        assert_eq!(
            dispatch.trace_id, admit_trace,
            "the dispatch span continues the admitting trace: {dispatch:?}"
        );
        assert_eq!(
            chat.trace_id, admit_trace,
            "the execution span is in the same trace: {chat:?}"
        );
        // Oracle 2: real TREE nesting — the execution's parent IS the dispatch span,
        // by span-id linkage (not merely a shared trace id). This is what a cluster
        // e2e reads from OTLP; asserted here in-process.
        assert_eq!(
            chat.parent_span_id.as_deref(),
            Some(dispatch.span_id.as_str()),
            "the execution span nests directly under wake.dispatch: {chat:?} / {dispatch:?}"
        );
        // And the dispatch span mints its own id (a real remote-parent child), never
        // reusing the wire parent's span id.
        assert_ne!(
            dispatch.span_id, "b7ad6b7169203331",
            "the dispatch span mints its own id, not the wire parent's"
        );
    }
}
