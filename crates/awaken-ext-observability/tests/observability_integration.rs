//! Integration tests for the awaken-ext-observability crate.
//!
//! Tests the public API: AgentMetrics aggregation, InMemorySink lifecycle,
//! MetricsSink trait, GenAISpan/ToolSpan types, and Plugin construction.
//! Internal phase hooks are tested in the crate's unit tests; these tests
//! exercise cross-type integration through the public surface.

use awaken_ext_observability::{
    AgentMetrics, CompositeSink, DelegationSpan, GenAISpan, HandoffSpan, InMemorySink,
    MetricsEvent, MetricsSink, ObservabilityPlugin, SpanContext, SuspensionSpan, ToolSpan,
};
use awaken_runtime::plugins::Plugin;

// ── GenAISpan and ToolSpan construction helpers ────────────────────

fn make_genai_span(model: &str, provider: &str, input: i32, output: i32) -> GenAISpan {
    GenAISpan {
        context: SpanContext::default(),
        step_index: None,
        model: model.into(),
        provider: provider.into(),
        operation: "chat".into(),
        response_model: None,
        response_id: None,
        finish_reasons: vec![],
        error_type: None,
        error_class: None,
        thinking_tokens: None,
        input_tokens: Some(input),
        output_tokens: Some(output),
        total_tokens: Some(input + output),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        temperature: None,
        top_p: None,
        max_tokens: None,
        stop_sequences: vec![],
        duration_ms: 100,
    }
}

fn make_genai_span_with_cache(
    model: &str,
    provider: &str,
    input: i32,
    output: i32,
    cache_read: i32,
    cache_creation: i32,
) -> GenAISpan {
    GenAISpan {
        cache_read_input_tokens: Some(cache_read),
        cache_creation_input_tokens: Some(cache_creation),
        ..make_genai_span(model, provider, input, output)
    }
}

fn make_genai_error_span(model: &str, provider: &str, error_type: &str) -> GenAISpan {
    GenAISpan {
        error_type: Some(error_type.into()),
        error_class: Some(error_type.into()),
        input_tokens: None,
        output_tokens: None,
        total_tokens: None,
        ..make_genai_span(model, provider, 0, 0)
    }
}

fn make_tool_span(name: &str, error: bool) -> ToolSpan {
    ToolSpan {
        context: SpanContext::default(),
        step_index: None,
        name: name.into(),
        operation: "execute_tool".into(),
        call_id: format!("call_{name}"),
        tool_type: "function".into(),
        error_type: if error {
            Some("tool_error".into())
        } else {
            None
        },
        duration_ms: 50,
    }
}

// ── Plugin construction and descriptor ─────────────────────────────

#[test]
fn plugin_descriptor_name_is_observability() {
    let plugin = ObservabilityPlugin::new(InMemorySink::new());
    assert_eq!(plugin.descriptor().name, "observability");
}

#[test]
fn plugin_builder_chaining() {
    let sink = InMemorySink::new();
    let _plugin = ObservabilityPlugin::new(sink)
        .with_model("claude-3.5-sonnet")
        .with_provider("anthropic")
        .with_temperature(0.7)
        .with_top_p(0.9)
        .with_max_tokens(4096)
        .with_stop_sequences(vec!["STOP".into(), "END".into()]);
}

// ── InMemorySink lifecycle ─────────────────────────────────────────

#[test]
fn sink_starts_empty() {
    let sink = InMemorySink::new();
    let m = sink.metrics();
    assert!(m.inferences.is_empty());
    assert!(m.tools.is_empty());
    assert_eq!(m.session_duration_ms, 0);
}

#[test]
fn sink_records_inferences_via_trait() {
    let sink = InMemorySink::new();
    sink.record(&MetricsEvent::Inference(make_genai_span(
        "gpt-4", "openai", 100, 50,
    )));
    sink.record(&MetricsEvent::Inference(make_genai_span(
        "gpt-4", "openai", 200, 75,
    )));

    let m = sink.metrics();
    assert_eq!(m.inference_count(), 2);
    assert_eq!(m.total_input_tokens(), 300);
    assert_eq!(m.total_output_tokens(), 125);
    assert_eq!(m.total_tokens(), 425);
}

