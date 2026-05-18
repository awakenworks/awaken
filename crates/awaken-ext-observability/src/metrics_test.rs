use super::*;

fn make_genai_span(model: &str, input: Option<i32>, output: Option<i32>) -> GenAISpan {
    GenAISpan {
        context: SpanContext::default(),
        step_index: None,
        model: model.to_string(),
        provider: "test".to_string(),
        operation: "chat".to_string(),
        response_model: None,
        response_id: None,
        finish_reasons: Vec::new(),
        error_type: None,
        error_class: None,
        thinking_tokens: None,
        input_tokens: input,
        output_tokens: output,
        total_tokens: match (input, output) {
            (Some(i), Some(o)) => Some(i + o),
            _ => None,
        },
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
    }
}

fn make_tool_span(name: &str, error: bool) -> ToolSpan {
    ToolSpan {
        context: SpanContext::default(),
        step_index: None,
        name: name.to_string(),
        operation: "execute_tool".to_string(),
        call_id: format!("call_{name}"),
        tool_type: "function".to_string(),
        call_arguments: None,
        call_result: None,
        error_type: if error {
            Some("tool_error".to_string())
        } else {
            None
        },
        duration_ms: 50,
        started_at_ms: 0,
        ended_at_ms: 0,
    }
}

fn make_background_task_span() -> BackgroundTaskSpan {
    BackgroundTaskSpan {
        context: SpanContext::default(),
        task_id: "task-1".to_string(),
        task_type: "sub_agent".to_string(),
        task_name: None,
        description: "background task".to_string(),
        status: TaskStatus::Running,
        parent_task_id: None,
        error_message: None,
        created_at_ms: 1_000,
        completed_at_ms: None,
    }
}

// ---- SpanContext serde roundtrip ----

#[test]
fn span_context_default_has_empty_fields() {
    let ctx = SpanContext::default();
    assert!(ctx.run_id.is_empty());
    assert!(ctx.thread_id.is_empty());
    assert!(ctx.agent_id.is_empty());
    assert!(ctx.parent_run_id.is_none());
    assert!(ctx.parent_tool_call_id.is_none());
}

#[test]
fn span_context_serde_roundtrip() {
    let ctx = SpanContext {
        run_id: "run-1".to_string(),
        thread_id: "thread-1".to_string(),
        agent_id: "agent-1".to_string(),
        parent_run_id: Some("parent-run-1".to_string()),
        parent_tool_call_id: Some("call-1".to_string()),
        ..Default::default()
    };
    let json = serde_json::to_string(&ctx).unwrap();
    let restored: SpanContext = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.run_id, "run-1");
    assert_eq!(restored.thread_id, "thread-1");
    assert_eq!(restored.agent_id, "agent-1");
    assert_eq!(restored.parent_run_id.as_deref(), Some("parent-run-1"));
    assert_eq!(restored.parent_tool_call_id.as_deref(), Some("call-1"));
}

#[test]
fn span_context_serde_skips_empty_fields() {
    let ctx = SpanContext::default();
    let json = serde_json::to_string(&ctx).unwrap();
    assert!(!json.contains("run_id"));
    assert!(!json.contains("thread_id"));
    assert!(!json.contains("agent_id"));
    assert!(!json.contains("parent_run_id"));
    assert!(!json.contains("parent_tool_call_id"));
}

// ---- AgentMetrics::default() ----

#[test]
fn default_returns_zeros() {
    let m = AgentMetrics::default();
    assert!(m.inferences.is_empty());
    assert!(m.tools.is_empty());
    assert!(m.background_tasks.is_empty());
    assert_eq!(m.session_duration_ms, 0);
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
}

#[test]
fn agent_metrics_deserializes_without_background_tasks() {
    let json = r#"{
            "inferences": [],
            "tools": [],
            "evaluations": [],
            "suspensions": [],
            "handoffs": [],
            "delegations": [],
            "session_duration_ms": 0
        }"#;

    let m: AgentMetrics = serde_json::from_str(json).unwrap();

    assert!(m.background_tasks.is_empty());
}

