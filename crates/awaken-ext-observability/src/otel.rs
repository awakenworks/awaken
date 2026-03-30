//! OpenTelemetry export backend for observability metrics.
//!
//! Implements [`MetricsSink`] by mapping [`GenAISpan`] and [`ToolSpan`] to
//! OpenTelemetry spans using GenAI semantic conventions.
//!
//! Feature-gated behind `otel`.

use std::sync::Mutex;

use opentelemetry::trace::{SpanKind, Status, Tracer};
use opentelemetry::{KeyValue, trace::TraceContextExt};
use opentelemetry_sdk::trace::SdkTracer;

use crate::metrics::{AgentMetrics, GenAISpan, MetricsEvent, ToolSpan};
use crate::otel_config::OtelConfig;
use crate::sink::MetricsSink;

/// OpenTelemetry-based metrics sink.
///
/// Records each inference and tool span as an OTel span using the
/// GenAI semantic conventions, arranged in a proper parent-child
/// hierarchy:
///
/// ```text
/// agent_session (root, SpanKind::Server)
///   ├─ chat gpt-4 (inference, SpanKind::Client)
///   │    ├─ execute_tool search (SpanKind::Internal)
///   │    └─ execute_tool read   (SpanKind::Internal)
///   └─ chat gpt-4 (inference, SpanKind::Client)
///        └─ execute_tool write  (SpanKind::Internal)
/// ```
///
/// The root session span is lazily created on first `record()` and
/// ended when `on_run_end()` is called.
pub struct OtelMetricsSink {
    tracer: SdkTracer,
    /// Root session span context — created lazily on first record().
    root_context: Mutex<Option<opentelemetry::Context>>,
    /// Current inference span context — tool spans become children of this.
    current_inference_cx: Mutex<Option<opentelemetry::Context>>,
}

impl OtelMetricsSink {
    /// Create a new OTel sink with the given SDK tracer.
    pub fn new(tracer: SdkTracer) -> Self {
        Self {
            tracer,
            root_context: Mutex::new(None),
            current_inference_cx: Mutex::new(None),
        }
    }

    /// Return the root session context, creating the root span lazily.
    fn ensure_root_context(&self) -> opentelemetry::Context {
        let mut root = self.root_context.lock().unwrap();
        if let Some(ref cx) = *root {
            return cx.clone();
        }
        let root_span = self
            .tracer
            .span_builder("agent_session")
            .with_kind(SpanKind::Server)
            .start(&self.tracer);
        let cx = opentelemetry::Context::new().with_span(root_span);
        *root = Some(cx.clone());
        cx
    }

    /// Append common SpanContext attributes to the given vec.
    fn push_context_attributes(attrs: &mut Vec<KeyValue>, ctx: &crate::metrics::SpanContext) {
        if !ctx.run_id.is_empty() {
            attrs.push(KeyValue::new("run.id", ctx.run_id.clone()));
        }
        if !ctx.thread_id.is_empty() {
            attrs.push(KeyValue::new("thread.id", ctx.thread_id.clone()));
        }
        if !ctx.agent_id.is_empty() {
            attrs.push(KeyValue::new("agent.id", ctx.agent_id.clone()));
        }
        if let Some(ref parent) = ctx.parent_run_id {
            attrs.push(KeyValue::new("parent_run.id", parent.clone()));
        }
    }

