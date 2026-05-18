use super::*;
use crate::metrics::{MetricsEvent, SpanContext};
use serde_json::json;
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
        started_at_ms: 0,
        ended_at_ms: 0,
        response_content: None,
        response_tool_calls: None,
        request_messages: None,
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
        call_arguments: None,
        call_result: None,
        error_type: None,
        duration_ms: 50,
        started_at_ms: 0,
        ended_at_ms: 0,
    }
}

fn sample_background_task_span(
    status: awaken_runtime::extensions::background::TaskStatus,
) -> BackgroundTaskSpan {
    use awaken_runtime::extensions::background::TaskStatus;
    let completed_at_ms = if matches!(status, TaskStatus::Running) {
        None
    } else {
        Some(1_500)
    };
    BackgroundTaskSpan {
        context: SpanContext::default(),
        task_id: "bg_1".to_string(),
        task_type: "sub_agent".to_string(),
        task_name: Some("worker".to_string()),
        description: "background worker".to_string(),
        status,
        parent_task_id: None,
        error_message: None,
        created_at_ms: 1_000,
        completed_at_ms,
    }
}

#[test]
fn genai_attributes_complete() {
    let span = sample_genai_span();
    let attrs = OtelMetricsSink::genai_attributes(&span);

    let attr_map: HashMap<&str, &KeyValue> = attrs.iter().map(|kv| (kv.key.as_str(), kv)).collect();

    assert!(attr_map.contains_key("gen_ai.provider.name"));
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
        started_at_ms: 0,
        ended_at_ms: 0,
        response_content: None,
        response_tool_calls: None,
        request_messages: None,
    };
    let attrs = OtelMetricsSink::genai_attributes(&span);

    // Should have the required GenAI span attributes available to Awaken.
    assert!(attrs.len() >= 3); // provider, model, operation
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

    let attr_map: HashMap<&str, &KeyValue> = attrs.iter().map(|kv| (kv.key.as_str(), kv)).collect();

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
fn tool_attributes_include_opt_in_payloads_as_json() {
    let span = ToolSpan {
        call_arguments: Some(json!({"query": "otel", "limit": 3})),
        call_result: Some(json!({"count": 1, "source": "docs"})),
        ..sample_tool_span()
    };
    let attrs = OtelMetricsSink::tool_attributes(&span);

    let arguments =
        kv_string(&attrs, "gen_ai.tool.call.arguments").expect("tool arguments attribute");
    let result = kv_string(&attrs, "gen_ai.tool.call.result").expect("tool result attribute");

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(arguments).expect("valid arguments json"),
        json!({"query": "otel", "limit": 3})
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(result).expect("valid result json"),
        json!({"count": 1, "source": "docs"})
    );
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
fn attr_map(span: &opentelemetry_sdk::trace::SpanData) -> HashMap<String, opentelemetry::Value> {
    span.attributes
        .iter()
        .map(|kv| (kv.key.to_string(), kv.value.clone()))
        .collect()
}

fn kv_string<'a>(attrs: &'a [KeyValue], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .and_then(|kv| match &kv.value {
            opentelemetry::Value::String(value) => Some(value.as_str()),
            _ => None,
        })
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
            parent_tool_call_id: None,
            ..Default::default()
        },
        step_index: Some(3),
        duration_ms: 1200,
        started_at_ms: 0,
        ended_at_ms: 0,
        ..sample_genai_span()
    };

    sink.record(&MetricsEvent::Inference(span));

    // on_run_end ends the root agent span.
    sink.on_run_end(&AgentMetrics {
        inferences: vec![sample_genai_span()],
        ..Default::default()
    });

    // Force the provider to flush so SimpleSpanProcessor exports.
    drop(sink);
    let _ = provider.shutdown();

    let spans = exporter.finished_spans();
    // 1 inference + 1 agent root = 2
    assert_eq!(spans.len(), 2, "expected 2 exported spans");

    let exported = spans
        .iter()
        .find(|s| s.name.as_ref() == "chat gpt-4")
        .expect("inference span not found");

    // SpanKind
    assert_eq!(exported.span_kind, opentelemetry::trace::SpanKind::Client);

    // The inference span has a parent (the auto-created root agent).
    assert!(
        exported.parent_span_id != opentelemetry::trace::SpanId::INVALID,
        "inference span should have a parent (the root agent span)"
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
        attrs.get("gen_ai.provider.name").map(|v| v.to_string()),
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
        attrs.get("awaken.run.id").map(|v| v.to_string()),
        Some("run-42".to_string())
    );
    assert_eq!(
        attrs.get("awaken.thread.id").map(|v| v.to_string()),
        Some("thread-7".to_string())
    );
    assert_eq!(
        attrs.get("awaken.agent.id").map(|v| v.to_string()),
        Some("agent-alpha".to_string())
    );
    assert_eq!(
        attrs.get("awaken.step.index").map(|v| v.to_string()),
        Some("3".to_string())
    );
}

#[test]
fn otlp_tool_span_has_all_required_attributes() {
    let (sink, exporter, provider) = make_capturing_sink();

    let context = SpanContext {
        run_id: "run-42".to_string(),
        thread_id: "thread-7".to_string(),
        agent_id: "agent-alpha".to_string(),
        parent_run_id: None,
        parent_tool_call_id: None,
        ..Default::default()
    };

    // Record an inference first so the tool becomes its child.
    sink.record(&MetricsEvent::Inference(GenAISpan {
        context: context.clone(),
        ..sample_genai_span()
    }));

    let span = ToolSpan {
        context,
        step_index: Some(1),
        ..sample_tool_span()
    };

    sink.record(&MetricsEvent::Tool(span));

    sink.on_run_end(&AgentMetrics::default());
    drop(sink);
    let _ = provider.shutdown();

    let spans = exporter.finished_spans();
    // 1 inference + 1 tool + 1 agent = 3
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
    let inference = spans
        .iter()
        .find(|s| s.name.as_ref() == "chat gpt-4")
        .expect("inference span not found");
    assert_eq!(
        exported.parent_span_id,
        inference.span_context.span_id(),
        "tool span should be parented to the active inference for the same run"
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
        attrs.get("awaken.run.id").map(|v| v.to_string()),
        Some("run-42".to_string())
    );
    assert_eq!(
        attrs.get("awaken.thread.id").map(|v| v.to_string()),
        Some("thread-7".to_string())
    );
    assert_eq!(
        attrs.get("awaken.agent.id").map(|v| v.to_string()),
        Some("agent-alpha".to_string())
    );
    assert_eq!(
        attrs.get("awaken.step.index").map(|v| v.to_string()),
        Some("1".to_string())
    );
}