#[test]
fn agent_metrics_deserializes_0_4_0_json_without_new_fields() {
    let json = r#"{
            "inferences": [],
            "tools": [],
            "suspensions": [],
            "handoffs": [],
            "delegations": [],
            "session_duration_ms": 0
        }"#;

    let m: AgentMetrics = serde_json::from_str(json).unwrap();

    assert!(m.evaluations.is_empty());
    assert!(m.background_tasks.is_empty());
}

#[test]
fn background_task_terminal_statuses_are_explicit() {
    for status in [
        TaskStatus::Completed,
        TaskStatus::Failed,
        TaskStatus::Cancelled,
    ] {
        assert!(
            BackgroundTaskSpan {
                status,
                ..make_background_task_span()
            }
            .is_terminal()
        );
    }
    assert!(
        !BackgroundTaskSpan {
            status: TaskStatus::Running,
            ..make_background_task_span()
        }
        .is_terminal()
    );
}

#[test]
fn background_task_status_deserializes_legacy_lowercase_string() {
    // Pre-enum builds wrote raw lowercase strings on the wire; new clients
    // must keep accepting them so already-persisted spans replay cleanly.
    let raw = r#"{
            "run_id": "r-legacy",
            "thread_id": "t",
            "agent_id": "a",
            "task_id": "bg-1",
            "task_type": "sub_agent",
            "description": "legacy",
            "status": "completed",
            "created_at_ms": 1
        }"#;
    let span: BackgroundTaskSpan = serde_json::from_str(raw).unwrap();
    assert_eq!(span.status, TaskStatus::Completed);
    assert!(span.is_terminal());
}

#[test]
fn background_task_status_round_trips_lowercase_wire_format() {
    let span = BackgroundTaskSpan {
        status: TaskStatus::Cancelled,
        ..make_background_task_span()
    };
    let json = serde_json::to_string(&span).unwrap();
    // Wire format must remain `"cancelled"` (lowercase) so existing
    // dashboards and persisted spans keep working.
    assert!(
        json.contains("\"status\":\"cancelled\""),
        "expected lowercase status, got {json}"
    );
    let restored: BackgroundTaskSpan = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.status, TaskStatus::Cancelled);
}

// ---- total_input_tokens() ----

#[test]
fn total_input_tokens_sums_across_spans() {
    let m = AgentMetrics {
        inferences: vec![
            make_genai_span("m", Some(100), Some(50)),
            make_genai_span("m", Some(200), Some(75)),
        ],
        ..Default::default()
    };
    assert_eq!(m.total_input_tokens(), 300);
}

#[test]
fn total_input_tokens_skips_none() {
    let m = AgentMetrics {
        inferences: vec![
            make_genai_span("m", Some(100), Some(50)),
            make_genai_span("m", None, Some(75)),
        ],
        ..Default::default()
    };
    assert_eq!(m.total_input_tokens(), 100);
}

// ---- total_output_tokens() ----

#[test]
fn total_output_tokens_sums_correctly() {
    let m = AgentMetrics {
        inferences: vec![
            make_genai_span("m", Some(100), Some(50)),
            make_genai_span("m", Some(200), Some(75)),
        ],
        ..Default::default()
    };
    assert_eq!(m.total_output_tokens(), 125);
}

// ---- total_cache_read_tokens() ----

#[test]
fn total_cache_read_tokens_handles_none_values() {
    let m = AgentMetrics {
        inferences: vec![
            GenAISpan {
                cache_read_input_tokens: Some(30),
                ..make_genai_span("m", Some(10), Some(5))
            },
            GenAISpan {
                cache_read_input_tokens: None,
                ..make_genai_span("m", Some(10), Some(5))
            },
            GenAISpan {
                cache_read_input_tokens: Some(20),
                ..make_genai_span("m", Some(10), Some(5))
            },
        ],
        ..Default::default()
    };
    assert_eq!(m.total_cache_read_tokens(), 50);
}

#[test]
fn total_cache_read_tokens_all_none_returns_zero() {
    let m = AgentMetrics {
        inferences: vec![
            make_genai_span("m", Some(10), Some(5)),
            make_genai_span("m", Some(10), Some(5)),
        ],
        ..Default::default()
    };
    assert_eq!(m.total_cache_read_tokens(), 0);
}

// ---- total_cache_creation_tokens() ----

