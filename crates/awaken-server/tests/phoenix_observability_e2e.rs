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
    AgentMetrics, BackgroundTaskSpan, DelegationSpan, EvaluationResultEvent, GenAISpan,
    MetricsEvent, MetricsSink, OtelConfig, OtelMetricsSink, SpanContext, ToolSpan,
};
use phoenix_test_helpers::{
    PhoenixConfig, attr_str, ensure_phoenix_healthy, setup_otel_provider, tracer_for,
    unique_suffix, wait_for_chat_span, wait_for_span,
};
use serde_json::json;

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
            parent_tool_call_id: None,
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
        started_at_ms: 0,
        ended_at_ms: 0,
    }
}

fn sample_tool_span(run_id: &str, step: u32, name: &str) -> ToolSpan {
    ToolSpan {
        context: SpanContext {
            run_id: run_id.to_string(),
            thread_id: "thread-phoenix-test".to_string(),
            agent_id: "agent-phoenix-test".to_string(),
            parent_run_id: None,
            parent_tool_call_id: None,
        },
        step_index: Some(step),
        name: name.to_string(),
        operation: "execute_tool".to_string(),
        call_id: format!("call_{name}_{step}"),
        tool_type: "function".to_string(),
        call_arguments: None,
        call_result: None,
        error_type: None,
        duration_ms: 120,
        started_at_ms: 0,
        ended_at_ms: 0,
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
            parent_tool_call_id: None,
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
        started_at_ms: 0,
        ended_at_ms: 0,
    }
}

fn build_tool_span(name: &str, run_id: &str) -> ToolSpan {
    ToolSpan {
        context: SpanContext {
            run_id: run_id.to_string(),
            thread_id: "thread-phoenix-helpers".to_string(),
            agent_id: "agent-phoenix-helpers".to_string(),
            parent_run_id: None,
            parent_tool_call_id: None,
        },
        step_index: Some(0),
        name: name.to_string(),
        operation: "execute_tool".to_string(),
        call_id: format!("call-{}-{}", name, unique_suffix()),
        tool_type: "function".to_string(),
        call_arguments: None,
        call_result: None,
        error_type: None,
        duration_ms: 75,
        started_at_ms: 0,
        ended_at_ms: 0,
    }
}

fn build_background_task_span(
    run_id: &str,
    task_id: &str,
    parent_tool_call_id: &str,
    status: &str,
    created_at_ms: u64,
    completed_at_ms: Option<u64>,
) -> BackgroundTaskSpan {
    BackgroundTaskSpan {
        context: SpanContext {
            run_id: run_id.to_string(),
            thread_id: "thread-phoenix-helpers".to_string(),
            agent_id: "agent-phoenix-helpers".to_string(),
            parent_run_id: None,
            parent_tool_call_id: Some(parent_tool_call_id.to_string()),
        },
        task_id: task_id.to_string(),
        task_type: "sub_agent".to_string(),
        task_name: Some("worker".to_string()),
        description: "background worker".to_string(),
        status: status.to_string(),
        parent_task_id: None,
        error_message: None,
        created_at_ms,
        completed_at_ms,
    }
}

fn value_contains_string(value: &serde_json::Value, needle: &str) -> bool {
    match value {
        serde_json::Value::String(text) => text.contains(needle),
        serde_json::Value::Array(items) => {
            items.iter().any(|item| value_contains_string(item, needle))
        }
        serde_json::Value::Object(map) => map
            .iter()
            .any(|(key, value)| key.contains(needle) || value_contains_string(value, needle)),
        _ => false,
    }
}

fn span_trace_id(span: &serde_json::Value) -> Option<&str> {
    span.get("context")?.get("trace_id")?.as_str()
}

fn span_id(span: &serde_json::Value) -> Option<&str> {
    span.get("context")?.get("span_id")?.as_str()
}