#[test]
fn otlp_delegation_span_references_agent_tool_and_child_run() {
    let (sink, exporter, provider) = make_capturing_sink();

    let context = SpanContext {
        run_id: "run-delegate".to_string(),
        thread_id: "thread-delegate".to_string(),
        agent_id: "agent-orchestrator".to_string(),
        parent_run_id: None,
        parent_tool_call_id: None,
        ..Default::default()
    };
    let inference = GenAISpan {
        context: context.clone(),
        ..sample_genai_span()
    };
    let tool = ToolSpan {
        context: context.clone(),
        name: "agent_run_worker".to_string(),
        call_id: "call-delegate".to_string(),
        ..sample_tool_span()
    };
    let delegation = DelegationSpan {
        context: context.clone(),
        parent_run_id: "run-delegate".to_string(),
        child_run_id: Some("child-run-delegate".to_string()),
        target_agent_id: "worker".to_string(),
        tool_call_id: "call-delegate".to_string(),
        duration_ms: Some(125),
        success: true,
        error_message: None,
        timestamp_ms: 0,
    };

    sink.record(&MetricsEvent::Inference(inference.clone()));
    sink.record(&MetricsEvent::Tool(tool.clone()));
    sink.record(&MetricsEvent::Delegation(delegation.clone()));
    sink.on_run_end(&AgentMetrics {
        inferences: vec![inference],
        tools: vec![tool],
        delegations: vec![delegation],
        ..Default::default()
    });

    drop(sink);
    let _ = provider.shutdown();

    let spans = exporter.finished_spans();
    assert_eq!(
        spans.len(),
        4,
        "expected agent, inference, tool, delegation"
    );

    let agent = spans
        .iter()
        .find(|s| s.name.starts_with("invoke_agent"))
        .expect("agent span not found");
    let tool = spans
        .iter()
        .find(|s| s.name.as_ref() == "execute_tool agent_run_worker")
        .expect("agent tool span not found");
    let delegation = spans
        .iter()
        .find(|s| s.name.as_ref() == "awaken.delegation")
        .expect("delegation span not found");

    assert_eq!(
        tool.span_context.trace_id(),
        agent.span_context.trace_id(),
        "agent tool span should be in the parent agent trace"
    );
    assert_eq!(
        delegation.span_context.trace_id(),
        agent.span_context.trace_id(),
        "delegation span should be in the parent agent trace"
    );
    assert_ne!(
        delegation.parent_span_id,
        opentelemetry::trace::SpanId::INVALID,
        "delegation span should keep a parent in the trace tree"
    );

    let tool_attrs = attr_map(tool);
    assert_eq!(
        tool_attrs.get("gen_ai.tool.call.id").map(|v| v.to_string()),
        Some("call-delegate".to_string())
    );
    assert_eq!(
        tool_attrs.get("gen_ai.tool.name").map(|v| v.to_string()),
        Some("agent_run_worker".to_string())
    );

    let delegation_attrs = attr_map(delegation);
    assert_eq!(
        delegation_attrs
            .get("awaken.delegation.parent_run_id")
            .map(|v| v.to_string()),
        Some("run-delegate".to_string())
    );
    assert_eq!(
        delegation_attrs
            .get("awaken.delegation.child_run_id")
            .map(|v| v.to_string()),
        Some("child-run-delegate".to_string())
    );
    assert_eq!(
        delegation_attrs
            .get("awaken.delegation.target_agent_id")
            .map(|v| v.to_string()),
        Some("worker".to_string())
    );
    assert_eq!(
        delegation_attrs
            .get("gen_ai.tool.call.id")
            .map(|v| v.to_string()),
        Some("call-delegate".to_string())
    );
    assert_eq!(
        delegation_attrs
            .get("awaken.delegation.success")
            .map(|v| v.to_string()),
        Some("true".to_string())
    );
}

#[test]
fn otlp_background_task_span_attaches_to_parent_tool_context() {
    let (sink, exporter, provider) = make_capturing_sink();

    let context = SpanContext {
        run_id: "run-bg".to_string(),
        thread_id: "thread-bg".to_string(),
        agent_id: "agent-bg".to_string(),
        parent_run_id: None,
        parent_tool_call_id: None,
        ..Default::default()
    };
    let parent_inference = GenAISpan {
        context: context.clone(),
        model: "parent-model".to_string(),
        ..sample_genai_span()
    };
    let tool = ToolSpan {
        context: context.clone(),
        name: "spawn_background".to_string(),
        call_id: "call-bg".to_string(),
        ..sample_tool_span()
    };
    let background_context = SpanContext {
        parent_tool_call_id: Some("call-bg".to_string()),
        ..context.clone()
    };
    let running = BackgroundTaskSpan {
        context: background_context.clone(),
        ..sample_background_task_span(awaken_runtime::extensions::background::TaskStatus::Running)
    };
    let completed = BackgroundTaskSpan {
        context: background_context,
        status: awaken_runtime::extensions::background::TaskStatus::Completed,
        completed_at_ms: Some(1_500),
        ..running.clone()
    };

    sink.record(&MetricsEvent::Inference(parent_inference.clone()));
    sink.record(&MetricsEvent::Tool(tool.clone()));
    sink.record_background_task(&running);
    sink.record_background_task(&completed);
    sink.on_run_end(&AgentMetrics {
        inferences: vec![parent_inference],
        tools: vec![tool],
        background_tasks: vec![completed],
        session_duration_ms: 600,
        ..Default::default()
    });

    drop(sink);
    let _ = provider.shutdown();

    let spans = exporter.finished_spans();
    let tool = spans
        .iter()
        .find(|s| s.name.as_ref() == "execute_tool spawn_background")
        .expect("background spawning tool span not found");
    let background = spans
        .iter()
        .find(|s| s.name.as_ref() == "awaken.background_task")
        .expect("background task span not found");

    assert_eq!(
        background.span_context.trace_id(),
        tool.span_context.trace_id(),
        "background task should remain in the parent tool trace"
    );
    assert_eq!(
        background.parent_span_id,
        tool.span_context.span_id(),
        "background task should be parented to the tool call that spawned it"
    );
    let attrs = attr_map(background);
    assert_eq!(
        attrs
            .get("awaken.background_task.status")
            .map(|v| v.to_string()),
        Some("completed".to_string())
    );
    assert_eq!(
        attrs
            .get("awaken.background_task.parent_tool_call_id")
            .map(|v| v.to_string()),
        Some("call-bg".to_string())
    );
}