#[test]
fn total_cache_creation_tokens_sums() {
    let m = AgentMetrics {
        inferences: vec![
            GenAISpan {
                cache_creation_input_tokens: Some(10),
                ..make_genai_span("m", Some(10), Some(5))
            },
            GenAISpan {
                cache_creation_input_tokens: Some(20),
                ..make_genai_span("m", Some(10), Some(5))
            },
            GenAISpan {
                cache_creation_input_tokens: None,
                ..make_genai_span("m", Some(10), Some(5))
            },
        ],
        ..Default::default()
    };
    assert_eq!(m.total_cache_creation_tokens(), 30);
}

// ---- stats_by_model() ----

#[test]
fn stats_by_model_groups_and_aggregates() {
    let m = AgentMetrics {
        inferences: vec![
            GenAISpan {
                provider: "openai".into(),
                cache_read_input_tokens: Some(5),
                ..make_genai_span("gpt-4", Some(100), Some(50))
            },
            GenAISpan {
                provider: "openai".into(),
                cache_read_input_tokens: Some(15),
                ..make_genai_span("gpt-4", Some(200), Some(75))
            },
            GenAISpan {
                provider: "anthropic".into(),
                ..make_genai_span("claude-3", Some(150), Some(60))
            },
        ],
        ..Default::default()
    };
    let stats = m.stats_by_model();
    assert_eq!(stats.len(), 2);

    // Sorted by model name: claude-3 first
    assert_eq!(stats[0].model, "claude-3");
    assert_eq!(stats[0].inference_count, 1);
    assert_eq!(stats[0].input_tokens, 150);

    assert_eq!(stats[1].model, "gpt-4");
    assert_eq!(stats[1].inference_count, 2);
    assert_eq!(stats[1].input_tokens, 300);
    assert_eq!(stats[1].output_tokens, 125);
    assert_eq!(stats[1].cache_read_input_tokens, 20);
    assert_eq!(stats[1].total_duration_ms, 200);
}

#[test]
fn stats_by_model_empty_inferences() {
    let m = AgentMetrics::default();
    assert!(m.stats_by_model().is_empty());
}

// ---- stats_by_tool() ----

#[test]
fn stats_by_tool_groups_and_aggregates() {
    let m = AgentMetrics {
        tools: vec![
            make_tool_span("search", false),
            make_tool_span("search", false),
            make_tool_span("write", true),
        ],
        ..Default::default()
    };
    let stats = m.stats_by_tool();
    assert_eq!(stats.len(), 2);

    let search = stats.iter().find(|s| s.name == "search").unwrap();
    assert_eq!(search.call_count, 2);
    assert_eq!(search.failure_count, 0);
    assert_eq!(search.total_duration_ms, 100);

    let write = stats.iter().find(|s| s.name == "write").unwrap();
    assert_eq!(write.call_count, 1);
    assert_eq!(write.failure_count, 1);
}

#[test]
fn stats_by_tool_empty_tools() {
    let m = AgentMetrics::default();
    assert!(m.stats_by_tool().is_empty());
}

// ---- stats_by_agent_and_tool() ----

fn make_tool_span_for_agent(name: &str, agent_id: &str, error: bool) -> ToolSpan {
    ToolSpan {
        context: SpanContext {
            run_id: "r1".into(),
            thread_id: "t1".into(),
            agent_id: agent_id.to_string(),
            ..Default::default()
        },
        ..make_tool_span(name, error)
    }
}

#[test]
fn stats_by_agent_and_tool_empty_when_no_tools() {
    let m = AgentMetrics::default();
    assert!(m.stats_by_agent_and_tool().is_empty());
}