#[test]
fn sink_records_tools_via_trait() {
    let sink = InMemorySink::new();
    sink.record(&MetricsEvent::Tool(make_tool_span("read_file", false)));
    sink.record(&MetricsEvent::Tool(make_tool_span("write_file", true)));
    sink.record(&MetricsEvent::Tool(make_tool_span("search", false)));

    let m = sink.metrics();
    assert_eq!(m.tool_count(), 3);
    assert_eq!(m.tool_failures(), 1);
}

#[test]
fn sink_records_session_duration_on_run_end() {
    let sink = InMemorySink::new();
    sink.record(&MetricsEvent::Inference(make_genai_span("m", "p", 10, 5)));
    sink.record(&MetricsEvent::Tool(make_tool_span("t", false)));

    let run_metrics = AgentMetrics {
        inferences: vec![],
        tools: vec![],
        session_duration_ms: 5000,
        ..Default::default()
    };
    sink.on_run_end(&run_metrics);

    let m = sink.metrics();
    assert_eq!(m.session_duration_ms, 5000);
    // Previously recorded spans are preserved
    assert_eq!(m.inference_count(), 1);
    assert_eq!(m.tool_count(), 1);
}

#[test]
fn sink_clone_shares_underlying_state() {
    let sink = InMemorySink::new();
    let clone = sink.clone();

    sink.record(&MetricsEvent::Inference(make_genai_span(
        "gpt-4", "openai", 100, 50,
    )));

    let m_from_clone = clone.metrics();
    assert_eq!(m_from_clone.inference_count(), 1);
    assert_eq!(m_from_clone.total_input_tokens(), 100);

    // Writing to clone is also visible from original
    clone.record(&MetricsEvent::Tool(make_tool_span("search", false)));
    let m_from_original = sink.metrics();
    assert_eq!(m_from_original.tool_count(), 1);
}

// ── Full simulation: inference + tool lifecycle through sink ───────

#[test]
fn full_agent_session_simulation() {
    let sink = InMemorySink::new();

    // Inference round 1
    sink.record(&MetricsEvent::Inference(make_genai_span(
        "gpt-4", "openai", 100, 50,
    )));

    // Tool calls from round 1
    sink.record(&MetricsEvent::Tool(make_tool_span("search", false)));
    sink.record(&MetricsEvent::Tool(make_tool_span("read_file", false)));

    // Inference round 2 (after tool results)
    sink.record(&MetricsEvent::Inference(make_genai_span(
        "gpt-4", "openai", 200, 75,
    )));

    // Tool call from round 2 (error)
    sink.record(&MetricsEvent::Tool(make_tool_span("write_file", true)));

    // Inference round 3 (final)
    sink.record(&MetricsEvent::Inference(make_genai_span(
        "gpt-4", "openai", 150, 40,
    )));

    // Session end
    let final_metrics = AgentMetrics {
        inferences: vec![],
        tools: vec![],
        session_duration_ms: 3000,
        ..Default::default()
    };
    sink.on_run_end(&final_metrics);

    let m = sink.metrics();
    assert_eq!(m.inference_count(), 3);
    assert_eq!(m.tool_count(), 3);
    assert_eq!(m.tool_failures(), 1);
    assert_eq!(m.total_input_tokens(), 450);
    assert_eq!(m.total_output_tokens(), 165);
    assert_eq!(m.total_tokens(), 615);
    assert_eq!(m.session_duration_ms, 3000);
}

// ── AgentMetrics aggregation: stats_by_model ───────────────────────