#[test]
fn otlp_background_task_before_tool_uses_real_tool_parent_and_duration() {
    let (sink, exporter, provider) = make_capturing_sink();

    let context = SpanContext {
        run_id: "run-bg-early".to_string(),
        thread_id: "thread-bg".to_string(),
        agent_id: "agent-bg".to_string(),
        parent_run_id: None,
        parent_tool_call_id: None,
        ..Default::default()
    };
    let parent_inference = GenAISpan {
        context: context.clone(),
        model: "parent-model".to_string(),
        ..sample_genai_span()
    };
    let tool = ToolSpan {
        context: context.clone(),
        name: "spawn_background".to_string(),
        call_id: "call-bg-early".to_string(),
        duration_ms: 250,
        started_at_ms: 0,
        ended_at_ms: 0,
        ..sample_tool_span()
    };
    let background_context = SpanContext {
        parent_tool_call_id: Some(tool.call_id.clone()),
        ..context.clone()
    };
    let running = BackgroundTaskSpan {
        context: background_context.clone(),
        task_id: "bg-early".to_string(),
        ..sample_background_task_span(awaken_runtime::extensions::background::TaskStatus::Running)
    };
    let completed = BackgroundTaskSpan {
        context: background_context,
        status: awaken_runtime::extensions::background::TaskStatus::Completed,
        completed_at_ms: Some(1_500),
        ..running.clone()
    };

    sink.record(&MetricsEvent::Inference(parent_inference.clone()));
    sink.record_background_task(&running);
    sink.record(&MetricsEvent::Tool(tool.clone()));
    sink.record_background_task(&completed);
    sink.on_run_end(&AgentMetrics {
        inferences: vec![parent_inference],
        tools: vec![tool],
        background_tasks: vec![completed],
        session_duration_ms: 600,
        ..Default::default()
    });

    drop(sink);
    let _ = provider.shutdown();

    let spans = exporter.finished_spans();
    let tool = spans
        .iter()
        .find(|s| s.name.as_ref() == "execute_tool spawn_background")
        .expect("background spawning tool span not found");
    let background = spans
        .iter()
        .find(|s| s.name.as_ref() == "awaken.background_task")
        .expect("background task span not found");

    assert_eq!(
        background.parent_span_id,
        tool.span_context.span_id(),
        "early background event should be reparented to the eventual tool span id"
    );
    let tool_duration = tool
        .end_time
        .duration_since(tool.start_time)
        .expect("tool end after start");
    assert_eq!(
        tool_duration,
        std::time::Duration::from_millis(250),
        "lazy tool completion should preserve ToolSpan duration"
    );
}

#[test]
fn otlp_run_end_defers_root_until_running_background_task_finishes() {
    let (sink, exporter, provider) = make_capturing_sink();

    let context = SpanContext {
        run_id: "run-bg-open".to_string(),
        thread_id: "thread-bg".to_string(),
        agent_id: "agent-bg".to_string(),
        parent_run_id: None,
        parent_tool_call_id: None,
        ..Default::default()
    };
    let inference = GenAISpan {
        context: context.clone(),
        model: "parent-model".to_string(),
        ..sample_genai_span()
    };
    let tool = ToolSpan {
        context: context.clone(),
        name: "spawn_background".to_string(),
        call_id: "call-bg-open".to_string(),
        ..sample_tool_span()
    };
    let background_context = SpanContext {
        parent_tool_call_id: Some(tool.call_id.clone()),
        ..context.clone()
    };
    let running = BackgroundTaskSpan {
        context: background_context.clone(),
        task_id: "bg-open".to_string(),
        ..sample_background_task_span(awaken_runtime::extensions::background::TaskStatus::Running)
    };
    let completed = BackgroundTaskSpan {
        context: background_context,
        status: awaken_runtime::extensions::background::TaskStatus::Completed,
        completed_at_ms: Some(1_500),
        ..running.clone()
    };

    sink.record(&MetricsEvent::Inference(inference.clone()));
    sink.record(&MetricsEvent::Tool(tool.clone()));
    sink.record_background_task(&running);
    sink.on_run_end(&AgentMetrics {
        inferences: vec![inference.clone()],
        tools: vec![tool.clone()],
        background_tasks: vec![running],
        session_duration_ms: 600,
        ..Default::default()
    });

    assert!(
        exporter
            .finished_spans()
            .iter()
            .all(|s| !s.name.starts_with("invoke_agent")),
        "root span should remain open while background task is running"
    );

    sink.record_background_task(&completed);
    drop(sink);
    let _ = provider.shutdown();

    let spans = exporter.finished_spans();
    let root = spans
        .iter()
        .find(|s| s.name.as_ref() == "invoke_agent agent-bg")
        .expect("root span should close after background task terminal event");
    let background = spans
        .iter()
        .find(|s| s.name.as_ref() == "awaken.background_task")
        .expect("background task span not found");
    assert_eq!(
        background.span_context.trace_id(),
        root.span_context.trace_id()
    );
}