#[test]
fn stats_by_agent_and_tool_groups_by_agent_then_tool() {
    let m = AgentMetrics {
        tools: vec![
            make_tool_span_for_agent("search", "planner", false),
            make_tool_span_for_agent("search", "planner", false),
            make_tool_span_for_agent("write", "planner", true),
            make_tool_span_for_agent("search", "worker", false),
            make_tool_span_for_agent("read", "worker", false),
        ],
        ..Default::default()
    };
    let stats = m.stats_by_agent_and_tool();
    assert_eq!(stats.len(), 4);

    let planner_search = stats
        .iter()
        .find(|s| s.agent_id == "planner" && s.tool == "search")
        .unwrap();
    assert_eq!(planner_search.call_count, 2);
    assert_eq!(planner_search.failure_count, 0);
    assert_eq!(planner_search.total_duration_ms, 100);

    let planner_write = stats
        .iter()
        .find(|s| s.agent_id == "planner" && s.tool == "write")
        .unwrap();
    assert_eq!(planner_write.call_count, 1);
    assert_eq!(planner_write.failure_count, 1);

    let worker_search = stats
        .iter()
        .find(|s| s.agent_id == "worker" && s.tool == "search")
        .unwrap();
    assert_eq!(worker_search.call_count, 1);

    let worker_read = stats
        .iter()
        .find(|s| s.agent_id == "worker" && s.tool == "read")
        .unwrap();
    assert_eq!(worker_read.call_count, 1);
}

#[test]
fn stats_by_agent_and_tool_results_sorted_lex() {
    let m = AgentMetrics {
        tools: vec![
            make_tool_span_for_agent("zebra", "worker", false),
            make_tool_span_for_agent("alpha", "planner", false),
            make_tool_span_for_agent("beta", "planner", false),
        ],
        ..Default::default()
    };
    let stats = m.stats_by_agent_and_tool();
    let keys: Vec<(&str, &str)> = stats
        .iter()
        .map(|s| (s.agent_id.as_str(), s.tool.as_str()))
        .collect();
    assert_eq!(
        keys,
        vec![
            ("planner", "alpha"),
            ("planner", "beta"),
            ("worker", "zebra")
        ]
    );
}

#[test]
fn stats_by_agent_and_tool_preserves_empty_agent_id_bucket() {
    let m = AgentMetrics {
        tools: vec![
            make_tool_span("search", false), // empty agent_id
            make_tool_span_for_agent("search", "named", false),
        ],
        ..Default::default()
    };
    let stats = m.stats_by_agent_and_tool();
    assert_eq!(stats.len(), 2);
    assert_eq!(stats[0].agent_id, ""); // empty sorts first
    assert_eq!(stats[0].tool, "search");
    assert_eq!(stats[0].call_count, 1);
    assert_eq!(stats[1].agent_id, "named");
    assert_eq!(stats[1].call_count, 1);
}

#[test]
fn stats_by_agent_and_tool_distinguishes_failures_per_bucket() {
    let m = AgentMetrics {
        tools: vec![
            make_tool_span_for_agent("write", "agent_a", false),
            make_tool_span_for_agent("write", "agent_a", true),
            make_tool_span_for_agent("write", "agent_b", false),
        ],
        ..Default::default()
    };
    let stats = m.stats_by_agent_and_tool();
    let a = stats.iter().find(|s| s.agent_id == "agent_a").unwrap();
    let b = stats.iter().find(|s| s.agent_id == "agent_b").unwrap();
    assert_eq!(a.call_count, 2);
    assert_eq!(a.failure_count, 1);
    assert_eq!(b.call_count, 1);
    assert_eq!(b.failure_count, 0);
}

#[test]
fn stats_by_agent_and_tool_aggregates_durations() {
    let m = AgentMetrics {
        tools: vec![
            ToolSpan {
                duration_ms: 30,
                started_at_ms: 0,
                ended_at_ms: 0,
                ..make_tool_span_for_agent("search", "x", false)
            },
            ToolSpan {
                duration_ms: 70,
                started_at_ms: 0,
                ended_at_ms: 0,
                ..make_tool_span_for_agent("search", "x", false)
            },
        ],
        ..Default::default()
    };
    let stats = m.stats_by_agent_and_tool();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].total_duration_ms, 100);
}

#[test]
fn stats_by_agent_and_tool_total_calls_match_per_tool_aggregation() {
    // Cross-check: summing per-(agent,tool) call_counts must equal the
    // per-tool aggregation for the same set of spans.
    let m = AgentMetrics {
        tools: vec![
            make_tool_span_for_agent("search", "a", false),
            make_tool_span_for_agent("search", "b", false),
            make_tool_span_for_agent("search", "a", true),
            make_tool_span_for_agent("write", "b", false),
        ],
        ..Default::default()
    };
    let by_tool = m.stats_by_tool();
    let by_pair = m.stats_by_agent_and_tool();

    for tool_stats in &by_tool {
        let summed: usize = by_pair
            .iter()
            .filter(|s| s.tool == tool_stats.name)
            .map(|s| s.call_count)
            .sum();
        assert_eq!(summed, tool_stats.call_count, "tool {}", tool_stats.name);
    }
}

