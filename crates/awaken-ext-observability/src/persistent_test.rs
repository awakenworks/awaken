use super::*;
use crate::InMemorySink;
use crate::metrics::{DelegationSpan, GenAISpan, HandoffSpan, SuspensionSpan, ToolSpan};
use std::sync::atomic::{AtomicBool, Ordering};

/// A sink whose `flush()` can be toggled to fail.
struct FailableSink {
    inner: InMemorySink,
    fail_flush: Arc<AtomicBool>,
}

impl FailableSink {
    fn new(fail_flush: bool) -> Self {
        Self {
            inner: InMemorySink::new(),
            fail_flush: Arc::new(AtomicBool::new(fail_flush)),
        }
    }
}

impl MetricsSink for FailableSink {
    fn record(&self, event: &MetricsEvent) {
        self.inner.record(event);
    }
    fn on_run_end(&self, metrics: &AgentMetrics) {
        self.inner.on_run_end(metrics);
    }
    fn flush(&self) -> Result<(), SinkError> {
        if self.fail_flush.load(Ordering::Relaxed) {
            Err(SinkError::new("flush failed"))
        } else {
            Ok(())
        }
    }
}

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("awaken-persistent-sink-test")
        .join(name)
        .join(uuid::Uuid::now_v7().hyphenated().to_string());
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sample_genai_span() -> GenAISpan {
    GenAISpan {
        context: crate::metrics::SpanContext::default(),
        step_index: None,
        model: "test-model".to_string(),
        provider: "test-provider".to_string(),
        operation: "chat".to_string(),
        response_model: None,
        response_id: None,
        finish_reasons: vec!["end_turn".to_string()],
        error_type: None,
        error_class: None,
        thinking_tokens: None,
        input_tokens: Some(100),
        output_tokens: Some(50),
        total_tokens: Some(150),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        temperature: None,
        top_p: None,
        max_tokens: None,
        stop_sequences: Vec::new(),
        duration_ms: 200,
        started_at_ms: 0,
        ended_at_ms: 0,
        response_content: None,
        response_tool_calls: None,
        request_messages: None,
    }
}

fn sample_tool_span() -> ToolSpan {
    ToolSpan {
        context: crate::metrics::SpanContext::default(),
        step_index: None,
        name: "read_file".to_string(),
        operation: "execute".to_string(),
        call_id: "call_1".to_string(),
        tool_type: "function".to_string(),
        call_arguments: None,
        call_result: None,
        error_type: None,
        duration_ms: 50,
        started_at_ms: 0,
        ended_at_ms: 0,
    }
}

fn sample_suspension_span() -> SuspensionSpan {
    SuspensionSpan {
        context: crate::metrics::SpanContext::default(),
        tool_call_id: "c1".to_string(),
        tool_name: "search".to_string(),
        action: "suspended".to_string(),
        resume_mode: None,
        duration_ms: None,
        timestamp_ms: 1000,
    }
}

fn sample_handoff_span() -> HandoffSpan {
    HandoffSpan {
        context: crate::metrics::SpanContext::default(),
        from_agent_id: "agent-a".to_string(),
        to_agent_id: "agent-b".to_string(),
        reason: Some("escalation".to_string()),
        timestamp_ms: 2000,
    }
}

fn sample_delegation_span() -> DelegationSpan {
    DelegationSpan {
        context: crate::metrics::SpanContext::default(),
        parent_run_id: "run-1".to_string(),
        child_run_id: Some("run-2".to_string()),
        target_agent_id: "worker".to_string(),
        tool_call_id: "c1".to_string(),
        duration_ms: Some(500),
        success: true,
        error_message: None,
        timestamp_ms: 3000,
    }
}

#[test]
fn persistent_sink_delegates_to_inner() {
    let inner = Arc::new(InMemorySink::new());
    let config = PersistenceConfig {
        storage_dir: test_dir("delegates"),
        ..Default::default()
    };
    let sink = PersistentSink::new(Arc::clone(&inner) as Arc<dyn MetricsSink>, config).unwrap();

    sink.record(&MetricsEvent::Inference(sample_genai_span()));
    sink.record(&MetricsEvent::Tool(sample_tool_span()));
    sink.record(&MetricsEvent::Suspension(sample_suspension_span()));
    sink.record(&MetricsEvent::Handoff(sample_handoff_span()));
    sink.record(&MetricsEvent::Delegation(sample_delegation_span()));
    sink.on_run_end(&AgentMetrics {
        session_duration_ms: 5000,
        ..Default::default()
    });

    let metrics = inner.metrics();
    assert_eq!(metrics.inferences.len(), 1);
    assert_eq!(metrics.tools.len(), 1);
    assert_eq!(metrics.suspensions.len(), 1);
    assert_eq!(metrics.handoffs.len(), 1);
    assert_eq!(metrics.delegations.len(), 1);
    assert_eq!(metrics.session_duration_ms, 5000);
}