fn span_parent_id(span: &serde_json::Value) -> Option<&str> {
    span.get("parent_id").and_then(serde_json::Value::as_str)
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
    assert_eq!(attr_str(&span, "gen_ai.provider.name"), Some("openai"));
    assert_eq!(attr_str(&span, "gen_ai.operation.name"), Some("chat"));
    assert_eq!(
        attr_str(&span, "gen_ai.conversation.id"),
        Some("thread-phoenix-helpers")
    );

    let agent_span = wait_for_span(&cfg.project_spans_url, |span| {
        attr_str(span, "gen_ai.request.model") == Some(model.as_str())
            && attr_str(span, "gen_ai.operation.name") == Some("invoke_agent")
    })
    .await
    .expect("phoenix returned the invoke_agent span");
    assert_eq!(
        attr_str(&agent_span, "gen_ai.agent.id"),
        Some("agent-phoenix-helpers")
    );
    assert_eq!(
        attr_str(&agent_span, "gen_ai.conversation.id"),
        Some("thread-phoenix-helpers")
    );
    assert_eq!(
        attr_str(&agent_span, "gen_ai.provider.name"),
        Some("openai")
    );

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

    let span = wait_for_chat_span(&cfg.project_spans_url, &model)
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
    let tool_span = ToolSpan {
        call_arguments: Some(json!({"query": "otel genai", "limit": 2})),
        call_result: Some(json!({"count": 2, "ok": true})),
        ..build_tool_span(&tool_name, &run_id)
    };

    sink.record(&MetricsEvent::Inference(build_inference_span(
        &model, &run_id,
    )));
    sink.record(&MetricsEvent::Tool(tool_span.clone()));
    sink.on_run_end(&AgentMetrics {
        inferences: vec![build_inference_span(&model, &run_id)],
        tools: vec![tool_span],
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
    let arguments = attr_str(&span, "gen_ai.tool.call.arguments")
        .expect("Phoenix returned tool call arguments");
    let result =
        attr_str(&span, "gen_ai.tool.call.result").expect("Phoenix returned tool call result");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(arguments).expect("arguments JSON"),
        json!({"query": "otel genai", "limit": 2})
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(result).expect("result JSON"),
        json!({"count": 2, "ok": true})
    );

    provider.shutdown().expect("provider shutdown");
}

#[ignore = "requires running Phoenix: ./scripts/e2e-phoenix.sh"]
#[tokio::test]
async fn phoenix_via_helpers_delegation_span_correlated_with_agent_tool() {
    let cfg = PhoenixConfig::from_env();
    if !require_phoenix(&cfg).await {
        return;
    }

    let model = unique_model();
    let provider = setup_otel_provider(&cfg.otlp_traces_endpoint, "awaken-e2e-helpers-delegation")
        .expect("init OTLP provider");
    let tracer = tracer_for(&provider, "awaken-e2e-helpers-delegation");

    let sink = OtelMetricsSink::new(tracer);
    let run_id = format!("phoenix-helpers-delegation-{}", unique_suffix());
    let child_model = unique_model();
    let child_run_id = format!("phoenix-child-run-{}", unique_suffix());
    let tool_span = build_tool_span("agent_run_worker", &run_id);
    let tool_call_id = tool_span.call_id.clone();
    let child_inference = GenAISpan {
        context: SpanContext {
            run_id: child_run_id.clone(),
            thread_id: child_run_id.clone(),
            agent_id: "worker".to_string(),
            parent_run_id: Some(run_id.clone()),
            parent_tool_call_id: Some(tool_call_id.clone()),
        },
        step_index: Some(0),
        model: child_model.clone(),
        provider: "openai".to_string(),
        operation: "chat".to_string(),
        response_model: Some(child_model.clone()),
        response_id: Some(format!("phoenix-child-response-{}", unique_suffix())),
        finish_reasons: vec!["stop".to_string()],
        error_type: None,
        error_class: None,
        thinking_tokens: None,
        input_tokens: Some(30),
        output_tokens: Some(12),
        total_tokens: Some(42),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        temperature: Some(0.2),
        top_p: None,
        max_tokens: Some(512),
        stop_sequences: Vec::new(),
        duration_ms: 300,
        started_at_ms: 0,
        ended_at_ms: 0,
    };
    let delegation = DelegationSpan {
        context: tool_span.context.clone(),
        parent_run_id: run_id.clone(),
        child_run_id: Some(child_run_id.clone()),
        target_agent_id: "worker".to_string(),
        tool_call_id: tool_call_id.clone(),
        duration_ms: Some(250),
        success: true,
        error_message: None,
        timestamp_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_millis() as u64,
    };

    sink.record(&MetricsEvent::Inference(build_inference_span(
        &model, &run_id,
    )));
    sink.record(&MetricsEvent::Inference(child_inference.clone()));
    sink.on_run_end(&AgentMetrics {
        inferences: vec![child_inference],
        session_duration_ms: 300,
        ..Default::default()
    });
    sink.record(&MetricsEvent::Tool(tool_span.clone()));
    sink.record(&MetricsEvent::Delegation(delegation.clone()));
    sink.on_run_end(&AgentMetrics {
        inferences: vec![build_inference_span(&model, &run_id)],
        tools: vec![tool_span],
        delegations: vec![delegation],
        session_duration_ms: 950,
        ..Default::default()
    });

    drop(sink);
    provider.force_flush().expect("force_flush");

    let agent_span = wait_for_span(&cfg.project_spans_url, |span| {
        attr_str(span, "gen_ai.request.model") == Some(model.as_str())
            && attr_str(span, "gen_ai.operation.name") == Some("invoke_agent")
    })
    .await
    .expect("phoenix returned the parent invoke_agent span");
    let parent_chat = wait_for_span(&cfg.project_spans_url, |span| {
        attr_str(span, "gen_ai.request.model") == Some(model.as_str())
            && attr_str(span, "gen_ai.operation.name") == Some("chat")
    })
    .await
    .expect("phoenix returned the parent chat span");
    let child_agent_span = wait_for_span(&cfg.project_spans_url, |span| {
        attr_str(span, "gen_ai.request.model") == Some(child_model.as_str())
            && attr_str(span, "gen_ai.operation.name") == Some("invoke_agent")
            && attr_str(span, "awaken.run.id") == Some(child_run_id.as_str())
    })
    .await
    .expect("phoenix returned the child invoke_agent span");
    let child_chat = wait_for_span(&cfg.project_spans_url, |span| {
        attr_str(span, "gen_ai.request.model") == Some(child_model.as_str())
            && attr_str(span, "gen_ai.operation.name") == Some("chat")
    })
    .await
    .expect("phoenix returned the child chat span");
    let tool = wait_for_span(&cfg.project_spans_url, |span| {
        attr_str(span, "gen_ai.tool.call.id") == Some(tool_call_id.as_str())
            && attr_str(span, "gen_ai.tool.name") == Some("agent_run_worker")
    })
    .await
    .expect("phoenix returned the agent_run tool span");
    let delegation = wait_for_span(&cfg.project_spans_url, |span| {
        span.get("name").and_then(serde_json::Value::as_str) == Some("awaken.delegation")
            && attr_str(span, "gen_ai.tool.call.id") == Some(tool_call_id.as_str())
    })
    .await
    .expect("phoenix returned the delegation span");

    assert_eq!(
        attr_str(&delegation, "awaken.delegation.parent_run_id"),
        Some(run_id.as_str())
    );
    assert_eq!(
        attr_str(&delegation, "awaken.delegation.child_run_id"),
        Some(child_run_id.as_str())
    );
    assert_eq!(
        attr_str(&delegation, "awaken.delegation.target_agent_id"),
        Some("worker")
    );
    assert_eq!(
        delegation
            .get("attributes")
            .and_then(|attrs| attrs.get("awaken.delegation.success"))
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );

    assert_eq!(span_trace_id(&tool), span_trace_id(&agent_span));
    assert_eq!(span_trace_id(&delegation), span_trace_id(&agent_span));
    assert_eq!(span_trace_id(&child_agent_span), span_trace_id(&agent_span));
    assert_eq!(span_trace_id(&child_chat), span_trace_id(&agent_span));
    assert_eq!(span_parent_id(&tool), span_id(&parent_chat));
    assert_eq!(span_parent_id(&child_agent_span), span_id(&parent_chat));
    assert_eq!(span_parent_id(&child_chat), span_id(&child_agent_span));
    assert!(
        span_parent_id(&delegation).is_some(),
        "delegation span should remain attached to the exported trace tree"
    );

    provider.shutdown().expect("provider shutdown");
}

#[ignore = "requires running Phoenix: ./scripts/e2e-phoenix.sh"]
#[tokio::test]
async fn phoenix_via_helpers_background_task_span_correlated_with_parent_tool() {
    let cfg = PhoenixConfig::from_env();
    if !require_phoenix(&cfg).await {
        return;
    }

    let model = unique_model();
    let provider = setup_otel_provider(&cfg.otlp_traces_endpoint, "awaken-e2e-helpers-bg-task")
        .expect("init OTLP provider");
    let tracer = tracer_for(&provider, "awaken-e2e-helpers-bg-task");

    let sink = OtelMetricsSink::new(tracer);
    let run_id = format!("phoenix-helpers-bg-{}", unique_suffix());
    let tool_span = build_tool_span("spawn_background", &run_id);
    let tool_call_id = tool_span.call_id.clone();
    let task_id = format!("bg-phoenix-{}", unique_suffix());
    let created_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_millis() as u64;
    let running = build_background_task_span(
        &run_id,
        &task_id,
        &tool_call_id,
        "running",
        created_at_ms,
        None,
    );
    let completed = build_background_task_span(
        &run_id,
        &task_id,
        &tool_call_id,
        "completed",
        created_at_ms,
        Some(created_at_ms + 125),
    );

    let inference = build_inference_span(&model, &run_id);
    sink.record(&MetricsEvent::Inference(inference.clone()));
    sink.record(&MetricsEvent::Tool(tool_span.clone()));
    sink.record(&MetricsEvent::BackgroundTask(running.clone()));
    sink.record(&MetricsEvent::BackgroundTask(completed.clone()));
    sink.on_run_end(&AgentMetrics {
        inferences: vec![inference],
        tools: vec![tool_span],
        background_tasks: vec![completed],
        session_duration_ms: 950,
        ..Default::default()
    });

    drop(sink);
    provider.force_flush().expect("force_flush");

    let agent_span = wait_for_span(&cfg.project_spans_url, |span| {
        attr_str(span, "gen_ai.request.model") == Some(model.as_str())
            && attr_str(span, "gen_ai.operation.name") == Some("invoke_agent")
    })
    .await
    .expect("phoenix returned the parent invoke_agent span");
    let parent_chat = wait_for_span(&cfg.project_spans_url, |span| {
        attr_str(span, "gen_ai.request.model") == Some(model.as_str())
            && attr_str(span, "gen_ai.operation.name") == Some("chat")
    })
    .await
    .expect("phoenix returned the parent chat span");
    let tool = wait_for_span(&cfg.project_spans_url, |span| {
        attr_str(span, "gen_ai.tool.call.id") == Some(tool_call_id.as_str())
            && attr_str(span, "gen_ai.tool.name") == Some("spawn_background")
    })
    .await
    .expect("phoenix returned the spawn_background tool span");
    let background_task = wait_for_span(&cfg.project_spans_url, |span| {
        span.get("name").and_then(serde_json::Value::as_str) == Some("awaken.background_task")
            && attr_str(span, "awaken.background_task.id") == Some(task_id.as_str())
            && attr_str(span, "awaken.background_task.status") == Some("completed")
    })
    .await
    .expect("phoenix returned the background task span");

    assert_eq!(span_trace_id(&tool), span_trace_id(&agent_span));
    assert_eq!(span_trace_id(&background_task), span_trace_id(&agent_span));
    assert_eq!(span_parent_id(&tool), span_id(&parent_chat));
    assert_eq!(span_parent_id(&background_task), span_id(&tool));
    assert_eq!(
        attr_str(
            &background_task,
            "awaken.background_task.parent_tool_call_id"
        ),
        Some(tool_call_id.as_str())
    );

    provider.shutdown().expect("provider shutdown");
}

#[ignore = "requires running Phoenix: ./scripts/e2e-phoenix.sh"]
#[tokio::test]
async fn phoenix_via_helpers_background_subagent_run_nested_under_task() {
    use std::sync::Arc;

    use awaken_runtime::extensions::background::{
        BackgroundTaskManager, BackgroundTaskPlugin, TaskParentContext, TaskResult,
    };

    let cfg = PhoenixConfig::from_env();
    if !require_phoenix(&cfg).await {
        return;
    }

    let model = unique_model();
    let child_model = unique_model();
    let provider = setup_otel_provider(&cfg.otlp_traces_endpoint, "awaken-e2e-helpers-bg-subagent")
        .expect("init OTLP provider");
    let tracer = tracer_for(&provider, "awaken-e2e-helpers-bg-subagent");

    let sink = Arc::new(OtelMetricsSink::new(tracer));
    let store = awaken_runtime::StateStore::new();
    let manager = Arc::new(BackgroundTaskManager::new());
    manager.set_store(store.clone());
    store
        .install_plugin(BackgroundTaskPlugin::new(manager.clone()))
        .expect("background keys should register");

    let run_id = format!("phoenix-helpers-bg-subagent-{}", unique_suffix());
    let child_run_id = format!("phoenix-bg-child-run-{}", unique_suffix());
    let tool_span = build_tool_span("spawn_background", &run_id);
    let tool_call_id = tool_span.call_id.clone();
    let created_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_millis() as u64;
    let child_inference = GenAISpan {
        context: SpanContext {
            run_id: child_run_id.clone(),
            thread_id: child_run_id.clone(),
            agent_id: "worker".to_string(),
            parent_run_id: Some(run_id.clone()),
            parent_tool_call_id: Some(tool_call_id.clone()),
        },
        step_index: Some(0),
        model: child_model.clone(),
        provider: "openai".to_string(),
        operation: "chat".to_string(),
        response_model: Some(child_model.clone()),
        response_id: Some(format!("phoenix-bg-child-response-{}", unique_suffix())),
        finish_reasons: vec!["stop".to_string()],
        error_type: None,
        error_class: None,
        thinking_tokens: None,
        input_tokens: Some(25),
        output_tokens: Some(10),
        total_tokens: Some(35),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        temperature: Some(0.2),
        top_p: None,
        max_tokens: Some(512),
        stop_sequences: Vec::new(),
        duration_ms: 250,
        started_at_ms: 0,
        ended_at_ms: 0,
    };
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();

    let inference = build_inference_span(&model, &run_id);
    sink.record(&MetricsEvent::Inference(inference.clone()));
    // Background workers may start before the parent tool span is emitted.
    // The OTLP exporter should still materialize Tool -> BackgroundTask -> child
    // by reading the runtime task-local background context.
    let task_id = manager
        .spawn(
            &child_run_id,
            "sub_agent",
            Some("worker"),
            "worker agent",
            TaskParentContext {
                task_id: None,
                run_id: Some(run_id.clone()),
                call_id: Some(tool_call_id.clone()),
                agent_id: Some("agent-phoenix-helpers".to_string()),
            },
            {
                let sink = sink.clone();
                let child_inference = child_inference.clone();
                move |_ctx| async move {
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
        inferences: vec![child_inference],
        session_duration_ms: 250,
        ..Default::default()
    });
    sink.record(&MetricsEvent::Tool(tool_span.clone()));
    let completed = build_background_task_span(
        &run_id,
        &task_id,
        &tool_call_id,
        "completed",
        created_at_ms,
        Some(created_at_ms + 250),
    );
    sink.record(&MetricsEvent::BackgroundTask(completed.clone()));
    sink.on_run_end(&AgentMetrics {
        inferences: vec![inference],
        tools: vec![tool_span],
        background_tasks: vec![completed],
        session_duration_ms: 950,
        ..Default::default()
    });

    drop(sink);
    provider.force_flush().expect("force_flush");

    let tool = wait_for_span(&cfg.project_spans_url, |span| {
        attr_str(span, "gen_ai.tool.call.id") == Some(tool_call_id.as_str())
            && attr_str(span, "gen_ai.tool.name") == Some("spawn_background")
    })
    .await
    .expect("phoenix returned the spawn_background tool span");
    let background_task = wait_for_span(&cfg.project_spans_url, |span| {
        span.get("name").and_then(serde_json::Value::as_str) == Some("awaken.background_task")
            && attr_str(span, "awaken.background_task.id") == Some(task_id.as_str())
            && attr_str(span, "awaken.background_task.status") == Some("completed")
    })
    .await
    .expect("phoenix returned the background task span");
    let child_agent_span = wait_for_span(&cfg.project_spans_url, |span| {
        attr_str(span, "gen_ai.request.model") == Some(child_model.as_str())
            && attr_str(span, "gen_ai.operation.name") == Some("invoke_agent")
            && attr_str(span, "awaken.run.id") == Some(child_run_id.as_str())
    })
    .await
    .expect("phoenix returned the background child invoke_agent span");
    let child_chat = wait_for_span(&cfg.project_spans_url, |span| {
        attr_str(span, "gen_ai.request.model") == Some(child_model.as_str())
            && attr_str(span, "gen_ai.operation.name") == Some("chat")
    })
    .await
    .expect("phoenix returned the background child chat span");

    assert_eq!(span_trace_id(&background_task), span_trace_id(&tool));
    assert_eq!(span_trace_id(&child_agent_span), span_trace_id(&tool));
    assert_eq!(span_trace_id(&child_chat), span_trace_id(&tool));
    assert_eq!(span_parent_id(&background_task), span_id(&tool));
    assert_eq!(span_parent_id(&child_agent_span), span_id(&background_task));
    assert_eq!(span_parent_id(&child_chat), span_id(&child_agent_span));
    assert_eq!(
        attr_str(&child_agent_span, "awaken.parent_task.id"),
        Some(task_id.as_str())
    );

    provider.shutdown().expect("provider shutdown");
}

#[ignore = "requires running Phoenix: ./scripts/e2e-phoenix.sh"]
#[tokio::test]
async fn phoenix_via_helpers_evaluation_event_exported() {
    let cfg = PhoenixConfig::from_env();
    if !require_phoenix(&cfg).await {
        return;
    }

    let model = unique_model();
    let provider = setup_otel_provider(&cfg.otlp_traces_endpoint, "awaken-e2e-helpers-eval")
        .expect("init OTLP provider");
    let tracer = tracer_for(&provider, "awaken-e2e-helpers-eval");

    let sink = OtelMetricsSink::new(tracer);
    let run_id = format!("phoenix-helpers-eval-{}", unique_suffix());
    let inference = build_inference_span(&model, &run_id);
    let response_id = inference
        .response_id
        .clone()
        .expect("test inference includes response id");
    let event = EvaluationResultEvent {
        context: inference.context.clone(),
        name: "faithfulness".to_string(),
        score_label: Some("pass".to_string()),
        score_value: Some(1.0),
        explanation: Some("grounded in retrieved context".to_string()),
        response_id: Some(response_id.clone()),
        error_type: None,
        timestamp_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_millis() as u64,
    };

    sink.record(&MetricsEvent::Inference(inference.clone()));
    sink.record(&MetricsEvent::EvaluationResult(event.clone()));
    sink.on_run_end(&AgentMetrics {
        inferences: vec![inference],
        evaluations: vec![event],
        session_duration_ms: 900,
        ..Default::default()
    });

    drop(sink);
    provider.force_flush().expect("force_flush");

    let span = wait_for_span(&cfg.project_spans_url, |span| {
        attr_str(span, "gen_ai.request.model") == Some(model.as_str())
            && attr_str(span, "gen_ai.operation.name") == Some("chat")
            && value_contains_string(span, "gen_ai.evaluation.result")
            && value_contains_string(span, &response_id)
    })
    .await
    .expect("phoenix returned the evaluation event on the chat span");

    assert!(value_contains_string(&span, "gen_ai.evaluation.name"));
    assert!(value_contains_string(&span, "faithfulness"));
    assert!(value_contains_string(
        &span,
        "gen_ai.evaluation.score.label"
    ));
    assert!(value_contains_string(&span, "pass"));

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

    assert_eq!(attr_str(&span, "awaken.run.id"), Some(run_id.as_str()));
    assert_eq!(
        attr_str(&span, "awaken.thread.id"),
        Some("thread-phoenix-helpers")
    );
    assert_eq!(
        attr_str(&span, "awaken.agent.id"),
        Some("agent-phoenix-helpers")
    );
    assert_eq!(
        attr_str(&span, "gen_ai.conversation.id"),
        Some("thread-phoenix-helpers")
    );
    assert_eq!(
        attr_str(&span, "gen_ai.agent.id"),
        Some("agent-phoenix-helpers")
    );

    provider.shutdown().expect("provider shutdown");
}