#[test]
fn stats_by_model_groups_by_model_and_provider() {
    let m = AgentMetrics {
        inferences: vec![
            make_genai_span("gpt-4", "openai", 100, 50),
            make_genai_span("gpt-4", "openai", 200, 75),
            make_genai_span("claude-3", "anthropic", 150, 60),
        ],
        tools: vec![],
        session_duration_ms: 0,
        ..Default::default()
    };

    let stats = m.stats_by_model();
    assert_eq!(stats.len(), 2);

    // Sorted alphabetically by model name
    assert_eq!(stats[0].model, "claude-3");
    assert_eq!(stats[0].provider, "anthropic");
    assert_eq!(stats[0].inference_count, 1);
    assert_eq!(stats[0].input_tokens, 150);
    assert_eq!(stats[0].output_tokens, 60);

    assert_eq!(stats[1].model, "gpt-4");
    assert_eq!(stats[1].provider, "openai");
    assert_eq!(stats[1].inference_count, 2);
    assert_eq!(stats[1].input_tokens, 300);
    assert_eq!(stats[1].output_tokens, 125);
    assert_eq!(stats[1].total_duration_ms, 200);
}

#[test]
fn stats_by_model_includes_cache_tokens() {
    let m = AgentMetrics {
        inferences: vec![
            make_genai_span_with_cache("gpt-4", "openai", 100, 50, 20, 10),
            make_genai_span_with_cache("gpt-4", "openai", 200, 75, 30, 15),
        ],
        tools: vec![],
        session_duration_ms: 0,
        ..Default::default()
    };

    let stats = m.stats_by_model();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].cache_read_input_tokens, 50);
    assert_eq!(stats[0].cache_creation_input_tokens, 25);

    // Also via top-level aggregation
    assert_eq!(m.total_cache_read_tokens(), 50);
    assert_eq!(m.total_cache_creation_tokens(), 25);
}

#[test]
fn stats_by_model_empty_returns_empty() {
    let m = AgentMetrics::default();
    assert!(m.stats_by_model().is_empty());
}

// ── AgentMetrics aggregation: stats_by_tool ────────────────────────

#[test]
fn stats_by_tool_groups_and_counts_failures() {
    let m = AgentMetrics {
        inferences: vec![],
        tools: vec![
            make_tool_span("search", false),
            make_tool_span("search", false),
            make_tool_span("write", true),
            make_tool_span("read", false),
            make_tool_span("write", false),
        ],
        session_duration_ms: 0,
        ..Default::default()
    };

    let stats = m.stats_by_tool();
    assert_eq!(stats.len(), 3);

    let search = stats.iter().find(|s| s.name == "search").unwrap();
    assert_eq!(search.call_count, 2);
    assert_eq!(search.failure_count, 0);
    assert_eq!(search.total_duration_ms, 100);

    let write = stats.iter().find(|s| s.name == "write").unwrap();
    assert_eq!(write.call_count, 2);
    assert_eq!(write.failure_count, 1);

    let read = stats.iter().find(|s| s.name == "read").unwrap();
    assert_eq!(read.call_count, 1);
    assert_eq!(read.failure_count, 0);
}

#[test]
fn stats_by_tool_empty_returns_empty() {
    let m = AgentMetrics::default();
    assert!(m.stats_by_tool().is_empty());
}

// ── AgentMetrics duration aggregation ──────────────────────────────

#[test]
fn total_durations_accumulate() {
    let m = AgentMetrics {
        inferences: vec![
            GenAISpan {
                duration_ms: 200,
                ..make_genai_span("m", "p", 10, 5)
            },
            GenAISpan {
                duration_ms: 300,
                ..make_genai_span("m", "p", 10, 5)
            },
        ],
        tools: vec![
            ToolSpan {
                duration_ms: 50,
                ..make_tool_span("t1", false)
            },
            ToolSpan {
                duration_ms: 75,
                ..make_tool_span("t2", false)
            },
        ],
        session_duration_ms: 1000,
        ..Default::default()
    };

    assert_eq!(m.total_inference_duration_ms(), 500);
    assert_eq!(m.total_tool_duration_ms(), 125);
}