#[test]
fn persistent_sink_persists_on_flush_failure() {
    let failable = Arc::new(FailableSink::new(true));
    let dir = test_dir("flush-fail");
    let config = PersistenceConfig {
        storage_dir: dir.clone(),
        ..Default::default()
    };
    let sink = PersistentSink::new(Arc::clone(&failable) as Arc<dyn MetricsSink>, config).unwrap();

    sink.record(&MetricsEvent::Inference(sample_genai_span()));
    sink.record(&MetricsEvent::Tool(sample_tool_span()));

    let result = sink.flush();
    assert!(result.is_err());

    // Verify an NDJSON file was created
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
        .collect();
    assert_eq!(files.len(), 1);

    // Verify file has 2 lines (one per event)
    let content = std::fs::read_to_string(files[0].path()).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 2);
}

#[test]
fn persistent_sink_retry_replays_persisted() {
    let inner = Arc::new(InMemorySink::new());
    let dir = test_dir("retry-replay");
    let config = PersistenceConfig {
        storage_dir: dir.clone(),
        ..Default::default()
    };
    let sink = PersistentSink::new(Arc::clone(&inner) as Arc<dyn MetricsSink>, config).unwrap();

    // Manually create an NDJSON file with events
    let lines = vec![
        PersistedLine::Event(Box::new(MetricsEvent::Inference(sample_genai_span()))),
        PersistedLine::Event(Box::new(MetricsEvent::Tool(sample_tool_span()))),
        PersistedLine::Event(Box::new(MetricsEvent::Suspension(sample_suspension_span()))),
        PersistedLine::Event(Box::new(MetricsEvent::Handoff(sample_handoff_span()))),
        PersistedLine::Event(Box::new(MetricsEvent::Delegation(sample_delegation_span()))),
        PersistedLine::RunEnd {
            line_type: RunEndMarker::RunEnd,
            session_duration_ms: 9000,
        },
    ];
    let path = dir.join("failed_events_manual.ndjson");
    let mut file = std::fs::File::create(&path).unwrap();
    for line in &lines {
        writeln!(file, "{}", serde_json::to_string(line).unwrap()).unwrap();
    }
    drop(file);

    let replayed = sink.retry_persisted().unwrap();
    assert_eq!(replayed, 6);

    let metrics = inner.metrics();
    assert_eq!(metrics.inferences.len(), 1);
    assert_eq!(metrics.tools.len(), 1);
    assert_eq!(metrics.suspensions.len(), 1);
    assert_eq!(metrics.handoffs.len(), 1);
    assert_eq!(metrics.delegations.len(), 1);
    assert_eq!(metrics.session_duration_ms, 9000);
}

#[test]
fn persistent_sink_retry_deletes_file_on_success() {
    let inner = Arc::new(InMemorySink::new());
    let dir = test_dir("retry-delete");
    let config = PersistenceConfig {
        storage_dir: dir.clone(),
        ..Default::default()
    };
    let sink = PersistentSink::new(Arc::clone(&inner) as Arc<dyn MetricsSink>, config).unwrap();

    // Create an NDJSON file
    let path = dir.join("failed_events_delete_test.ndjson");
    let line = PersistedLine::Event(Box::new(MetricsEvent::Inference(sample_genai_span())));
    std::fs::write(&path, serde_json::to_string(&line).unwrap() + "\n").unwrap();

    assert!(path.exists());
    sink.retry_persisted().unwrap();
    assert!(!path.exists());
}

#[test]
fn persisted_line_serde_roundtrip() {
    let lines = vec![
        PersistedLine::Event(Box::new(MetricsEvent::Inference(sample_genai_span()))),
        PersistedLine::Event(Box::new(MetricsEvent::Tool(sample_tool_span()))),
        PersistedLine::Event(Box::new(MetricsEvent::Suspension(sample_suspension_span()))),
        PersistedLine::Event(Box::new(MetricsEvent::Handoff(sample_handoff_span()))),
        PersistedLine::Event(Box::new(MetricsEvent::Delegation(sample_delegation_span()))),
        PersistedLine::RunEnd {
            line_type: RunEndMarker::RunEnd,
            session_duration_ms: 42000,
        },
    ];

    for line in &lines {
        let json = serde_json::to_string(line).unwrap();
        let restored: PersistedLine = serde_json::from_str(&json).unwrap();
        // Verify round-trip by re-serializing
        let json2 = serde_json::to_string(&restored).unwrap();
        assert_eq!(json, json2);
    }
}

#[test]
fn persistent_sink_config_defaults() {
    let config = PersistenceConfig::default();
    assert_eq!(
        config.storage_dir,
        std::env::temp_dir().join("awaken-persistent-sink")
    );
    assert_eq!(config.max_retry_attempts, 8);
    assert_eq!(config.base_backoff, Duration::from_millis(500));
    assert_eq!(config.max_backoff, Duration::from_secs(30));
}