#[tokio::test]
async fn otlp_background_subagent_run_is_parented_to_background_task_span() {
    use std::sync::Arc;

    use awaken_runtime::extensions::background::{
        BackgroundTaskManager, BackgroundTaskPlugin, TaskParentContext, TaskResult,
    };

    let (sink, exporter, provider) = make_capturing_sink();
    let sink = Arc::new(sink);
    let store = awaken_runtime::StateStore::new();
    let manager = Arc::new(BackgroundTaskManager::new());
    manager.set_store(store.clone());
    store
        .install_plugin(BackgroundTaskPlugin::new(manager.clone()))
        .expect("background keys should register");

    let parent_context = SpanContext {
        run_id: "run-bg-subagent".to_string(),
        thread_id: "thread-bg-subagent".to_string(),
        agent_id: "agent-bg".to_string(),
        parent_run_id: None,
        parent_tool_call_id: None,
        ..Default::default()
    };
    let parent_inference = GenAISpan {
        context: parent_context.clone(),
        model: "parent-model".to_string(),
        ..sample_genai_span()
    };
    let tool = ToolSpan {
        context: parent_context.clone(),
        name: "spawn_background".to_string(),
        call_id: "call-bg-subagent".to_string(),
        ..sample_tool_span()
    };
    let child_context = SpanContext {
        run_id: "child-bg-run".to_string(),
        thread_id: "child-bg-run".to_string(),
        agent_id: "worker".to_string(),
        parent_run_id: Some(parent_context.run_id.clone()),
        parent_tool_call_id: Some(tool.call_id.clone()),
        ..Default::default()
    };
    let child_inference = GenAISpan {
        context: child_context.clone(),
        model: "child-model".to_string(),
        ..sample_genai_span()
    };
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();

    sink.record(&MetricsEvent::Inference(parent_inference.clone()));
    let task_id = manager
        .spawn(
            "thread-bg-subagent",
            "sub_agent",
            Some("worker"),
            "worker agent",
            TaskParentContext {
                run_id: Some(parent_context.run_id.clone()),
                call_id: Some(tool.call_id.clone()),
                agent_id: Some(parent_context.agent_id.clone()),
                task_id: None,
            },
            {
                let sink = sink.clone();
                let child_inference = child_inference.clone();
                move |_ctx| async move {
                    // The child agent run can be observed before the spawning
                    // tool span and background lifecycle hook are emitted; it
                    // should still be nested as Tool -> BackgroundTask ->
                    // invoke_agent by reading the runtime task-local context.
                    sink.record(&MetricsEvent::Inference(child_inference));
                    let _ = done_tx.send(());
                    TaskResult::Success(json!({}))
                }
            },
        )
        .await
        .expect("background sub-agent task should spawn");
    done_rx
        .await
        .expect("background child inference should be recorded");

    sink.on_run_end(&AgentMetrics {
        inferences: vec![child_inference.clone()],
        session_duration_ms: 300,
        ..Default::default()
    });
    let completed = BackgroundTaskSpan {
        context: SpanContext {
            parent_tool_call_id: Some(tool.call_id.clone()),
            ..parent_context.clone()
        },
        task_id: task_id.clone(),
        status: awaken_runtime::extensions::background::TaskStatus::Completed,
        completed_at_ms: Some(1_500),
        ..sample_background_task_span(awaken_runtime::extensions::background::TaskStatus::Completed)
    };
    sink.record(&MetricsEvent::Tool(tool.clone()));
    sink.record_background_task(&completed);
    sink.on_run_end(&AgentMetrics {
        inferences: vec![parent_inference],
        tools: vec![tool],
        background_tasks: vec![completed],
        session_duration_ms: 900,
        ..Default::default()
    });

    drop(sink);
    let _ = provider.shutdown();

    let spans = exporter.finished_spans();
    let tool = spans
        .iter()
        .find(|s| s.name.as_ref() == "execute_tool spawn_background")
        .expect("background spawning tool span not found");
    let background = spans
        .iter()
        .filter(|s| s.name.as_ref() == "awaken.background_task")
        .collect::<Vec<_>>();
    assert_eq!(
        background.len(),
        1,
        "background task lineage should use one task span"
    );
    let background = background[0];
    let child_agent = spans
        .iter()
        .find(|s| s.name.as_ref() == "invoke_agent worker")
        .expect("child agent root span not found");
    let child_chat = spans
        .iter()
        .find(|s| s.name.as_ref() == "chat child-model")
        .expect("child chat span not found");

    assert_eq!(
        background.span_context.trace_id(),
        tool.span_context.trace_id()
    );
    assert_eq!(
        child_agent.span_context.trace_id(),
        tool.span_context.trace_id()
    );
    assert_eq!(
        background.parent_span_id,
        tool.span_context.span_id(),
        "background task should stay under the spawning tool"
    );
    assert_eq!(
        child_agent.parent_span_id,
        background.span_context.span_id(),
        "background sub-agent root should be parented to the task span"
    );
    assert_eq!(
        child_chat.parent_span_id,
        child_agent.span_context.span_id(),
        "child chat should remain under the child agent root"
    );

    let child_attrs = attr_map(child_agent);
    assert_eq!(
        child_attrs
            .get("awaken.parent_task.id")
            .map(|v| v.to_string()),
        Some(task_id)
    );
}

#[test]
fn otlp_subagent_invoke_agent_inherits_parent_run_context() {
    let (sink, exporter, provider) = make_capturing_sink();

    let parent_context = SpanContext {
        run_id: "parent-run".to_string(),
        thread_id: "thread-delegate".to_string(),
        agent_id: "orchestrator".to_string(),
        parent_run_id: None,
        parent_tool_call_id: None,
        ..Default::default()
    };
    let child_context = SpanContext {
        run_id: "child-run".to_string(),
        thread_id: "child-run".to_string(),
        agent_id: "worker".to_string(),
        parent_run_id: Some("parent-run".to_string()),
        parent_tool_call_id: None,
        ..Default::default()
    };
    let parent_inference = GenAISpan {
        context: parent_context.clone(),
        model: "parent-model".to_string(),
        ..sample_genai_span()
    };
    let child_inference = GenAISpan {
        context: child_context.clone(),
        model: "child-model".to_string(),
        ..sample_genai_span()
    };

    sink.record(&MetricsEvent::Inference(parent_inference.clone()));
    sink.record(&MetricsEvent::Inference(child_inference.clone()));
    sink.on_run_end(&AgentMetrics {
        inferences: vec![child_inference],
        session_duration_ms: 250,
        ..Default::default()
    });
    sink.on_run_end(&AgentMetrics {
        inferences: vec![parent_inference],
        session_duration_ms: 500,
        ..Default::default()
    });

    drop(sink);
    let _ = provider.shutdown();

    let spans = exporter.finished_spans();
    let parent_agent = spans
        .iter()
        .find(|s| s.name.as_ref() == "invoke_agent orchestrator")
        .expect("parent agent span not found");
    let parent_chat = spans
        .iter()
        .find(|s| s.name.as_ref() == "chat parent-model")
        .expect("parent inference span not found");
    let child_agent = spans
        .iter()
        .find(|s| s.name.as_ref() == "invoke_agent worker")
        .expect("child agent span not found");
    let child_chat = spans
        .iter()
        .find(|s| s.name.as_ref() == "chat child-model")
        .expect("child inference span not found");

    assert_eq!(
        child_agent.span_context.trace_id(),
        parent_agent.span_context.trace_id(),
        "child invoke_agent should share the parent trace id"
    );
    assert_eq!(
        child_agent.parent_span_id,
        parent_chat.span_context.span_id(),
        "child invoke_agent should inherit the parent run's active inference context"
    );
    assert_eq!(
        child_chat.parent_span_id,
        child_agent.span_context.span_id(),
        "child inference should remain under the child invoke_agent span"
    );

    let child_agent_attrs = attr_map(child_agent);
    assert_eq!(
        child_agent_attrs
            .get("awaken.parent_run.id")
            .map(|v| v.to_string()),
        Some("parent-run".to_string())
    );
}