// ── AgentMetrics with error spans ──────────────────────────────────

#[test]
fn error_spans_counted_in_inference_count_but_no_tokens() {
    let m = AgentMetrics {
        inferences: vec![
            make_genai_span("gpt-4", "openai", 100, 50),
            make_genai_error_span("gpt-4", "openai", "rate_limit"),
        ],
        tools: vec![],
        session_duration_ms: 0,
        ..Default::default()
    };

    assert_eq!(m.inference_count(), 2);
    assert_eq!(m.total_input_tokens(), 100);
    assert_eq!(m.total_output_tokens(), 50);

    let stats = m.stats_by_model();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].inference_count, 2);
    assert_eq!(stats[0].input_tokens, 100);
}

// ── AgentMetrics serde roundtrip ───────────────────────────────────

#[test]
fn agent_metrics_serde_roundtrip() {
    let metrics = AgentMetrics {
        inferences: vec![
            make_genai_span("gpt-4", "openai", 100, 50),
            make_genai_span("claude-3", "anthropic", 200, 75),
        ],
        tools: vec![
            make_tool_span("search", false),
            make_tool_span("write", true),
        ],
        session_duration_ms: 5000,
        ..Default::default()
    };

    let json = serde_json::to_string(&metrics).unwrap();
    let restored: AgentMetrics = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.inference_count(), 2);
    assert_eq!(restored.tool_count(), 2);
    assert_eq!(restored.tool_failures(), 1);
    assert_eq!(restored.session_duration_ms, 5000);
    assert_eq!(restored.total_input_tokens(), 300);
    assert_eq!(restored.total_output_tokens(), 125);
}

#[test]
fn genai_span_serde_roundtrip() {
    let span = GenAISpan {
        response_model: Some("gpt-4-0125".into()),
        response_id: Some("resp_123".into()),
        finish_reasons: vec!["end_turn".into()],
        thinking_tokens: Some(100),
        temperature: Some(0.7),
        top_p: Some(0.9),
        max_tokens: Some(4096),
        stop_sequences: vec!["STOP".into()],
        ..make_genai_span("gpt-4", "openai", 100, 50)
    };

    let json = serde_json::to_string(&span).unwrap();
    let restored: GenAISpan = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.model, "gpt-4");
    assert_eq!(restored.response_model.as_deref(), Some("gpt-4-0125"));
    assert_eq!(restored.thinking_tokens, Some(100));
    assert_eq!(restored.temperature, Some(0.7));
    assert_eq!(restored.stop_sequences, vec!["STOP"]);
}

#[test]
fn tool_span_serde_roundtrip() {
    let span = make_tool_span("search", false);
    let json = serde_json::to_string(&span).unwrap();
    let restored: ToolSpan = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.name, "search");
    assert!(restored.is_success());
    assert_eq!(restored.duration_ms, 50);
}

#[test]
fn tool_span_is_success_reflects_error_type() {
    let success = make_tool_span("search", false);
    assert!(success.is_success());

    let failure = make_tool_span("write", true);
    assert!(!failure.is_success());
    assert_eq!(failure.error_type.as_deref(), Some("tool_error"));
}

// ── Edge cases ─────────────────────────────────────────────────────

#[test]
fn all_none_token_fields_sum_to_zero() {
    let m = AgentMetrics {
        inferences: vec![GenAISpan {
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            ..make_genai_span("m", "p", 0, 0)
        }],
        tools: vec![],
        session_duration_ms: 0,
        ..Default::default()
    };

    assert_eq!(m.total_input_tokens(), 0);
    assert_eq!(m.total_output_tokens(), 0);
    assert_eq!(m.total_tokens(), 0);
}