    /// Build OTel attributes from a GenAI inference span.
    fn genai_attributes(span: &GenAISpan) -> Vec<KeyValue> {
        let mut attrs = vec![
            KeyValue::new("gen_ai.system", span.provider.clone()),
            KeyValue::new("gen_ai.request.model", span.model.clone()),
            KeyValue::new("gen_ai.operation.name", span.operation.clone()),
        ];

        Self::push_context_attributes(&mut attrs, &span.context);
        if let Some(step) = span.step_index {
            attrs.push(KeyValue::new("step.index", step as i64));
        }

        if let Some(ref response_model) = span.response_model {
            attrs.push(KeyValue::new(
                "gen_ai.response.model",
                response_model.clone(),
            ));
        }
        if let Some(ref response_id) = span.response_id {
            attrs.push(KeyValue::new("gen_ai.response.id", response_id.clone()));
        }
        if !span.finish_reasons.is_empty() {
            attrs.push(KeyValue::new(
                "gen_ai.response.finish_reasons",
                format!("{:?}", span.finish_reasons),
            ));
        }

        // Token usage
        if let Some(input) = span.input_tokens {
            attrs.push(KeyValue::new("gen_ai.usage.input_tokens", i64::from(input)));
        }
        if let Some(output) = span.output_tokens {
            attrs.push(KeyValue::new(
                "gen_ai.usage.output_tokens",
                i64::from(output),
            ));
        }
        if let Some(cache_read) = span.cache_read_input_tokens {
            attrs.push(KeyValue::new(
                "gen_ai.usage.cache_read.input_tokens",
                i64::from(cache_read),
            ));
        }
        if let Some(cache_creation) = span.cache_creation_input_tokens {
            attrs.push(KeyValue::new(
                "gen_ai.usage.cache_creation.input_tokens",
                i64::from(cache_creation),
            ));
        }
        if let Some(thinking) = span.thinking_tokens {
            attrs.push(KeyValue::new(
                "gen_ai.usage.thinking_tokens",
                i64::from(thinking),
            ));
        }

        // Request parameters
        if let Some(temp) = span.temperature {
            attrs.push(KeyValue::new("gen_ai.request.temperature", temp));
        }
        if let Some(top_p) = span.top_p {
            attrs.push(KeyValue::new("gen_ai.request.top_p", top_p));
        }
        if let Some(max_tokens) = span.max_tokens {
            attrs.push(KeyValue::new(
                "gen_ai.request.max_tokens",
                i64::from(max_tokens),
            ));
        }

        // Duration
        attrs.push(KeyValue::new(
            "gen_ai.client.operation.duration",
            span.duration_ms as f64 / 1000.0,
        ));

        // Error
        if let Some(ref error_type) = span.error_type {
            attrs.push(KeyValue::new("error.type", error_type.clone()));
        }

        attrs
    }

    /// Build OTel attributes from a tool execution span.
    fn tool_attributes(span: &ToolSpan) -> Vec<KeyValue> {
        let mut attrs = vec![
            KeyValue::new("gen_ai.tool.name", span.name.clone()),
            KeyValue::new("gen_ai.operation.name", span.operation.clone()),
            KeyValue::new("gen_ai.tool.call.id", span.call_id.clone()),
            KeyValue::new("gen_ai.tool.type", span.tool_type.clone()),
        ];

        Self::push_context_attributes(&mut attrs, &span.context);
        if let Some(step) = span.step_index {
            attrs.push(KeyValue::new("step.index", step as i64));
        }

        if let Some(ref error_type) = span.error_type {
            attrs.push(KeyValue::new("error.type", error_type.clone()));
        }

        attrs
    }

    fn record_inference(&self, span: &GenAISpan) {
        let attrs = Self::genai_attributes(span);
        let span_name = format!("{} {}", span.operation, span.model);

        let end_time = std::time::SystemTime::now();
        let start_time = end_time - std::time::Duration::from_millis(span.duration_ms);

        let root_cx = self.ensure_root_context();

        let otel_span = self
            .tracer
            .span_builder(span_name)
            .with_kind(SpanKind::Client)
            .with_attributes(attrs)
            .with_start_time(start_time)
            .start_with_context(&self.tracer, &root_cx);

        let inference_cx = root_cx.with_span(otel_span);

        if span.error_type.is_some() {
            inference_cx
                .span()
                .set_status(Status::error(span.error_type.clone().unwrap_or_default()));
        }

        inference_cx.span().end_with_timestamp(end_time);

        // Store so tool spans become children of this inference.
        *self.current_inference_cx.lock().unwrap() = Some(inference_cx);
    }