#[test]
fn otlp_run_end_closes_agent_span() {
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
    // 1 inference + 1 tool + 1 agent = 3 spans
    assert_eq!(spans.len(), 3, "expected 3 exported spans");

    // Find the agent span.
    let agent = spans
        .iter()
        .find(|s| s.name.starts_with("invoke_agent"))
        .expect("agent span not found");

    assert_eq!(agent.span_kind, opentelemetry::trace::SpanKind::Internal);

    // Agent span is the root — no parent.
    assert_eq!(
        agent.parent_span_id,
        opentelemetry::trace::SpanId::INVALID,
        "agent span should be the root (no parent)"
    );

    // All other spans share the same trace_id as the session.
    let trace_id = agent.span_context.trace_id();
    for s in &spans {
        assert_eq!(
            s.span_context.trace_id(),
            trace_id,
            "span '{}' should share trace_id with agent",
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
        agent.span_context.span_id(),
        "inference span should be a child of the agent"
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

    let attrs = attr_map(agent);
    assert_eq!(
        attrs.get("gen_ai.operation.name").map(|v| v.to_string()),
        Some("invoke_agent".to_string())
    );
    assert_eq!(
        attrs.get("gen_ai.provider.name").map(|v| v.to_string()),
        Some("openai".to_string())
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
        attrs
            .get("awaken.session.inference_count")
            .map(|v| v.to_string()),
        Some("1".to_string())
    );
    assert_eq!(
        attrs
            .get("awaken.session.tool_count")
            .map(|v| v.to_string()),
        Some("1".to_string())
    );
    assert_eq!(
        attrs
            .get("awaken.session.tool_failures")
            .map(|v| v.to_string()),
        Some("0".to_string())
    );
    assert!(attrs.contains_key("awaken.session.duration"));
}

#[test]
fn otlp_internal_events_do_not_overwrite_agent_provider() {
    let (sink, exporter, provider) = make_capturing_sink();

    let context = SpanContext {
        run_id: "run-provider".to_string(),
        thread_id: "thread-provider".to_string(),
        agent_id: "agent-provider".to_string(),
        parent_run_id: None,
        parent_tool_call_id: None,
        ..Default::default()
    };
    sink.record(&MetricsEvent::Inference(GenAISpan {
        context: context.clone(),
        provider: "openai".to_string(),
        ..sample_genai_span()
    }));
    sink.record(&MetricsEvent::Handoff(HandoffSpan {
        context,
        from_agent_id: "agent-provider".to_string(),
        to_agent_id: "agent-next".to_string(),
        reason: Some("handoff".to_string()),
        timestamp_ms: 0,
    }));
    sink.on_run_end(&AgentMetrics::default());

    drop(sink);
    let _ = provider.shutdown();

    let spans = exporter.finished_spans();
    let agent = spans
        .iter()
        .find(|s| s.name.starts_with("invoke_agent"))
        .expect("agent span not found");
    let attrs = attr_map(agent);
    assert_eq!(
        attrs.get("gen_ai.provider.name").map(|v| v.to_string()),
        Some("openai".to_string())
    );
}

#[test]
fn otlp_evaluation_result_event_is_parented_to_active_inference() {
    let (sink, exporter, provider) = make_capturing_sink();

    let context = SpanContext {
        run_id: "run-eval".to_string(),
        thread_id: "thread-eval".to_string(),
        agent_id: "agent-eval".to_string(),
        parent_run_id: None,
        parent_tool_call_id: None,
        ..Default::default()
    };
    let inference = GenAISpan {
        context: context.clone(),
        response_id: Some("chatcmpl-eval".to_string()),
        ..sample_genai_span()
    };

    sink.record(&MetricsEvent::Inference(inference.clone()));
    sink.record_evaluation_result(&EvaluationResultEvent {
        context,
        name: "faithfulness".to_string(),
        score_label: Some("pass".to_string()),
        score_value: Some(1.0),
        explanation: Some("answer is grounded".to_string()),
        response_id: Some("chatcmpl-eval".to_string()),
        error_type: None,
        timestamp_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_millis() as u64,
    });
    sink.on_run_end(&AgentMetrics {
        inferences: vec![inference],
        ..Default::default()
    });

    drop(sink);
    let _ = provider.shutdown();

    let spans = exporter.finished_spans();
    let inference_span = spans
        .iter()
        .find(|s| s.name.as_ref() == "chat gpt-4")
        .expect("inference span not found");
    let event = inference_span
        .events
        .iter()
        .find(|event| event.name.as_ref() == "gen_ai.evaluation.result")
        .expect("evaluation event not found");

    assert_eq!(
        kv_string(&event.attributes, "gen_ai.evaluation.name"),
        Some("faithfulness")
    );
    assert_eq!(
        kv_string(&event.attributes, "gen_ai.evaluation.score.label"),
        Some("pass")
    );
    assert_eq!(
        kv_string(&event.attributes, "gen_ai.evaluation.explanation"),
        Some("answer is grounded")
    );
    assert_eq!(
        kv_string(&event.attributes, "gen_ai.response.id"),
        Some("chatcmpl-eval")
    );
    let event_attrs: HashMap<String, opentelemetry::Value> = event
        .attributes
        .iter()
        .map(|kv| (kv.key.to_string(), kv.value.clone()))
        .collect();
    assert_eq!(
        event_attrs
            .get("gen_ai.evaluation.score.value")
            .map(|v| v.to_string()),
        Some("1".to_string())
    );
    assert_eq!(
        kv_string(&event.attributes, "awaken.run.id"),
        Some("run-eval")
    );
}

#[test]
fn otlp_multi_step_creates_correlated_spans() {
    let (sink, exporter, provider) = make_capturing_sink();

    let ctx = SpanContext {
        run_id: "run-99".to_string(),
        thread_id: "thread-1".to_string(),
        agent_id: "agent-beta".to_string(),
        parent_run_id: None,
        parent_tool_call_id: None,
        ..Default::default()
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
        inferences: vec![
            GenAISpan {
                context: ctx.clone(),
                step_index: Some(0),
                model: "gpt-4".to_string(),
                ..sample_genai_span()
            },
            GenAISpan {
                context: ctx.clone(),
                step_index: Some(1),
                model: "gpt-4".to_string(),
                ..sample_genai_span()
            },
        ],
        tools: vec![sample_tool_span(), sample_tool_span(), sample_tool_span()],
        session_duration_ms: 5000,
        ..Default::default()
    });

    drop(sink);
    let _ = provider.shutdown();

    let spans = exporter.finished_spans();
    // 2 inferences + 3 tools + 1 agent = 6
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

    // Agent span is root (no parent).
    let agent = spans
        .iter()
        .find(|s| s.name.starts_with("invoke_agent"))
        .expect("agent span not found");
    assert_eq!(agent.parent_span_id, opentelemetry::trace::SpanId::INVALID);

    // Both inference spans are children of the session.
    let inferences: Vec<_> = spans
        .iter()
        .filter(|s| s.name.starts_with("chat"))
        .collect();
    assert_eq!(inferences.len(), 2);
    for inf in &inferences {
        assert_eq!(
            inf.parent_span_id,
            agent.span_context.span_id(),
            "inference span should be child of agent"
        );
    }

    // Step 0 tools are children of the step-0 inference.
    let step0_inference = inferences
        .iter()
        .find(|s| {
            attr_map(s).get("awaken.step.index").map(|v| v.to_string()) == Some("0".to_string())
        })
        .expect("step 0 inference not found");
    let step0_tools: Vec<_> = spans
        .iter()
        .filter(|s| {
            let a = attr_map(s);
            s.name.starts_with("execute_tool")
                && a.get("awaken.step.index").map(|v| v.to_string()) == Some("0".to_string())
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
        .find(|s| {
            attr_map(s).get("awaken.step.index").map(|v| v.to_string()) == Some("1".to_string())
        })
        .expect("step 1 inference not found");
    let step1_tools: Vec<_> = spans
        .iter()
        .filter(|s| {
            let a = attr_map(s);
            s.name.starts_with("execute_tool")
                && a.get("awaken.step.index").map(|v| v.to_string()) == Some("1".to_string())
        })
        .collect();
    assert_eq!(step1_tools.len(), 1, "expected 1 tool at step 1");
    assert_eq!(
        step1_tools[0].parent_span_id,
        step1_inference.span_context.span_id(),
        "step-1 tool should be child of step-1 inference"
    );

    // All spans share the same awaken.run.id.
    for s in &spans {
        let attrs = attr_map(s);
        assert_eq!(
            attrs.get("awaken.run.id").map(|v| v.to_string()),
            Some("run-99".to_string()),
            "span '{}' missing awaken.run.id",
            s.name
        );
    }
}

#[test]
fn tool_span_uses_absolute_timestamps_when_provided() {
    let (sink, exporter, provider) = make_capturing_sink();
    let context = SpanContext {
        run_id: "run-time".to_string(),
        thread_id: "thread-time".to_string(),
        agent_id: "agent-time".to_string(),
        parent_run_id: None,
        parent_tool_call_id: None,
        ..Default::default()
    };
    sink.record(&MetricsEvent::Inference(GenAISpan {
        context: context.clone(),
        ..sample_genai_span()
    }));
    let started_at_ms: u64 = 1_700_000_000_000;
    let duration_ms: u64 = 150;
    let tool = ToolSpan {
        context: context.clone(),
        step_index: Some(0),
        duration_ms,
        started_at_ms,
        ended_at_ms: started_at_ms + duration_ms,
        ..sample_tool_span()
    };
    sink.record(&MetricsEvent::Tool(tool));
    sink.on_run_end(&AgentMetrics::default());
    drop(sink);
    let _ = provider.shutdown();

    let spans = exporter.finished_spans();
    let tool_span = spans
        .iter()
        .find(|s| s.name.starts_with("execute_tool"))
        .expect("tool span exported");
    let start_ms = tool_span
        .start_time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let end_ms = tool_span
        .end_time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert_eq!(
        start_ms, started_at_ms,
        "OTel start should equal started_at_ms"
    );
    assert_eq!(
        end_ms,
        started_at_ms + duration_ms,
        "OTel end should equal started_at_ms + duration"
    );
}

#[test]
fn drop_marks_running_background_task_with_shutdown_close_reason() {
    // Sink is dropped while a background task is still running. The
    // exported span must surface that abandonment via close_reason
    // instead of being silently truncated, so dashboards can tell
    // shutdown-killed tasks apart from natural completions.
    use awaken_runtime::extensions::background::TaskStatus;

    let exporter = capture::InMemorySpanExporter::new();
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = {
        use opentelemetry::trace::TracerProvider;
        provider.tracer("awaken-test")
    };
    let sink = OtelMetricsSink::new(tracer);

    let context = SpanContext {
        run_id: "run-shutdown".to_string(),
        thread_id: "thread-shutdown".to_string(),
        agent_id: "agent-shutdown".to_string(),
        parent_run_id: None,
        parent_tool_call_id: None,
        ..Default::default()
    };
    sink.record(&MetricsEvent::Inference(GenAISpan {
        context: context.clone(),
        ..sample_genai_span()
    }));
    let running = BackgroundTaskSpan {
        context: context.clone(),
        status: TaskStatus::Running,
        ..sample_background_task_span(TaskStatus::Running)
    };
    sink.record_background_task(&running);

    // Drop without ever flushing or completing the task.
    drop(sink);
    let _ = provider.shutdown();

    let spans = exporter.finished_spans();
    let bg = spans
        .iter()
        .find(|s| s.name.as_ref() == "awaken.background_task")
        .expect("background span should still be exported on drop");
    let attrs = attr_map(bg);
    assert_eq!(
        attrs
            .get("awaken.background_task.close_reason")
            .map(|v| v.to_string()),
        Some("shutdown".to_string()),
        "Drop path must mark abandoned background tasks with close_reason=shutdown",
    );
}

#[test]
fn background_tasks_with_same_id_in_different_runs_get_independent_spans() {
    // Two tasks happen to share `task_id = "bg-1"` but live in different
    // runs. Each must produce its own OTel span; completing one must not
    // close the other's span. Earlier the OTel maps were keyed by
    // `task_id` alone, which would silently merge them.
    use awaken_runtime::extensions::background::TaskStatus;

    let (sink, exporter, provider) = make_capturing_sink();

    let make_ctx = |run: &str, thread: &str| SpanContext {
        run_id: run.to_string(),
        thread_id: thread.to_string(),
        agent_id: "agent".to_string(),
        parent_run_id: None,
        parent_tool_call_id: None,
        ..Default::default()
    };

    // Open both runs' roots, then record one Running event in each.
    sink.record(&MetricsEvent::Inference(GenAISpan {
        context: make_ctx("run-a", "thread-a"),
        ..sample_genai_span()
    }));
    sink.record(&MetricsEvent::Inference(GenAISpan {
        context: make_ctx("run-b", "thread-b"),
        ..sample_genai_span()
    }));
    let mut shared = sample_background_task_span(TaskStatus::Running);
    shared.task_id = "bg-1".to_string();
    let task_a = BackgroundTaskSpan {
        context: make_ctx("run-a", "thread-a"),
        ..shared.clone()
    };
    let task_b = BackgroundTaskSpan {
        context: make_ctx("run-b", "thread-b"),
        ..shared
    };
    sink.record_background_task(&task_a);
    sink.record_background_task(&task_b);

    // Completing run-a's task must not affect run-b's task.
    let completed_a = BackgroundTaskSpan {
        status: TaskStatus::Completed,
        completed_at_ms: Some(2_000),
        ..task_a
    };
    sink.record_background_task(&completed_a);

    // run-b stays running; close it via flush_run after run-end.
    sink.on_run_end(&AgentMetrics::default());
    let run_b_key = OtelMetricsSink::run_key(&make_ctx("run-b", "thread-b"));
    sink.flush_run(&run_b_key, "abandoned");

    drop(sink);
    let _ = provider.shutdown();

    let bg_spans: Vec<_> = exporter
        .finished_spans()
        .into_iter()
        .filter(|s| s.name.as_ref() == "awaken.background_task")
        .collect();
    assert_eq!(
        bg_spans.len(),
        2,
        "same-id tasks in different runs must each get their own span"
    );

    let close_reasons: Vec<_> = bg_spans
        .iter()
        .filter_map(|s| {
            attr_map(s)
                .get("awaken.background_task.close_reason")
                .map(|v| v.to_string())
        })
        .collect();
    assert_eq!(
        close_reasons,
        vec!["abandoned".to_string()],
        "only run-b's task should carry close_reason; run-a completed cleanly"
    );
}

#[test]
fn background_task_with_parent_run_id_attaches_to_parent_run_tool_span() {
    // The spawning agent runs a tool in `parent-run`; that tool spawns a
    // background task with its own `run_id = child-run` but the parent
    // lineage is conveyed via `parent_run_id`. The background task span
    // MUST nest under the parent-run tool span, not under a stranded
    // synthetic context in `child-run`.
    use awaken_runtime::extensions::background::TaskStatus;

    let (sink, exporter, provider) = make_capturing_sink();
    let parent_context = SpanContext {
        run_id: "parent-run".to_string(),
        thread_id: "thread-parent".to_string(),
        agent_id: "agent-parent".to_string(),
        parent_run_id: None,
        parent_tool_call_id: None,
        ..Default::default()
    };

    // Open the parent run's root + a tool span that has the call_id we
    // are going to reference from the background task.
    sink.record(&MetricsEvent::Inference(GenAISpan {
        context: parent_context.clone(),
        ..sample_genai_span()
    }));
    let tool = ToolSpan {
        context: parent_context.clone(),
        call_id: "call-cross-run".to_string(),
        ..sample_tool_span()
    };
    sink.record(&MetricsEvent::Tool(tool.clone()));

    // Background task lives in a different run (`child-run`) but points at
    // the parent run via `parent_run_id` and the parent tool via
    // `parent_tool_call_id`.
    let bg_context = SpanContext {
        run_id: "child-run".to_string(),
        thread_id: "thread-child".to_string(),
        agent_id: "agent-child".to_string(),
        parent_run_id: Some("parent-run".to_string()),
        parent_tool_call_id: Some("call-cross-run".to_string()),
        ..Default::default()
    };
    let completed = BackgroundTaskSpan {
        context: bg_context,
        status: TaskStatus::Completed,
        completed_at_ms: Some(2_000),
        ..sample_background_task_span(TaskStatus::Completed)
    };
    sink.record_background_task(&completed);
    sink.on_run_end(&AgentMetrics::default());
    drop(sink);
    let _ = provider.shutdown();

    let spans = exporter.finished_spans();
    let tool_span = spans
        .iter()
        .find(|s| s.name.starts_with("execute_tool"))
        .expect("parent tool span exported");
    let bg_span = spans
        .iter()
        .find(|s| s.name.as_ref() == "awaken.background_task")
        .expect("background task span exported");
    assert_eq!(
        bg_span.parent_span_id,
        tool_span.span_context.span_id(),
        "background task must parent under the parent-run tool span"
    );
    assert_eq!(
        bg_span.span_context.trace_id(),
        tool_span.span_context.trace_id(),
        "background task must share parent run's trace id"
    );
}

#[test]
fn synthetic_tool_span_anchors_at_earliest_child_timestamp() {
    // Background task arrives before the real tool span; the real one
    // never shows up. The synthetic parent span must start at the
    // earliest child's `created_at_ms`, not at run-end's "now", so the
    // exported trace stays causal.
    use awaken_runtime::extensions::background::TaskStatus;

    let (sink, exporter, provider) = make_capturing_sink();
    let context = SpanContext {
        run_id: "run-synth".to_string(),
        thread_id: "thread-synth".to_string(),
        agent_id: "agent-synth".to_string(),
        parent_run_id: None,
        parent_tool_call_id: None,
        ..Default::default()
    };

    // Open a root via an inference, then a background task that
    // references a tool call id whose ToolSpan we never emit.
    sink.record(&MetricsEvent::Inference(GenAISpan {
        context: context.clone(),
        ..sample_genai_span()
    }));
    let child_created_at_ms: u64 = 1_700_000_000_500;
    let bg_context = SpanContext {
        parent_tool_call_id: Some("call-missing".to_string()),
        ..context.clone()
    };
    let running = BackgroundTaskSpan {
        context: bg_context.clone(),
        created_at_ms: child_created_at_ms,
        status: TaskStatus::Running,
        ..sample_background_task_span(TaskStatus::Running)
    };
    sink.record_background_task(&running);
    let completed = BackgroundTaskSpan {
        context: bg_context,
        created_at_ms: child_created_at_ms,
        completed_at_ms: Some(child_created_at_ms + 50),
        status: TaskStatus::Completed,
        ..sample_background_task_span(TaskStatus::Completed)
    };
    sink.record_background_task(&completed);
    sink.on_run_end(&AgentMetrics::default());
    drop(sink);
    let _ = provider.shutdown();

    let spans = exporter.finished_spans();
    let synthetic = spans
        .iter()
        .find(|s| {
            s.name.as_ref() == "execute_tool"
                && attr_map(s)
                    .get("awaken.tool.synthetic_parent")
                    .is_some_and(|v| v.to_string() == "true")
        })
        .expect("synthetic execute_tool span not found");
    let start_ms = synthetic
        .start_time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert_eq!(
        start_ms, child_created_at_ms,
        "synthetic parent must start at earliest child"
    );
    assert!(
        synthetic.end_time >= synthetic.start_time,
        "synthetic span must not end before it starts"
    );
}

#[test]
fn flush_run_reaches_otel_sink_through_batching_and_composite() {
    // Pin: a caller holding only `Arc<dyn MetricsSink>` (CompositeSink
    // wrapping a BatchingSink wrapping the OTel sink) can still trigger
    // the close-reason path. The trait method must forward, and any
    // buffered events must drain before close so the OTel sink sees the
    // running task before being asked to close it.
    use std::sync::Arc;

    use awaken_runtime::extensions::background::TaskStatus;

    use crate::{BatchingConfig, BatchingSink, CompositeSink};

    let exporter = capture::InMemorySpanExporter::new();
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = {
        use opentelemetry::trace::TracerProvider;
        provider.tracer("awaken-flush-run-test")
    };
    let otel: Arc<dyn MetricsSink> = Arc::new(OtelMetricsSink::new(tracer));
    let batching: Arc<dyn MetricsSink> = Arc::new(BatchingSink::new(
        otel,
        BatchingConfig {
            max_batch_size: 1024,
            max_buffer_size: 4096,
            ..Default::default()
        },
    ));
    let composite: Arc<dyn MetricsSink> = Arc::new(CompositeSink::new(vec![batching]));

    let context = SpanContext {
        run_id: "run-trait".to_string(),
        thread_id: "thread-trait".to_string(),
        agent_id: "agent-trait".to_string(),
        parent_run_id: None,
        parent_tool_call_id: None,
        ..Default::default()
    };
    composite.record(&MetricsEvent::Inference(GenAISpan {
        context: context.clone(),
        ..sample_genai_span()
    }));
    composite.record(&MetricsEvent::BackgroundTask(BackgroundTaskSpan {
        context: context.clone(),
        status: TaskStatus::Running,
        ..sample_background_task_span(TaskStatus::Running)
    }));

    composite
        .flush_run(&OtelMetricsSink::run_key(&context), "abandoned")
        .expect("flush_run via trait must succeed");

    drop(composite);
    let _ = provider.shutdown();

    let spans = exporter.finished_spans();
    let bg = spans
        .iter()
        .find(|s| s.name.as_ref() == "awaken.background_task")
        .expect("background span must exist");
    let attrs = attr_map(bg);
    assert_eq!(
        attrs
            .get("awaken.background_task.close_reason")
            .map(|v| v.to_string()),
        Some("abandoned".to_string())
    );
}

#[test]
fn flush_run_force_closes_running_background_task_with_close_reason() {
    use awaken_runtime::extensions::background::TaskStatus;

    let (sink, exporter, provider) = make_capturing_sink();
    let context = SpanContext {
        run_id: "run-abandoned".to_string(),
        thread_id: "thread-bg".to_string(),
        agent_id: "agent-bg".to_string(),
        parent_run_id: None,
        parent_tool_call_id: Some("call-bg".to_string()),
        ..Default::default()
    };

    // Inference creates the root, then a never-terminal background task.
    sink.record(&MetricsEvent::Inference(GenAISpan {
        context: context.clone(),
        ..sample_genai_span()
    }));
    let running = BackgroundTaskSpan {
        context: context.clone(),
        status: TaskStatus::Running,
        ..sample_background_task_span(TaskStatus::Running)
    };
    sink.record_background_task(&running);
    // Run ends but background task never reports terminal status.
    sink.on_run_end(&AgentMetrics {
        inferences: vec![GenAISpan {
            context: context.clone(),
            ..sample_genai_span()
        }],
        session_duration_ms: 100,
        ..Default::default()
    });
    assert!(
        sink.has_running_background_tasks_for_run(&OtelMetricsSink::run_key(&context)),
        "background task should still be tracked before flush"
    );

    sink.flush_run(&OtelMetricsSink::run_key(&context), "abandoned");

    assert!(
        !sink.has_running_background_tasks_for_run(&OtelMetricsSink::run_key(&context)),
        "flush_run must clear active background tasks"
    );

    drop(sink);
    let _ = provider.shutdown();

    let spans = exporter.finished_spans();
    let bg = spans
        .iter()
        .find(|s| s.name.as_ref() == "awaken.background_task")
        .expect("background task span should be flushed");
    let attrs = attr_map(bg);
    assert_eq!(
        attrs
            .get("awaken.background_task.close_reason")
            .map(|v| v.to_string()),
        Some("abandoned".to_string())
    );
}