#[test]
fn zero_duration_spans_handled() {
    let m = AgentMetrics {
        inferences: vec![GenAISpan {
            duration_ms: 0,
            ..make_genai_span("m", "p", 10, 5)
        }],
        tools: vec![ToolSpan {
            duration_ms: 0,
            ..make_tool_span("t", false)
        }],
        session_duration_ms: 0,
        ..Default::default()
    };

    assert_eq!(m.total_inference_duration_ms(), 0);
    assert_eq!(m.total_tool_duration_ms(), 0);
}

#[test]
fn default_metrics_are_all_zero() {
    let m = AgentMetrics::default();
    assert_eq!(m.total_input_tokens(), 0);
    assert_eq!(m.total_output_tokens(), 0);
    assert_eq!(m.total_tokens(), 0);
    assert_eq!(m.total_cache_read_tokens(), 0);
    assert_eq!(m.total_cache_creation_tokens(), 0);
    assert_eq!(m.total_inference_duration_ms(), 0);
    assert_eq!(m.total_tool_duration_ms(), 0);
    assert_eq!(m.inference_count(), 0);
    assert_eq!(m.tool_count(), 0);
    assert_eq!(m.tool_failures(), 0);
    assert!(m.stats_by_model().is_empty());
    assert!(m.stats_by_tool().is_empty());
}

// ── CompositeSink integration ─────────────────────────────────────

#[test]
fn composite_sink_with_in_memory_captures_all_events() {
    use std::sync::Arc;

    let sink_a = Arc::new(InMemorySink::new());
    let sink_b = Arc::new(InMemorySink::new());
    let composite = CompositeSink::new(vec![sink_a.clone(), sink_b.clone()]);

    // Inference
    composite.record(&MetricsEvent::Inference(make_genai_span(
        "gpt-4", "openai", 100, 50,
    )));
    composite.record(&MetricsEvent::Inference(make_genai_span(
        "claude-3",
        "anthropic",
        200,
        75,
    )));

    // Tools
    composite.record(&MetricsEvent::Tool(make_tool_span("search", false)));
    composite.record(&MetricsEvent::Tool(make_tool_span("write", true)));

    // Suspension
    composite.record(&MetricsEvent::Suspension(SuspensionSpan {
        context: SpanContext::default(),
        tool_call_id: "c1".into(),
        tool_name: "search".into(),
        action: "suspended".into(),
        resume_mode: None,
        duration_ms: None,
        timestamp_ms: 1000,
    }));

    // Handoff
    composite.record(&MetricsEvent::Handoff(HandoffSpan {
        context: SpanContext::default(),
        from_agent_id: "agent-a".into(),
        to_agent_id: "agent-b".into(),
        reason: Some("escalation".into()),
        timestamp_ms: 2000,
    }));

    // Delegation
    composite.record(&MetricsEvent::Delegation(DelegationSpan {
        context: SpanContext::default(),
        parent_run_id: "run-1".into(),
        child_run_id: Some("run-2".into()),
        target_agent_id: "worker".into(),
        tool_call_id: "c1".into(),
        duration_ms: Some(500),
        success: true,
        error_message: None,
        timestamp_ms: 3000,
    }));

    // Run end
    composite.on_run_end(&AgentMetrics {
        session_duration_ms: 5000,
        ..Default::default()
    });

    // Verify both sinks received all events
    for sink in [&sink_a, &sink_b] {
        let m = sink.metrics();
        assert_eq!(m.inference_count(), 2);
        assert_eq!(m.tool_count(), 2);
        assert_eq!(m.tool_failures(), 1);
        assert_eq!(m.total_suspensions(), 1);
        assert_eq!(m.total_handoffs(), 1);
        assert_eq!(m.total_delegations(), 1);
        assert_eq!(m.successful_delegations(), 1);
        assert_eq!(m.total_input_tokens(), 300);
        assert_eq!(m.total_output_tokens(), 125);
        assert_eq!(m.session_duration_ms, 5000);
    }

    // Flush and shutdown should succeed
    assert!(composite.flush().is_ok());
    assert!(composite.shutdown().is_ok());
}
