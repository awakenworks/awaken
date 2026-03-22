//! OpenTelemetry export backend for observability metrics.
//!
//! Implements [`MetricsSink`] by mapping [`GenAISpan`] and [`ToolSpan`] to
//! OpenTelemetry spans using GenAI semantic conventions.
//!
//! Feature-gated behind `otel`.

use opentelemetry::trace::{SpanKind, Status, Tracer};
use opentelemetry::{KeyValue, trace::TraceContextExt};
use opentelemetry_sdk::trace::SdkTracer;

use crate::metrics::{AgentMetrics, GenAISpan, ToolSpan};
use crate::sink::MetricsSink;

/// OpenTelemetry-based metrics sink.
///
/// Records each inference and tool span as an OTel span using the
/// GenAI semantic conventions. Requires an OTel tracer provider to be
/// configured before use.
pub struct OtelMetricsSink {
    tracer: SdkTracer,
}

impl OtelMetricsSink {
    /// Create a new OTel sink with the given SDK tracer.
    pub fn new(tracer: SdkTracer) -> Self {
        Self { tracer }
    }

    /// Build OTel attributes from a GenAI inference span.
    fn genai_attributes(span: &GenAISpan) -> Vec<KeyValue> {
        let mut attrs = vec![
            KeyValue::new("gen_ai.system", span.provider.clone()),
            KeyValue::new("gen_ai.request.model", span.model.clone()),
            KeyValue::new("gen_ai.operation.name", span.operation.clone()),
        ];

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

        if let Some(ref error_type) = span.error_type {
            attrs.push(KeyValue::new("error.type", error_type.clone()));
        }

        attrs
    }
}

impl MetricsSink for OtelMetricsSink {
    fn on_inference(&self, span: &GenAISpan) {
        let attrs = Self::genai_attributes(span);
        let span_name = format!("{} {}", span.operation, span.model);

        let otel_span = self
            .tracer
            .span_builder(span_name)
            .with_kind(SpanKind::Client)
            .with_attributes(attrs)
            .start(&self.tracer);

        let cx = opentelemetry::Context::current_with_span(otel_span);
        if span.error_type.is_some() {
            cx.span()
                .set_status(Status::error(span.error_type.clone().unwrap_or_default()));
        }
        cx.span().end();
    }

    fn on_tool(&self, span: &ToolSpan) {
        let attrs = Self::tool_attributes(span);
        let span_name = format!("execute_tool {}", span.name);

        let otel_span = self
            .tracer
            .span_builder(span_name)
            .with_kind(SpanKind::Internal)
            .with_attributes(attrs)
            .start(&self.tracer);

        let cx = opentelemetry::Context::current_with_span(otel_span);
        if span.error_type.is_some() {
            cx.span()
                .set_status(Status::error(span.error_type.clone().unwrap_or_default()));
        }
        cx.span().end();
    }

    fn on_run_end(&self, metrics: &AgentMetrics) {
        let attrs = vec![
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

        let otel_span = self
            .tracer
            .span_builder("agent_session")
            .with_kind(SpanKind::Server)
            .with_attributes(attrs)
            .start(&self.tracer);

        let cx = opentelemetry::Context::current_with_span(otel_span);
        cx.span().end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_genai_span() -> GenAISpan {
        GenAISpan {
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

        let attr_map: std::collections::HashMap<&str, &KeyValue> =
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

        let attr_map: std::collections::HashMap<&str, &KeyValue> =
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
        sink.on_inference(&sample_genai_span());
        sink.on_tool(&sample_tool_span());
        sink.on_run_end(&AgentMetrics {
            inferences: vec![sample_genai_span()],
            tools: vec![sample_tool_span()],
            session_duration_ms: 5000,
        });
    }
}