    fn record_tool(&self, span: &ToolSpan) {
        let attrs = Self::tool_attributes(span);
        let span_name = format!("execute_tool {}", span.name);

        let end_time = std::time::SystemTime::now();
        let start_time = end_time - std::time::Duration::from_millis(span.duration_ms);

        // Prefer current inference as parent; fall back to root.
        let parent_cx = self
            .current_inference_cx
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| self.ensure_root_context());

        let otel_span = self
            .tracer
            .span_builder(span_name)
            .with_kind(SpanKind::Internal)
            .with_attributes(attrs)
            .with_start_time(start_time)
            .start_with_context(&self.tracer, &parent_cx);

        let cx = parent_cx.with_span(otel_span);

        if span.error_type.is_some() {
            cx.span()
                .set_status(Status::error(span.error_type.clone().unwrap_or_default()));
        }

        cx.span().end_with_timestamp(end_time);
    }
}

/// Initialise an OTLP HTTP tracer from the given configuration.
///
/// Returns an `SdkTracerProvider` (caller should keep it alive) and an
/// `SdkTracer` suitable for passing to [`OtelMetricsSink::new`].
///
/// # Errors
///
/// Returns an error when no endpoint is configured or the OTLP exporter
/// fails to build.
pub fn init_otlp_tracer(
    config: &OtelConfig,
) -> Result<
    (opentelemetry_sdk::trace::SdkTracerProvider, SdkTracer),
    Box<dyn std::error::Error + Send + Sync>,
> {
    use opentelemetry::trace::TracerProvider;
    use opentelemetry_otlp::{SpanExporter, WithExportConfig};
    use opentelemetry_sdk::Resource;

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

    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(Resource::builder().with_attributes(resource_attrs).build())
        .build();

    let tracer = provider.tracer("awaken");
    Ok((provider, tracer))
}

impl MetricsSink for OtelMetricsSink {
    fn record(&self, event: &MetricsEvent) {
        match event {
            MetricsEvent::Inference(span) => self.record_inference(span),
            MetricsEvent::Tool(span) => self.record_tool(span),
            _ => {} // suspension/handoff/delegation: no OTel mapping yet
        }
    }

