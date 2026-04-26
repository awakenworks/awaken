// Phoenix/OTLP observability integration tests.
//
// Two layers of coverage live in this file:
//
// 1. Legacy smoke tests (preserved unchanged for 0.4 back-compat).  They
//    require a Phoenix instance and only verify that exporting spans does
//    not panic and that the provider shuts down cleanly:
//      docker run -p 6006:6006 -p 4318:4318 arizephoenix/phoenix
//      OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:6006 \
//        cargo test -p awaken-server --test phoenix_observability_e2e -- --ignored
//
// 2. Helper-driven verification tests (new in 0.4.x).  They use the
//    `phoenix-test-helpers` crate to poll Phoenix's REST API and assert
//    that exported spans actually round-trip with the expected
//    GenAI-semconv attributes.  Boot via:
//      ./scripts/e2e-phoenix.sh
//
// In-memory OTLP span verification (no external infra) lives in:
//   crates/awaken-ext-observability/src/otel.rs (unit tests behind `otel` feature)

use awaken_ext_observability::otel::init_otlp_tracer;
use awaken_ext_observability::{
    AgentMetrics, GenAISpan, MetricsEvent, MetricsSink, OtelConfig, OtelMetricsSink, SpanContext,
    ToolSpan,
};
use phoenix_test_helpers::{
    PhoenixConfig, attr_str, ensure_phoenix_healthy, setup_otel_provider, tracer_for,
    unique_suffix, wait_for_chat_span, wait_for_span, wait_for_span_with_model,
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

// ---------------------------------------------------------------------------
// Helper-driven REST verification tests (new in 0.4.x)
// ---------------------------------------------------------------------------

/// Skip the test gracefully when Phoenix is not reachable.
async fn require_phoenix(cfg: &PhoenixConfig) -> bool {
    if !cfg.is_configured() {
        eprintln!("[phoenix-e2e] PhoenixConfig not configured, skipping");
        return false;
    }
    if !ensure_phoenix_healthy(&cfg.base_url).await {
        eprintln!(
            "[phoenix-e2e] Phoenix not healthy at {}, skipping (boot via scripts/e2e-phoenix.sh)",
            cfg.base_url
        );
        return false;
    }
    true
}

fn unique_model() -> String {
    format!("awaken-phoenix-test-{}", unique_suffix())
}

fn build_inference_span(model: &str, run_id: &str) -> GenAISpan {
    GenAISpan {
        context: SpanContext {
            run_id: run_id.to_string(),
            thread_id: "thread-phoenix-helpers".to_string(),
            agent_id: "agent-phoenix-helpers".to_string(),
            parent_run_id: None,
        },
        step_index: Some(0),
        model: model.to_string(),
        provider: "openai".to_string(),
        operation: "chat".to_string(),
        response_model: Some(model.to_string()),
        response_id: Some(format!("phoenix-helpers-{}", unique_suffix())),
        finish_reasons: vec!["stop".to_string()],
        error_type: None,
        error_class: None,
        thinking_tokens: None,
        input_tokens: Some(120),
        output_tokens: Some(45),
        total_tokens: Some(165),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        temperature: Some(0.5),
        top_p: None,
        max_tokens: Some(2048),
        stop_sequences: Vec::new(),
        duration_ms: 800,
    }
}

fn build_tool_span(name: &str, run_id: &str) -> ToolSpan {
    ToolSpan {
        context: SpanContext {
            run_id: run_id.to_string(),
            thread_id: "thread-phoenix-helpers".to_string(),
            agent_id: "agent-phoenix-helpers".to_string(),
            parent_run_id: None,
        },
        step_index: Some(0),
        name: name.to_string(),
        operation: "execute_tool".to_string(),
        call_id: format!("call-{}-{}", name, unique_suffix()),
        tool_type: "function".to_string(),
        error_type: None,
        duration_ms: 75,
    }
}

#[ignore = "requires running Phoenix: ./scripts/e2e-phoenix.sh"]
#[tokio::test]
async fn phoenix_via_helpers_chat_span_attributes() {
    let cfg = PhoenixConfig::from_env();
    if !require_phoenix(&cfg).await {
        return;
    }

    let model = unique_model();
    let provider = setup_otel_provider(&cfg.otlp_traces_endpoint, "awaken-e2e-helpers")
        .expect("init OTLP provider");
    let tracer = tracer_for(&provider, "awaken-e2e-helpers");

    let sink = OtelMetricsSink::new(tracer);
    let run_id = format!("phoenix-helpers-chat-{}", unique_suffix());

    sink.record(&MetricsEvent::Inference(build_inference_span(
        &model, &run_id,
    )));
    sink.on_run_end(&AgentMetrics {
        inferences: vec![build_inference_span(&model, &run_id)],
        session_duration_ms: 800,
        ..Default::default()
    });

    drop(sink);
    provider.force_flush().expect("force_flush");

    let span = wait_for_chat_span(&cfg.project_spans_url, &model)
        .await
        .expect("phoenix returned the chat span we just exported");

    assert_eq!(
        attr_str(&span, "gen_ai.request.model"),
        Some(model.as_str())
    );
    assert_eq!(attr_str(&span, "gen_ai.system"), Some("openai"));
    assert_eq!(attr_str(&span, "gen_ai.operation.name"), Some("chat"));

    provider.shutdown().expect("provider shutdown");
}

#[ignore = "requires running Phoenix: ./scripts/e2e-phoenix.sh"]
#[tokio::test]
async fn phoenix_via_helpers_error_span_status() {
    let cfg = PhoenixConfig::from_env();
    if !require_phoenix(&cfg).await {
        return;
    }

    let model = unique_model();
    let provider = setup_otel_provider(&cfg.otlp_traces_endpoint, "awaken-e2e-helpers-err")
        .expect("init OTLP provider");
    let tracer = tracer_for(&provider, "awaken-e2e-helpers-err");

    let sink = OtelMetricsSink::new(tracer);
    let run_id = format!("phoenix-helpers-err-{}", unique_suffix());

    let mut errored = build_inference_span(&model, &run_id);
    errored.error_type = Some("rate_limit".to_string());
    errored.error_class = Some("rate_limit".to_string());
    sink.record(&MetricsEvent::Inference(errored.clone()));

    sink.on_run_end(&AgentMetrics {
        inferences: vec![errored],
        ..Default::default()
    });

    drop(sink);
    provider.force_flush().expect("force_flush");

    let span = wait_for_span_with_model(&cfg.project_spans_url, &model)
        .await
        .expect("phoenix returned the errored span");

    assert_eq!(attr_str(&span, "error.type"), Some("rate_limit"));

    provider.shutdown().expect("provider shutdown");
}

#[ignore = "requires running Phoenix: ./scripts/e2e-phoenix.sh"]
#[tokio::test]
async fn phoenix_via_helpers_tool_span_correlated() {
    let cfg = PhoenixConfig::from_env();
    if !require_phoenix(&cfg).await {
        return;
    }

    let model = unique_model();
    let provider = setup_otel_provider(&cfg.otlp_traces_endpoint, "awaken-e2e-helpers-tool")
        .expect("init OTLP provider");
    let tracer = tracer_for(&provider, "awaken-e2e-helpers-tool");

    let sink = OtelMetricsSink::new(tracer);
    let run_id = format!("phoenix-helpers-tool-{}", unique_suffix());
    let tool_name = format!("phoenix-tool-{}", unique_suffix());

    sink.record(&MetricsEvent::Inference(build_inference_span(
        &model, &run_id,
    )));
    sink.record(&MetricsEvent::Tool(build_tool_span(&tool_name, &run_id)));
    sink.on_run_end(&AgentMetrics {
        inferences: vec![build_inference_span(&model, &run_id)],
        tools: vec![build_tool_span(&tool_name, &run_id)],
        session_duration_ms: 900,
        ..Default::default()
    });

    drop(sink);
    provider.force_flush().expect("force_flush");

    let span = wait_for_span(&cfg.project_spans_url, |span| {
        attr_str(span, "gen_ai.tool.name") == Some(tool_name.as_str())
    })
    .await
    .expect("phoenix returned the tool span");

    assert_eq!(
        attr_str(&span, "gen_ai.tool.name"),
        Some(tool_name.as_str())
    );
    assert_eq!(
        attr_str(&span, "gen_ai.operation.name"),
        Some("execute_tool")
    );

    provider.shutdown().expect("provider shutdown");
}

#[ignore = "requires running Phoenix: ./scripts/e2e-phoenix.sh"]
#[tokio::test]
async fn phoenix_via_helpers_run_context_propagated() {
    let cfg = PhoenixConfig::from_env();
    if !require_phoenix(&cfg).await {
        return;
    }

    let model = unique_model();
    let provider = setup_otel_provider(&cfg.otlp_traces_endpoint, "awaken-e2e-helpers-ctx")
        .expect("init OTLP provider");
    let tracer = tracer_for(&provider, "awaken-e2e-helpers-ctx");

    let sink = OtelMetricsSink::new(tracer);
    let run_id = format!("phoenix-helpers-ctx-{}", unique_suffix());

    sink.record(&MetricsEvent::Inference(build_inference_span(
        &model, &run_id,
    )));
    sink.on_run_end(&AgentMetrics::default());

    drop(sink);
    provider.force_flush().expect("force_flush");

    let span = wait_for_chat_span(&cfg.project_spans_url, &model)
        .await
        .expect("phoenix returned the run-context span");

    assert_eq!(attr_str(&span, "run.id"), Some(run_id.as_str()));
    assert_eq!(attr_str(&span, "thread.id"), Some("thread-phoenix-helpers"));
    assert_eq!(attr_str(&span, "agent.id"), Some("agent-phoenix-helpers"));

    provider.shutdown().expect("provider shutdown");
}