// ---- tool_failures() ----

#[test]
fn tool_failures_counts_non_success() {
    let m = AgentMetrics {
        tools: vec![
            make_tool_span("a", false),
            make_tool_span("b", true),
            make_tool_span("c", true),
            make_tool_span("d", false),
        ],
        ..Default::default()
    };
    assert_eq!(m.tool_failures(), 2);
}

// ---- inference_count() and tool_count() ----

#[test]
fn inference_count_and_tool_count() {
    let m = AgentMetrics {
        inferences: vec![
            make_genai_span("a", Some(1), Some(1)),
            make_genai_span("b", Some(2), Some(2)),
            make_genai_span("c", Some(3), Some(3)),
        ],
        tools: vec![make_tool_span("t1", false), make_tool_span("t2", false)],
        ..Default::default()
    };
    assert_eq!(m.inference_count(), 3);
    assert_eq!(m.tool_count(), 2);
}

// ---- Edge cases ----

#[test]
fn empty_spans_edge_case() {
    let m = AgentMetrics::default();
    assert_eq!(m.total_input_tokens(), 0);
    assert_eq!(m.total_output_tokens(), 0);
    assert_eq!(m.inference_count(), 0);
    assert_eq!(m.tool_count(), 0);
    assert!(m.stats_by_model().is_empty());
    assert!(m.stats_by_tool().is_empty());
}

#[test]
fn zero_duration_spans() {
    let m = AgentMetrics {
        inferences: vec![GenAISpan {
            duration_ms: 0,
            started_at_ms: 0,
            ended_at_ms: 0,
            ..make_genai_span("m", Some(10), Some(5))
        }],
        tools: vec![ToolSpan {
            duration_ms: 0,
            started_at_ms: 0,
            ended_at_ms: 0,
            ..make_tool_span("t", false)
        }],
        ..Default::default()
    };
    assert_eq!(m.total_inference_duration_ms(), 0);
    assert_eq!(m.total_tool_duration_ms(), 0);
}

#[test]
fn all_none_token_fields() {
    let m = AgentMetrics {
        inferences: vec![make_genai_span("m", None, None)],
        ..Default::default()
    };
    assert_eq!(m.total_input_tokens(), 0);
    assert_eq!(m.total_output_tokens(), 0);
    assert_eq!(m.total_tokens(), 0);
}

// ---- ToolSpan::is_success ----

#[test]
fn tool_span_is_success_true() {
    let span = make_tool_span("search", false);
    assert!(span.is_success());
}

#[test]
fn tool_span_is_success_false() {
    let span = make_tool_span("write", true);
    assert!(!span.is_success());
}

// ---- New span type serde roundtrips ----

#[test]
fn suspension_span_serde_roundtrip() {
    let span = SuspensionSpan {
        context: SpanContext::default(),
        tool_call_id: "c1".to_string(),
        tool_name: "search".to_string(),
        action: "suspended".to_string(),
        resume_mode: Some("use_decision".to_string()),
        duration_ms: Some(5000),
        timestamp_ms: 1000,
    };
    let json = serde_json::to_string(&span).unwrap();
    let restored: SuspensionSpan = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.tool_call_id, "c1");
    assert_eq!(restored.action, "suspended");
    assert_eq!(restored.resume_mode.as_deref(), Some("use_decision"));
    assert_eq!(restored.duration_ms, Some(5000));
}

#[test]
fn handoff_span_serde_roundtrip() {
    let span = HandoffSpan {
        context: SpanContext::default(),
        from_agent_id: "agent-a".to_string(),
        to_agent_id: "agent-b".to_string(),
        reason: Some("escalation".to_string()),
        timestamp_ms: 2000,
    };
    let json = serde_json::to_string(&span).unwrap();
    let restored: HandoffSpan = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.from_agent_id, "agent-a");
    assert_eq!(restored.to_agent_id, "agent-b");
    assert_eq!(restored.reason.as_deref(), Some("escalation"));
}