    fn on_run_end(&self, metrics: &AgentMetrics) {
        let session_attrs = [
            KeyValue::new(
                "gen_ai.usage.input_tokens",
                i64::from(metrics.total_input_tokens()),
            ),
            KeyValue::new(
                "gen_ai.usage.output_tokens",
                i64::from(metrics.total_output_tokens()),
            ),
            KeyValue::new(
                "gen_ai.session.inference_count",
                metrics.inference_count() as i64,
            ),
            KeyValue::new("gen_ai.session.tool_count", metrics.tool_count() as i64),
            KeyValue::new(
                "gen_ai.session.tool_failures",
                metrics.tool_failures() as i64,
            ),
            KeyValue::new(
                "gen_ai.session.duration",
                metrics.session_duration_ms as f64 / 1000.0,
            ),
        ];

        // End the root session span (created lazily during record()).
        let mut root = self.root_context.lock().unwrap();
        if let Some(cx) = root.take() {
            let span_ref = cx.span();
            span_ref.set_attributes(session_attrs);
            span_ref.end();
        }

        // Clear inference context for potential reuse.
        *self.current_inference_cx.lock().unwrap() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{MetricsEvent, SpanContext};
    use std::collections::HashMap;

    fn sample_genai_span() -> GenAISpan {
        GenAISpan {
            context: SpanContext::default(),
            step_index: None,
            model: "gpt-4".to_string(),
            provider: "openai".to_string(),
            operation: "chat".to_string(),
            response_model: Some("gpt-4-0125".to_string()),
            response_id: Some("chatcmpl-123".to_string()),
            finish_reasons: vec!["stop".to_string()],
            error_type: None,
            error_class: None,
            thinking_tokens: None,
            input_tokens: Some(100),
            output_tokens: Some(50),
            total_tokens: Some(150),
            cache_read_input_tokens: Some(20),
            cache_creation_input_tokens: None,
            temperature: Some(0.7),
            top_p: Some(0.9),
            max_tokens: Some(4096),
            stop_sequences: Vec::new(),
            duration_ms: 1200,
        }
    }

    fn sample_tool_span() -> ToolSpan {
        ToolSpan {
            context: SpanContext::default(),
            step_index: None,
            name: "read_file".to_string(),
            operation: "execute_tool".to_string(),
            call_id: "call_abc123".to_string(),
            tool_type: "function".to_string(),
            error_type: None,
            duration_ms: 50,
        }
    }

    #[test]
    fn genai_attributes_complete() {
        let span = sample_genai_span();
        let attrs = OtelMetricsSink::genai_attributes(&span);

        let attr_map: HashMap<&str, &KeyValue> =
            attrs.iter().map(|kv| (kv.key.as_str(), kv)).collect();

        assert!(attr_map.contains_key("gen_ai.system"));
        assert!(attr_map.contains_key("gen_ai.request.model"));
        assert!(attr_map.contains_key("gen_ai.operation.name"));
        assert!(attr_map.contains_key("gen_ai.response.model"));
        assert!(attr_map.contains_key("gen_ai.response.id"));
        assert!(attr_map.contains_key("gen_ai.usage.input_tokens"));
        assert!(attr_map.contains_key("gen_ai.usage.output_tokens"));
        assert!(attr_map.contains_key("gen_ai.usage.cache_read.input_tokens"));
        assert!(attr_map.contains_key("gen_ai.request.temperature"));
        assert!(attr_map.contains_key("gen_ai.request.top_p"));
        assert!(attr_map.contains_key("gen_ai.request.max_tokens"));
        assert!(attr_map.contains_key("gen_ai.client.operation.duration"));
    }

    #[test]
    fn genai_attributes_minimal() {
        let span = GenAISpan {
            context: SpanContext::default(),
            step_index: None,
            model: "claude-3".to_string(),
            provider: "anthropic".to_string(),
            operation: "chat".to_string(),
            response_model: None,
            response_id: None,
            finish_reasons: Vec::new(),
            error_type: None,
            error_class: None,
            thinking_tokens: None,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop_sequences: Vec::new(),
            duration_ms: 100,
        };
        let attrs = OtelMetricsSink::genai_attributes(&span);

        // Should have at least the required attributes
        assert!(attrs.len() >= 4); // system, model, operation, duration
        assert!(
            !attrs
                .iter()
                .any(|kv| kv.key.as_str() == "gen_ai.response.model")
        );
    }

    #[test]
    fn genai_attributes_with_error() {
        let span = GenAISpan {
            error_type: Some("rate_limit".to_string()),
            ..sample_genai_span()
        };
        let attrs = OtelMetricsSink::genai_attributes(&span);
        assert!(attrs.iter().any(|kv| kv.key.as_str() == "error.type"));
    }

    #[test]
    fn tool_attributes_success() {
        let span = sample_tool_span();
        let attrs = OtelMetricsSink::tool_attributes(&span);

        let attr_map: HashMap<&str, &KeyValue> =
            attrs.iter().map(|kv| (kv.key.as_str(), kv)).collect();

        assert!(attr_map.contains_key("gen_ai.tool.name"));
        assert!(attr_map.contains_key("gen_ai.operation.name"));
        assert!(attr_map.contains_key("gen_ai.tool.call.id"));
        assert!(attr_map.contains_key("gen_ai.tool.type"));
        assert!(!attr_map.contains_key("error.type"));
    }

    #[test]
    fn tool_attributes_with_error() {
        let span = ToolSpan {
            error_type: Some("permission_denied".to_string()),
            ..sample_tool_span()
        };
        let attrs = OtelMetricsSink::tool_attributes(&span);
        assert!(attrs.iter().any(|kv| kv.key.as_str() == "error.type"));
    }

    #[test]
    fn otel_sink_with_noop_tracer() {
        use opentelemetry::trace::TracerProvider;
        use opentelemetry_sdk::trace::SdkTracerProvider;

        let provider = SdkTracerProvider::builder().build();
        let tracer = provider.tracer("test");
        let sink = OtelMetricsSink::new(tracer);

        // Should not panic with noop spans
        sink.record(&MetricsEvent::Inference(sample_genai_span()));
        sink.record(&MetricsEvent::Tool(sample_tool_span()));
        sink.on_run_end(&AgentMetrics {
            inferences: vec![sample_genai_span()],
            tools: vec![sample_tool_span()],
            session_duration_ms: 5000,
            ..Default::default()
        });
    }

    // ── In-memory span exporter for OTLP pipeline verification ────────

    /// A simple in-memory span exporter that captures exported spans for
    /// test assertions. Uses `Arc<Mutex<Vec<SpanData>>>` so the test can
    /// read back the spans after the provider flushes.
    mod capture {
        use futures_util::future::BoxFuture;
        use opentelemetry_sdk::error::OTelSdkResult;
        use opentelemetry_sdk::trace::{SpanData, SpanExporter};
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Debug)]
        pub struct InMemorySpanExporter {
            spans: Arc<Mutex<Vec<SpanData>>>,
        }

