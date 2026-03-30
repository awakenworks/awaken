// Phoenix/OTLP observability integration tests.
//
// These tests require a running Phoenix instance accepting OTLP/HTTP traces:
//   docker run -p 6006:6006 -p 4318:4318 arizephoenix/phoenix
//
// Set OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 (or PHOENIX_COLLECTOR_ENDPOINT)
// before running these tests:
//   cargo test -p awaken-server --test phoenix_observability_e2e -- --ignored
//
// In-memory OTLP span verification (no external infra) lives in:
//   crates/awaken-ext-observability/src/otel.rs (unit tests behind `otel` feature)

use awaken_ext_observability::otel::init_otlp_tracer;
use awaken_ext_observability::{
    AgentMetrics, GenAISpan, MetricsEvent, MetricsSink, OtelConfig, OtelMetricsSink, SpanContext,
    ToolSpan,
};

fn phoenix_configured() -> bool {
    std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok()
        || std::env::var("PHOENIX_COLLECTOR_ENDPOINT").is_ok()
}

fn build_config() -> OtelConfig {
    if let Ok(endpoint) = std::env::var("PHOENIX_COLLECTOR_ENDPOINT") {
        OtelConfig::builder()
            .endpoint(endpoint)
            .service_name("awaken-test")
            .service_version("0.0.0-test")
            .build()
    } else {
        let mut cfg = OtelConfig::from_env();
        if cfg.service_name.is_none() {
            cfg.service_name = Some("awaken-test".to_string());
        }
        cfg
    }
}

fn sample_genai_span(run_id: &str, step: u32) -> GenAISpan {
    GenAISpan {
        context: SpanContext {
            run_id: run_id.to_string(),
            thread_id: "thread-phoenix-test".to_string(),
            agent_id: "agent-phoenix-test".to_string(),
            parent_run_id: None,
        },
        step_index: Some(step),
        model: "gpt-4-test".to_string(),
        provider: "openai".to_string(),
        operation: "chat".to_string(),
        response_model: Some("gpt-4-0125-preview".to_string()),
        response_id: Some(format!("chatcmpl-phoenix-{step}")),
        finish_reasons: vec!["stop".to_string()],
        error_type: None,
        error_class: None,
        thinking_tokens: None,
        input_tokens: Some(200),
        output_tokens: Some(80),
        total_tokens: Some(280),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        temperature: Some(0.7),
        top_p: None,
        max_tokens: Some(4096),
        stop_sequences: Vec::new(),
        duration_ms: 1500,
    }
}

fn sample_tool_span(run_id: &str, step: u32, name: &str) -> ToolSpan {
    ToolSpan {
        context: SpanContext {
            run_id: run_id.to_string(),
            thread_id: "thread-phoenix-test".to_string(),
            agent_id: "agent-phoenix-test".to_string(),
            parent_run_id: None,
        },
        step_index: Some(step),
        name: name.to_string(),
        operation: "execute_tool".to_string(),
        call_id: format!("call_{name}_{step}"),
        tool_type: "function".to_string(),
        error_type: None,
        duration_ms: 120,
    }
}

#[ignore = "requires running Phoenix: docker run -p 6006:6006 -p 4318:4318 arizephoenix/phoenix"]
#[tokio::test]
async fn phoenix_receives_genai_inference_span() {
    if !phoenix_configured() {
        return;
    }

    let config = build_config();
    let (provider, tracer) = init_otlp_tracer(&config).expect("failed to init OTLP tracer");

    let sink = OtelMetricsSink::new(tracer);
    let run_id = format!("phoenix-test-inference-{}", uuid::Uuid::new_v4());

    sink.record(&MetricsEvent::Inference(sample_genai_span(&run_id, 0)));

    drop(sink);
    provider.shutdown().expect("provider shutdown failed");
}

#[ignore = "requires running Phoenix: docker run -p 6006:6006 -p 4318:4318 arizephoenix/phoenix"]
#[tokio::test]
async fn phoenix_receives_tool_span_correlated_with_inference() {
    if !phoenix_configured() {
        return;
    }

    let config = build_config();
    let (provider, tracer) = init_otlp_tracer(&config).expect("failed to init OTLP tracer");

    let sink = OtelMetricsSink::new(tracer);
    let run_id = format!("phoenix-test-correlated-{}", uuid::Uuid::new_v4());

    sink.record(&MetricsEvent::Inference(sample_genai_span(&run_id, 0)));
    sink.record(&MetricsEvent::Tool(sample_tool_span(&run_id, 0, "search")));

    drop(sink);
    provider.shutdown().expect("provider shutdown failed");
}

#[ignore = "requires running Phoenix: docker run -p 6006:6006 -p 4318:4318 arizephoenix/phoenix"]
#[tokio::test]
async fn phoenix_receives_full_agent_session() {
    if !phoenix_configured() {
        return;
    }

    let config = build_config();
    let (provider, tracer) = init_otlp_tracer(&config).expect("failed to init OTLP tracer");

    let sink = OtelMetricsSink::new(tracer);
    let run_id = format!("phoenix-test-session-{}", uuid::Uuid::new_v4());

    sink.record(&MetricsEvent::Inference(sample_genai_span(&run_id, 0)));
    sink.record(&MetricsEvent::Tool(sample_tool_span(&run_id, 0, "search")));
    sink.record(&MetricsEvent::Tool(sample_tool_span(
        &run_id,
        0,
        "read_file",
    )));
    sink.record(&MetricsEvent::Inference(sample_genai_span(&run_id, 1)));
    sink.record(&MetricsEvent::Tool(sample_tool_span(
        &run_id,
        1,
        "write_file",
    )));

    let metrics = AgentMetrics {
        inferences: vec![sample_genai_span(&run_id, 0), sample_genai_span(&run_id, 1)],
        tools: vec![
            sample_tool_span(&run_id, 0, "search"),
            sample_tool_span(&run_id, 0, "read_file"),
            sample_tool_span(&run_id, 1, "write_file"),
        ],
        session_duration_ms: 5000,
        ..Default::default()
    };
    sink.on_run_end(&metrics);

    drop(sink);
    provider.shutdown().expect("provider shutdown failed");
}