#[test]
fn delegation_span_serde_roundtrip() {
    let span = DelegationSpan {
        context: SpanContext::default(),
        parent_run_id: "run-1".to_string(),
        child_run_id: Some("run-2".to_string()),
        target_agent_id: "worker".to_string(),
        tool_call_id: "c1".to_string(),
        duration_ms: Some(500),
        success: true,
        error_message: None,
        timestamp_ms: 3000,
    };
    let json = serde_json::to_string(&span).unwrap();
    let restored: DelegationSpan = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.parent_run_id, "run-1");
    assert_eq!(restored.child_run_id.as_deref(), Some("run-2"));
    assert!(restored.success);
    assert!(restored.error_message.is_none());
}

// ---- AgentMetrics new helpers ----

#[test]
fn agent_metrics_total_suspensions() {
    let m = AgentMetrics {
        suspensions: vec![
            SuspensionSpan {
                context: SpanContext::default(),
                tool_call_id: "c1".to_string(),
                tool_name: "s".to_string(),
                action: "suspended".to_string(),
                resume_mode: None,
                duration_ms: None,
                timestamp_ms: 0,
            },
            SuspensionSpan {
                context: SpanContext::default(),
                tool_call_id: "c1".to_string(),
                tool_name: "s".to_string(),
                action: "resumed".to_string(),
                resume_mode: Some("use_decision".to_string()),
                duration_ms: Some(100),
                timestamp_ms: 100,
            },
        ],
        ..Default::default()
    };
    assert_eq!(m.total_suspensions(), 2);
}

#[test]
fn agent_metrics_total_delegations() {
    let m = AgentMetrics {
        delegations: vec![
            DelegationSpan {
                context: SpanContext::default(),
                parent_run_id: "r1".to_string(),
                child_run_id: None,
                target_agent_id: "w1".to_string(),
                tool_call_id: "c1".to_string(),
                duration_ms: None,
                success: true,
                error_message: None,
                timestamp_ms: 0,
            },
            DelegationSpan {
                context: SpanContext::default(),
                parent_run_id: "r1".to_string(),
                child_run_id: None,
                target_agent_id: "w2".to_string(),
                tool_call_id: "c2".to_string(),
                duration_ms: None,
                success: false,
                error_message: Some("timeout".to_string()),
                timestamp_ms: 0,
            },
        ],
        ..Default::default()
    };
    assert_eq!(m.total_delegations(), 2);
}

#[test]
fn agent_metrics_successful_delegations() {
    let m = AgentMetrics {
        delegations: vec![
            DelegationSpan {
                context: SpanContext::default(),
                parent_run_id: "r1".to_string(),
                child_run_id: None,
                target_agent_id: "w1".to_string(),
                tool_call_id: "c1".to_string(),
                duration_ms: None,
                success: true,
                error_message: None,
                timestamp_ms: 0,
            },
            DelegationSpan {
                context: SpanContext::default(),
                parent_run_id: "r1".to_string(),
                child_run_id: None,
                target_agent_id: "w2".to_string(),
                tool_call_id: "c2".to_string(),
                duration_ms: None,
                success: false,
                error_message: Some("timeout".to_string()),
                timestamp_ms: 0,
            },
            DelegationSpan {
                context: SpanContext::default(),
                parent_run_id: "r1".to_string(),
                child_run_id: Some("r3".to_string()),
                target_agent_id: "w3".to_string(),
                tool_call_id: "c3".to_string(),
                duration_ms: Some(200),
                success: true,
                error_message: None,
                timestamp_ms: 0,
            },
        ],
        ..Default::default()
    };
    assert_eq!(m.successful_delegations(), 2);
}

#[test]
fn agent_metrics_default_has_empty_new_fields() {
    let m = AgentMetrics::default();
    assert!(m.suspensions.is_empty());
    assert!(m.handoffs.is_empty());
    assert!(m.delegations.is_empty());
    assert_eq!(m.total_suspensions(), 0);
    assert_eq!(m.total_handoffs(), 0);
    assert_eq!(m.total_delegations(), 0);
    assert_eq!(m.successful_delegations(), 0);
}