        impl InMemorySpanExporter {
            pub fn new() -> Self {
                Self {
                    spans: Arc::new(Mutex::new(Vec::new())),
                }
            }

            pub fn finished_spans(&self) -> Vec<SpanData> {
                self.spans.lock().unwrap().clone()
            }
        }

        impl SpanExporter for InMemorySpanExporter {
            fn export(&mut self, batch: Vec<SpanData>) -> BoxFuture<'static, OTelSdkResult> {
                self.spans.lock().unwrap().extend(batch);
                Box::pin(std::future::ready(Ok(())))
            }
        }
    }

    /// Build an OtelMetricsSink backed by our in-memory exporter so
    /// exported OTel spans can be inspected.
    fn make_capturing_sink() -> (
        OtelMetricsSink,
        capture::InMemorySpanExporter,
        opentelemetry_sdk::trace::SdkTracerProvider,
    ) {
        use opentelemetry::trace::TracerProvider;
        use opentelemetry_sdk::trace::SdkTracerProvider;

        let exporter = capture::InMemorySpanExporter::new();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("awaken-test");
        let sink = OtelMetricsSink::new(tracer);
        (sink, exporter, provider)
    }

    /// Helper: build a HashMap of attribute key -> Value from a SpanData.
    fn attr_map(
        span: &opentelemetry_sdk::trace::SpanData,
    ) -> HashMap<String, opentelemetry::Value> {
        span.attributes
            .iter()
            .map(|kv| (kv.key.to_string(), kv.value.clone()))
            .collect()
    }

    // ── OTLP pipeline span verification tests ────────────────────────

    #[test]
    fn otlp_genai_span_has_all_required_attributes() {
        let (sink, exporter, provider) = make_capturing_sink();

        let span = GenAISpan {
            context: SpanContext {
                run_id: "run-42".to_string(),
                thread_id: "thread-7".to_string(),
                agent_id: "agent-alpha".to_string(),
                parent_run_id: None,
            },
            step_index: Some(3),
            duration_ms: 1200,
            ..sample_genai_span()
        };

        sink.record(&MetricsEvent::Inference(span));

        // on_run_end ends the root session span.
        sink.on_run_end(&AgentMetrics {
            inferences: vec![sample_genai_span()],
            ..Default::default()
        });

        // Force the provider to flush so SimpleSpanProcessor exports.
        drop(sink);
        let _ = provider.shutdown();

        let spans = exporter.finished_spans();
        // 1 inference + 1 session root = 2
        assert_eq!(spans.len(), 2, "expected 2 exported spans");

        let exported = spans
            .iter()
            .find(|s| s.name.as_ref() == "chat gpt-4")
            .expect("inference span not found");

        // SpanKind
        assert_eq!(exported.span_kind, opentelemetry::trace::SpanKind::Client);

        // The inference span has a parent (the auto-created root session).
        assert!(
            exported.parent_span_id != opentelemetry::trace::SpanId::INVALID,
            "inference span should have a parent (the root session span)"
        );

        // Verify duration is approximately correct (>= 1s).
        let duration = exported
            .end_time
            .duration_since(exported.start_time)
            .expect("end > start");
        assert!(
            duration >= std::time::Duration::from_millis(1000),
            "span duration should be >= 1s, got {duration:?}"
        );

        // Attributes
        let attrs = attr_map(exported);
        assert_eq!(
            attrs.get("gen_ai.system").map(|v| v.to_string()),
            Some("openai".to_string())
        );
        assert_eq!(
            attrs.get("gen_ai.request.model").map(|v| v.to_string()),
            Some("gpt-4".to_string())
        );
        assert_eq!(
            attrs
                .get("gen_ai.usage.input_tokens")
                .map(|v| v.to_string()),
            Some("100".to_string())
        );
        assert_eq!(
            attrs
                .get("gen_ai.usage.output_tokens")
                .map(|v| v.to_string()),
            Some("50".to_string())
        );
        assert_eq!(
            attrs.get("run.id").map(|v| v.to_string()),
            Some("run-42".to_string())
        );
        assert_eq!(
            attrs.get("thread.id").map(|v| v.to_string()),
            Some("thread-7".to_string())
        );
        assert_eq!(
            attrs.get("agent.id").map(|v| v.to_string()),
            Some("agent-alpha".to_string())
        );
        assert_eq!(
            attrs.get("step.index").map(|v| v.to_string()),
            Some("3".to_string())
        );
        // Duration present
        assert!(attrs.contains_key("gen_ai.client.operation.duration"));
    }

    #[test]
    fn otlp_tool_span_has_all_required_attributes() {
        let (sink, exporter, provider) = make_capturing_sink();

        // Record an inference first so the tool becomes its child.
        sink.record(&MetricsEvent::Inference(sample_genai_span()));

        let span = ToolSpan {
            context: SpanContext {
                run_id: "run-42".to_string(),
                thread_id: "thread-7".to_string(),
                agent_id: "agent-alpha".to_string(),
                parent_run_id: None,
            },
            step_index: Some(1),
            ..sample_tool_span()
        };

        sink.record(&MetricsEvent::Tool(span));

        sink.on_run_end(&AgentMetrics::default());
        drop(sink);
        let _ = provider.shutdown();

        let spans = exporter.finished_spans();
        // 1 inference + 1 tool + 1 session = 3
        assert_eq!(spans.len(), 3, "expected 3 exported spans");

        let exported = spans
            .iter()
            .find(|s| s.name.as_ref() == "execute_tool read_file")
            .expect("tool span not found");

        // SpanKind
        assert_eq!(exported.span_kind, opentelemetry::trace::SpanKind::Internal);

        // Tool span has a parent (the inference span).
        assert!(
            exported.parent_span_id != opentelemetry::trace::SpanId::INVALID,
            "tool span should have a parent"
        );

        // Attributes
        let attrs = attr_map(exported);
        assert_eq!(
            attrs.get("gen_ai.tool.call.id").map(|v| v.to_string()),
            Some("call_abc123".to_string())
        );
        assert_eq!(
            attrs.get("gen_ai.tool.name").map(|v| v.to_string()),
            Some("read_file".to_string())
        );
        assert_eq!(
            attrs.get("run.id").map(|v| v.to_string()),
            Some("run-42".to_string())
        );
        assert_eq!(
            attrs.get("thread.id").map(|v| v.to_string()),
            Some("thread-7".to_string())
        );
        assert_eq!(
            attrs.get("agent.id").map(|v| v.to_string()),
            Some("agent-alpha".to_string())
        );
        assert_eq!(
            attrs.get("step.index").map(|v| v.to_string()),
            Some("1".to_string())
        );
    }

    #[test]
    fn otlp_run_end_creates_session_span() {
        let (sink, exporter, provider) = make_capturing_sink();

        // Record some events first.
        sink.record(&MetricsEvent::Inference(sample_genai_span()));
        sink.record(&MetricsEvent::Tool(sample_tool_span()));

        // Now fire on_run_end with aggregate metrics.
        let metrics = AgentMetrics {
            inferences: vec![sample_genai_span()],
            tools: vec![sample_tool_span()],
            session_duration_ms: 8000,
            ..Default::default()
        };
        sink.on_run_end(&metrics);

        drop(sink);
        let _ = provider.shutdown();

        let spans = exporter.finished_spans();
        // 1 inference + 1 tool + 1 session = 3 spans
        assert_eq!(spans.len(), 3, "expected 3 exported spans");

        // Find the session span.
        let session = spans
            .iter()
            .find(|s| s.name.as_ref() == "agent_session")
            .expect("session span not found");

        assert_eq!(session.span_kind, opentelemetry::trace::SpanKind::Server);

        // Session span is the root — no parent.
        assert_eq!(
            session.parent_span_id,
            opentelemetry::trace::SpanId::INVALID,
            "session span should be the root (no parent)"
        );

        // All other spans share the same trace_id as the session.
        let trace_id = session.span_context.trace_id();
        for s in &spans {
            assert_eq!(
                s.span_context.trace_id(),
                trace_id,
                "span '{}' should share trace_id with session",
                s.name
            );
        }

        // Inference and tool spans should be children (have a parent).
        let inference = spans
            .iter()
            .find(|s| s.name.starts_with("chat"))
            .expect("inference span not found");
        assert_eq!(
            inference.parent_span_id,
            session.span_context.span_id(),
            "inference span should be a child of the session"
        );

        let tool = spans
            .iter()
            .find(|s| s.name.starts_with("execute_tool"))
            .expect("tool span not found");
        assert_eq!(
            tool.parent_span_id,
            inference.span_context.span_id(),
            "tool span should be a child of the inference"
        );

        let attrs = attr_map(session);
        assert_eq!(
            attrs
                .get("gen_ai.usage.input_tokens")
                .map(|v| v.to_string()),
            Some("100".to_string())
        );
        assert_eq!(
            attrs
                .get("gen_ai.usage.output_tokens")
                .map(|v| v.to_string()),
            Some("50".to_string())
        );
        assert_eq!(
            attrs
                .get("gen_ai.session.inference_count")
                .map(|v| v.to_string()),
            Some("1".to_string())
        );
        assert_eq!(
            attrs
                .get("gen_ai.session.tool_count")
                .map(|v| v.to_string()),
            Some("1".to_string())
        );
        assert_eq!(
            attrs
                .get("gen_ai.session.tool_failures")
                .map(|v| v.to_string()),
            Some("0".to_string())
        );
        assert!(attrs.contains_key("gen_ai.session.duration"));
    }

    #[test]
    fn otlp_multi_step_creates_correlated_spans() {
        let (sink, exporter, provider) = make_capturing_sink();

        let ctx = SpanContext {
            run_id: "run-99".to_string(),
            thread_id: "thread-1".to_string(),
            agent_id: "agent-beta".to_string(),
            parent_run_id: None,
        };

        // Step 0: inference + 2 tools
        sink.record(&MetricsEvent::Inference(GenAISpan {
            context: ctx.clone(),
            step_index: Some(0),
            model: "gpt-4".to_string(),
            ..sample_genai_span()
        }));
        sink.record(&MetricsEvent::Tool(ToolSpan {
            context: ctx.clone(),
            step_index: Some(0),
            name: "search".to_string(),
            call_id: "call_1".to_string(),
            ..sample_tool_span()
        }));
        sink.record(&MetricsEvent::Tool(ToolSpan {
            context: ctx.clone(),
            step_index: Some(0),
            name: "read".to_string(),
            call_id: "call_2".to_string(),
            ..sample_tool_span()
        }));

        // Step 1: inference + 1 tool
        sink.record(&MetricsEvent::Inference(GenAISpan {
            context: ctx.clone(),
            step_index: Some(1),
            model: "gpt-4".to_string(),
            ..sample_genai_span()
        }));
        sink.record(&MetricsEvent::Tool(ToolSpan {
            context: ctx.clone(),
            step_index: Some(1),
            name: "write".to_string(),
            call_id: "call_3".to_string(),
            ..sample_tool_span()
        }));

        sink.on_run_end(&AgentMetrics {
            inferences: vec![sample_genai_span(), sample_genai_span()],
            tools: vec![sample_tool_span(), sample_tool_span(), sample_tool_span()],
            session_duration_ms: 5000,
            ..Default::default()
        });

        drop(sink);
        let _ = provider.shutdown();

        let spans = exporter.finished_spans();
        // 2 inferences + 3 tools + 1 session = 6
        assert_eq!(
            spans.len(),
            6,
            "expected 6 exported spans (2 inferences + 3 tools + 1 session)"
        );

        // All spans share the same trace_id.
        let trace_id = spans[0].span_context.trace_id();
        for s in &spans {
            assert_eq!(
                s.span_context.trace_id(),
                trace_id,
                "span '{}' should share trace_id",
                s.name
            );
        }

        // Session span is root (no parent).
        let session = spans
            .iter()
            .find(|s| s.name.as_ref() == "agent_session")
            .expect("session span not found");
        assert_eq!(
            session.parent_span_id,
            opentelemetry::trace::SpanId::INVALID
        );

        // Both inference spans are children of the session.
        let inferences: Vec<_> = spans
            .iter()
            .filter(|s| s.name.starts_with("chat"))
            .collect();
        assert_eq!(inferences.len(), 2);
        for inf in &inferences {
            assert_eq!(
                inf.parent_span_id,
                session.span_context.span_id(),
                "inference span should be child of session"
            );
        }

        // Step 0 tools are children of the step-0 inference.
        let step0_inference = inferences
            .iter()
            .find(|s| attr_map(s).get("step.index").map(|v| v.to_string()) == Some("0".to_string()))
            .expect("step 0 inference not found");
        let step0_tools: Vec<_> = spans
            .iter()
            .filter(|s| {
                let a = attr_map(s);
                s.name.starts_with("execute_tool")
                    && a.get("step.index").map(|v| v.to_string()) == Some("0".to_string())
            })
            .collect();
        assert_eq!(step0_tools.len(), 2, "expected 2 tools at step 0");
        for tool in &step0_tools {
            assert_eq!(
                tool.parent_span_id,
                step0_inference.span_context.span_id(),
                "step-0 tool should be child of step-0 inference"
            );
        }

        // Step 1 tool is child of step-1 inference.
        let step1_inference = inferences
            .iter()
            .find(|s| attr_map(s).get("step.index").map(|v| v.to_string()) == Some("1".to_string()))
            .expect("step 1 inference not found");
        let step1_tools: Vec<_> = spans
            .iter()
            .filter(|s| {
                let a = attr_map(s);
                s.name.starts_with("execute_tool")
                    && a.get("step.index").map(|v| v.to_string()) == Some("1".to_string())
            })
            .collect();
        assert_eq!(step1_tools.len(), 1, "expected 1 tool at step 1");
        assert_eq!(
            step1_tools[0].parent_span_id,
            step1_inference.span_context.span_id(),
            "step-1 tool should be child of step-1 inference"
        );

        // All spans share the same run.id.
        for s in &spans {
            if s.name.as_ref() == "agent_session" {
                continue; // session span doesn't have run.id attribute
            }
            let attrs = attr_map(s);
            assert_eq!(
                attrs.get("run.id").map(|v| v.to_string()),
                Some("run-99".to_string()),
                "span '{}' missing run.id",
                s.name
            );
        }
    }
}
