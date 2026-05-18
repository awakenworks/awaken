use super::*;
use crate::metrics::{GenAISpan, MetricsEvent, SpanContext};
use crate::trace_store::file::FileTraceStore;

fn span(run_id: &str) -> GenAISpan {
    GenAISpan {
        context: SpanContext {
            run_id: run_id.into(),
            ..Default::default()
        },
        step_index: None,
        model: "m".into(),
        provider: "p".into(),
        operation: "chat".into(),
        response_model: None,
        response_id: None,
        finish_reasons: vec![],
        error_type: None,
        error_class: None,
        input_tokens: Some(1),
        output_tokens: Some(2),
        total_tokens: Some(3),
        thinking_tokens: None,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        temperature: None,
        top_p: None,
        max_tokens: None,
        stop_sequences: vec![],
        duration_ms: 0,
        started_at_ms: 0,
        ended_at_ms: 0,
        response_content: None,
        response_tool_calls: None,
        request_messages: None,
    }
}

#[test]
fn persistent_sink_writes_through_trace_store() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("awaken-ts-adapter-{now}"));
    std::fs::create_dir_all(&dir).unwrap();
    let store = std::sync::Arc::new(FileTraceStore::new(&dir).unwrap());
    let inner: std::sync::Arc<dyn MetricsSink> =
        std::sync::Arc::new(crate::sink::InMemorySink::new());
    let sink = PersistentSink::with_trace_store(
        inner,
        store.clone(),
        PersistenceConfig {
            storage_dir: dir.clone(),
            ..PersistenceConfig::default()
        },
    )
    .unwrap();

    sink.record(&MetricsEvent::Inference(span("01HXTRACE")));
    sink.record(&MetricsEvent::Inference(span("01HXTRACE")));

    let events = store.read("01HXTRACE").unwrap();
    assert_eq!(
        events.len(),
        2,
        "events appended through adapter must round-trip"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Helper: build a metrics struct from a single inference span.
fn metrics_from_span(s: GenAISpan) -> AgentMetrics {
    AgentMetrics {
        inferences: vec![s],
        ..Default::default()
    }
}

/// Helper: build a metrics struct combining a single inference and a
/// judge evaluation event with the given score. Used by F14 tests.
fn metrics_with_judge(s: GenAISpan, judge_score: f64) -> AgentMetrics {
    AgentMetrics {
        inferences: vec![s.clone()],
        evaluations: vec![EvaluationResultEvent {
            context: s.context,
            name: "test-judge".into(),
            score_value: Some(judge_score),
            score_label: None,
            explanation: None,
            response_id: None,
            error_type: None,
            timestamp_ms: 0,
        }],
        ..Default::default()
    }
}

#[test]
fn sampling_policy_always_flushes_on_run_end() {
    use crate::sampling::SamplingMode;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("awaken-ts-sampling-always-{now}"));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(FileTraceStore::new(&dir).unwrap());
    let inner: Arc<dyn MetricsSink> = Arc::new(crate::sink::InMemorySink::new());

    let policy = Arc::new(parking_lot::RwLock::new(SamplingPolicy {
        normal_traces: SamplingMode::Always,
        ..SamplingPolicy::default()
    }));

    let sink = PersistentSink::with_trace_store(
        inner,
        store.clone(),
        PersistenceConfig {
            storage_dir: dir.clone(),
            ..PersistenceConfig::default()
        },
    )
    .unwrap()
    .with_sampling_policy(policy);

    let s = span("01HXSAMPL1");
    sink.record(&MetricsEvent::Inference(s.clone()));
    sink.record(&MetricsEvent::Inference(s.clone()));

    // Before run_end: nothing written yet.
    assert!(
        store.read("01HXSAMPL1").is_err(),
        "buffered events must not appear before on_run_end"
    );

    sink.on_run_end(&metrics_from_span(s));

    let events = store.read("01HXSAMPL1").unwrap();
    assert_eq!(events.len(), 2, "both events flushed after on_run_end");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sampling_policy_never_drops_buffer_on_run_end() {
    use crate::sampling::SamplingMode;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("awaken-ts-sampling-never-{now}"));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(FileTraceStore::new(&dir).unwrap());
    let inner: Arc<dyn MetricsSink> = Arc::new(crate::sink::InMemorySink::new());

    let policy = Arc::new(parking_lot::RwLock::new(SamplingPolicy {
        normal_traces: SamplingMode::Never,
        error_traces: SamplingMode::Never,
        ..SamplingPolicy::default()
    }));

    let sink = PersistentSink::with_trace_store(
        inner,
        store.clone(),
        PersistenceConfig {
            storage_dir: dir.clone(),
            ..PersistenceConfig::default()
        },
    )
    .unwrap()
    .with_sampling_policy(policy);

    let s = span("01HXSAMPL2");
    sink.record(&MetricsEvent::Inference(s.clone()));
    sink.on_run_end(&metrics_from_span(s));

    // Policy says Never: run should not appear in the store.
    let result = store.read("01HXSAMPL2");
    assert!(
        result.is_err(),
        "events must be dropped when policy is Never"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn error_run_persists_despite_low_normal_sampling() {
    use crate::sampling::SamplingMode;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("awaken-ts-error-run-{now}"));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(FileTraceStore::new(&dir).unwrap());
    let inner: Arc<dyn MetricsSink> = Arc::new(crate::sink::InMemorySink::new());

    // normal_traces=Never but error_traces=Always — the error span must
    // still make it through.
    let policy = Arc::new(parking_lot::RwLock::new(SamplingPolicy {
        normal_traces: SamplingMode::Never,
        error_traces: SamplingMode::Always,
        ..SamplingPolicy::default()
    }));

    let sink = PersistentSink::with_trace_store(
        inner,
        store.clone(),
        PersistenceConfig {
            storage_dir: dir.clone(),
            ..PersistenceConfig::default()
        },
    )
    .unwrap()
    .with_sampling_policy(policy);

    let error_span = GenAISpan {
        context: SpanContext {
            run_id: "01HXERRORRUN".into(),
            ..Default::default()
        },
        error_type: Some("rate_limited".into()),
        ..span("01HXERRORRUN")
    };
    sink.record(&MetricsEvent::Inference(error_span.clone()));

    let metrics = AgentMetrics {
        inferences: vec![error_span],
        ..Default::default()
    };
    sink.on_run_end(&metrics);

    let events = store.read("01HXERRORRUN").unwrap();
    assert_eq!(events.len(), 1, "error run must be flushed to trace store");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn background_only_run_surfaces_on_list() {
    // Regression for F21: a run with no inference/tool spans (only
    // BackgroundTask events) used to be silently skipped at
    // on_run_end — `run_id_from_metrics` returned None, no index was
    // written, and `/v1/traces` couldn't surface the run. Including
    // background_tasks in the fallback chain fixes this.
    use crate::metrics::SpanContext;
    use awaken_runtime::extensions::background::TaskStatus;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("awaken-ts-bg-only-{now}"));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(FileTraceStore::new(&dir).unwrap());
    let inner: Arc<dyn MetricsSink> = Arc::new(crate::sink::InMemorySink::new());
    let sink = PersistentSink::with_trace_store(
        inner,
        store.clone(),
        PersistenceConfig {
            storage_dir: dir.clone(),
            ..PersistenceConfig::default()
        },
    )
    .unwrap();

    let bg = BackgroundTaskSpan {
        context: SpanContext {
            run_id: "01HXBGRUN".into(),
            agent_id: "bg-agent".into(),
            ..SpanContext::default()
        },
        task_id: "bg".into(),
        task_type: "summarise".into(),
        task_name: None,
        description: "x".into(),
        status: TaskStatus::Running,
        parent_task_id: None,
        error_message: None,
        created_at_ms: 1_000,
        completed_at_ms: None,
    };
    sink.record(&MetricsEvent::BackgroundTask(bg.clone()));
    let metrics = AgentMetrics {
        background_tasks: vec![bg],
        ..Default::default()
    };
    sink.on_run_end(&metrics);

    let runs = store
        .list(&crate::trace_store::TraceFilter::default())
        .unwrap();
    let entry = runs.iter().find(|r| r.run_id == "01HXBGRUN");
    assert!(
        entry.is_some(),
        "background-only run must produce an index entry; got: {:?}",
        runs
    );
    assert_eq!(entry.unwrap().agent_id, "bg-agent");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_summary_brackets_evaluation_only_run_at_event_timestamp() {
    // Round 7 #2: a standalone evaluation event (no inference/tool/bg)
    // must surface a real `started_at`/`ended_at` on the index instead
    // of falling back to UNIX_EPOCH. Without the timestamp coverage
    // `run_id_from_metrics` would still find the run, but list ordering
    // and retention would treat it as 1970-01-01.
    use crate::metrics::{EvaluationResultEvent, SpanContext};
    use std::time::UNIX_EPOCH;

    let event = EvaluationResultEvent {
        context: SpanContext {
            run_id: "01HXEVALONLY".into(),
            agent_id: "judge-agent".into(),
            ..SpanContext::default()
        },
        name: "exact_match".into(),
        score_label: None,
        score_value: Some(0.9),
        explanation: None,
        response_id: None,
        error_type: None,
        timestamp_ms: 1_700_000_000_000,
    };
    let metrics = AgentMetrics {
        evaluations: vec![event],
        ..Default::default()
    };
    let summary = derive_run_summary("01HXEVALONLY", &metrics);
    assert_eq!(summary.agent_id, "judge-agent");
    assert_ne!(
        summary.started_at, UNIX_EPOCH,
        "evaluation-only run must not land at UNIX_EPOCH on the index"
    );
    assert!(summary.ended_at.is_some());
    assert_eq!(summary.final_status.as_deref(), Some("ok"));
}

#[test]
fn run_summary_brackets_handoff_and_delegation_runs() {
    // Round 7 #2: standalone handoff and delegation events also
    // contribute to the time bracket. Delegation's `duration_ms`
    // extends the end bound past `timestamp_ms`.
    use crate::metrics::{DelegationSpan, HandoffSpan, SpanContext};
    use std::time::UNIX_EPOCH;

    let handoff = HandoffSpan {
        context: SpanContext {
            run_id: "01HXHANDOFF".into(),
            agent_id: "agent-b".into(),
            ..SpanContext::default()
        },
        from_agent_id: "agent-a".into(),
        to_agent_id: "agent-b".into(),
        reason: None,
        timestamp_ms: 1_700_000_000_000,
    };
    let delegation = DelegationSpan {
        context: SpanContext {
            run_id: "01HXHANDOFF".into(),
            agent_id: "agent-b".into(),
            ..SpanContext::default()
        },
        parent_run_id: "01HXHANDOFF".into(),
        child_run_id: None,
        target_agent_id: "sub-agent".into(),
        tool_call_id: "tc1".into(),
        duration_ms: Some(5_000),
        success: true,
        error_message: None,
        timestamp_ms: 1_700_000_001_000,
    };
    let metrics = AgentMetrics {
        handoffs: vec![handoff],
        delegations: vec![delegation],
        ..Default::default()
    };
    let summary = derive_run_summary("01HXHANDOFF", &metrics);
    assert_ne!(summary.started_at, UNIX_EPOCH);
    // ended_at must include the delegation's timestamp + duration.
    let ended = summary.ended_at.expect("ended_at populated");
    let ended_ms = ended.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
    assert_eq!(ended_ms, 1_700_000_006_000);
}

#[test]
fn run_summary_aggregates_attribution_from_non_inference_spans() {
    // Round 8 #1: prompt_id / experiment_id / variant_name must
    // surface on the index for inference-less runs too. Previously
    // these were read only from `metrics.inferences`, so an
    // evaluation-only / handoff-only / background-only run would
    // land with empty attribution and `/v1/traces?prompt_id=…`
    // would silently miss it.
    use crate::metrics::{EvaluationResultEvent, HandoffSpan, SpanContext};
    use awaken_runtime::extensions::background::TaskStatus;

    let ctx = |label: &str| SpanContext {
        run_id: "01HXAGGR".into(),
        agent_id: "agent-x".into(),
        prompt_id: Some(format!("prompt-{label}")),
        experiment_id: Some(format!("exp-{label}")),
        variant_name: Some(format!("variant-{label}")),
        ..SpanContext::default()
    };

    // Evaluation, handoff, and a background task all carry their
    // own SpanContext with attribution. No inference span at all.
    let evaluation = EvaluationResultEvent {
        context: ctx("eval"),
        name: "judge".into(),
        score_label: None,
        score_value: Some(0.7),
        explanation: None,
        response_id: None,
        error_type: None,
        timestamp_ms: 1_000,
    };
    let handoff = HandoffSpan {
        context: ctx("handoff"),
        from_agent_id: "agent-a".into(),
        to_agent_id: "agent-x".into(),
        reason: None,
        timestamp_ms: 1_500,
    };
    let bg = BackgroundTaskSpan {
        context: ctx("bg"),
        task_id: "t".into(),
        task_type: "x".into(),
        task_name: None,
        description: "x".into(),
        status: TaskStatus::Running,
        parent_task_id: None,
        error_message: None,
        created_at_ms: 2_000,
        completed_at_ms: Some(3_000),
    };

    let metrics = AgentMetrics {
        evaluations: vec![evaluation],
        handoffs: vec![handoff],
        background_tasks: vec![bg],
        ..Default::default()
    };
    let summary = derive_run_summary("01HXAGGR", &metrics);
    assert_eq!(summary.agent_id, "agent-x");
    // All three contexts contributed distinct prompt_ids (sorted/deduped).
    let mut expected = ["prompt-eval", "prompt-handoff", "prompt-bg"]
        .map(String::from)
        .to_vec();
    expected.sort();
    assert_eq!(summary.prompt_ids, expected);
    // First-non-None wins: iter order is inferences→tools→
    // evaluations→…, so the evaluation context's experiment id is
    // picked. Any of the three would be correct; the assertion
    // pins the iteration order so an accidental reordering shows
    // up here.
    assert_eq!(summary.experiment_id.as_deref(), Some("exp-eval"));
    assert_eq!(summary.variant_name.as_deref(), Some("variant-eval"));
}

#[test]
fn run_summary_final_status_treats_failed_task_without_message_as_error() {
    // Round 8 #2: a background task with `status == Failed` but
    // `error_message == None` must still flip `final_status` to
    // "error" — the producer is not contractually required to fill
    // the message, so relying on it alone hides real failures from
    // the index and the sampling gate.
    use crate::metrics::SpanContext;
    use awaken_runtime::extensions::background::TaskStatus;

    let bg = BackgroundTaskSpan {
        context: SpanContext {
            run_id: "01HXBGFAIL".into(),
            agent_id: "a".into(),
            ..SpanContext::default()
        },
        task_id: "t".into(),
        task_type: "x".into(),
        task_name: None,
        description: "x".into(),
        status: TaskStatus::Failed,
        parent_task_id: None,
        error_message: None,
        created_at_ms: 1,
        completed_at_ms: Some(2),
    };
    let m = AgentMetrics {
        background_tasks: vec![bg],
        ..Default::default()
    };
    assert_eq!(
        derive_run_summary("01HXBGFAIL", &m).final_status.as_deref(),
        Some("error"),
        "Failed background task without error_message must still flip final_status to error"
    );
    // And the sampling gate must see the same error signal.
    assert!(
        run_had_error(&m),
        "Failed background task must satisfy the sampling-gate error definition"
    );
}

#[test]
fn run_summary_final_status_covers_non_inference_errors() {
    // Round 7 #2: delegation failure, background-task error, and
    // evaluation error must all flip `final_status` to "error".
    // Suspensions and handoffs are status transitions, not failures,
    // and so are not asserted to influence the flag.
    use crate::metrics::{DelegationSpan, EvaluationResultEvent, SpanContext};
    use awaken_runtime::extensions::background::TaskStatus;

    let ctx = || SpanContext {
        run_id: "01HXFAIL".into(),
        agent_id: "a".into(),
        ..SpanContext::default()
    };

    let delegation_fail = DelegationSpan {
        context: ctx(),
        parent_run_id: "01HXFAIL".into(),
        child_run_id: None,
        target_agent_id: "child".into(),
        tool_call_id: "tc".into(),
        duration_ms: Some(1),
        success: false,
        error_message: Some("denied".into()),
        timestamp_ms: 1,
    };
    let m = AgentMetrics {
        delegations: vec![delegation_fail],
        ..Default::default()
    };
    assert_eq!(
        derive_run_summary("01HXFAIL", &m).final_status.as_deref(),
        Some("error"),
        "delegation !success must flip final_status to error"
    );

    let bg_error = BackgroundTaskSpan {
        context: ctx(),
        task_id: "t".into(),
        task_type: "x".into(),
        task_name: None,
        description: "x".into(),
        status: TaskStatus::Running,
        parent_task_id: None,
        error_message: Some("oom".into()),
        created_at_ms: 1,
        completed_at_ms: Some(2),
    };
    let m = AgentMetrics {
        background_tasks: vec![bg_error],
        ..Default::default()
    };
    assert_eq!(
        derive_run_summary("01HXFAIL", &m).final_status.as_deref(),
        Some("error"),
        "background-task error_message must flip final_status to error"
    );

    let eval_error = EvaluationResultEvent {
        context: ctx(),
        name: "x".into(),
        score_label: None,
        score_value: None,
        explanation: None,
        response_id: None,
        error_type: Some("timeout".into()),
        timestamp_ms: 1,
    };
    let m = AgentMetrics {
        evaluations: vec![eval_error],
        ..Default::default()
    };
    assert_eq!(
        derive_run_summary("01HXFAIL", &m).final_status.as_deref(),
        Some("error"),
        "evaluation error_type must flip final_status to error"
    );
}

#[test]
fn run_summary_index_carries_judge_score() {
    // Regression for F18: prior `derive_run_summary` hardcoded
    // judge_score=None even when the sampling path read it from
    // EvaluationResultEvent. Surface it on the index so list() can
    // explain a low-score retention and operators can sort/filter.
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("awaken-ts-summary-score-{now}"));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(FileTraceStore::new(&dir).unwrap());
    let inner: Arc<dyn MetricsSink> = Arc::new(crate::sink::InMemorySink::new());

    // No sampling policy → immediate path; index written at on_run_end.
    let sink = PersistentSink::with_trace_store(
        inner,
        store.clone(),
        PersistenceConfig {
            storage_dir: dir.clone(),
            ..PersistenceConfig::default()
        },
    )
    .unwrap();

    let s = span("01HXJUDGESUM");
    sink.record(&MetricsEvent::Inference(s.clone()));
    sink.on_run_end(&metrics_with_judge(s, 0.3));

    let runs = store
        .list(&crate::trace_store::TraceFilter::default())
        .unwrap();
    let entry = runs.iter().find(|r| r.run_id == "01HXJUDGESUM").unwrap();
    assert_eq!(
        entry.judge_score.map(|v| (v * 100.0).round() / 100.0),
        Some(0.3),
        "RunSummary index must carry the derived judge_score, not None"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn low_judge_score_promotes_run_under_normal_never_policy() {
    // Regression for F14: prior `on_run_end` hardcoded judge_score
    // to None, so the `low_judge_score` policy never fired. With
    // F14 the sink derives judge_score from the recorded
    // EvaluationResultEvent — a low-scoring run is now persisted
    // even when the normal-traces policy is `Never`.
    use crate::sampling::SamplingMode;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("awaken-ts-judge-{now}"));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(FileTraceStore::new(&dir).unwrap());
    let inner: Arc<dyn MetricsSink> = Arc::new(crate::sink::InMemorySink::new());

    let policy = Arc::new(parking_lot::RwLock::new(SamplingPolicy {
        normal_traces: SamplingMode::Never,
        // Defaults: low_judge_score = Always, threshold = 0.5
        ..SamplingPolicy::default()
    }));

    let sink = PersistentSink::with_trace_store(
        inner,
        store.clone(),
        PersistenceConfig {
            storage_dir: dir.clone(),
            ..PersistenceConfig::default()
        },
    )
    .unwrap()
    .with_sampling_policy(policy);

    let s = span("01HXJUDGE");
    sink.record(&MetricsEvent::Inference(s.clone()));
    // Low score (< default threshold 0.5) should override the
    // Never normal_traces policy.
    sink.on_run_end(&metrics_with_judge(s, 0.2));

    let events = store.read("01HXJUDGE").unwrap();
    assert_eq!(events.len(), 1, "low-judge run must persist");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn on_run_end_writes_run_summary_index_for_immediate_path() {
    // Regression for F3: prior `on_run_end` only flushed events. Without
    // a `write_index_for_run` call, `list()` could not surface the run
    // even though its `.ndjson` existed. This test pins the index path.
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("awaken-ts-index-{now}"));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(FileTraceStore::new(&dir).unwrap());
    let inner: Arc<dyn MetricsSink> = Arc::new(crate::sink::InMemorySink::new());

    // No sampling policy → immediate write-through path.
    let sink = PersistentSink::with_trace_store(
        inner,
        store.clone(),
        PersistenceConfig {
            storage_dir: dir.clone(),
            ..PersistenceConfig::default()
        },
    )
    .unwrap();

    let s = span("01HXINDEX");
    sink.record(&MetricsEvent::Inference(s.clone()));
    sink.on_run_end(&metrics_from_span(s));

    let runs = store
        .list(&crate::trace_store::TraceFilter::default())
        .unwrap();
    assert_eq!(
        runs.len(),
        1,
        "list() must surface the run via its index file"
    );
    assert_eq!(runs[0].run_id, "01HXINDEX");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn overflowed_run_is_dropped_not_flushed_as_tail_fragment() {
    // Regression: prior implementation called `entry.clear()` on overflow
    // and kept buffering, which produced a tail-fragment trace at run_end.
    // The `RunBuffer::Overflowed` enum state must prevent any further
    // events from being written for the overflowing run.
    use crate::sampling::SamplingMode;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("awaken-ts-overflow-{now}"));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(FileTraceStore::new(&dir).unwrap());
    let inner: Arc<dyn MetricsSink> = Arc::new(crate::sink::InMemorySink::new());

    let policy = Arc::new(parking_lot::RwLock::new(SamplingPolicy {
        normal_traces: SamplingMode::Always,
        ..SamplingPolicy::default()
    }));

    let sink = PersistentSink::with_trace_store(
        inner,
        store.clone(),
        PersistenceConfig {
            storage_dir: dir.clone(),
            ..PersistenceConfig::default()
        },
    )
    .unwrap()
    .with_sampling_policy(policy);

    let s = span("01HXOVERFLOW");
    assert_eq!(sink.overflow_count(), 0, "starts at zero");
    // Push MAX + 1 events to force the overflow transition.
    for _ in 0..=MAX_BUFFERED_EVENTS_PER_RUN {
        sink.record(&MetricsEvent::Inference(s.clone()));
    }
    assert_eq!(
        sink.overflow_count(),
        1,
        "overflow_count must increment exactly once when the buffer cap is hit"
    );

    sink.on_run_end(&metrics_from_span(s));

    // Overflowed run must not be persisted at all — never a partial
    // tail-fragment.
    assert!(
        store.read("01HXOVERFLOW").is_err(),
        "overflowed run must be dropped at on_run_end, not flushed as a fragment"
    );
    // Counter survives `on_run_end` so wiring banners can read it.
    assert_eq!(sink.overflow_count(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
