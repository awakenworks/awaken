use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use super::metrics::{AgentMetrics, MetricsEvent};
use super::sink::{MetricsSink, SinkError};

/// Configuration for [`PersistentSink`].
pub struct PersistenceConfig {
    /// Directory where failed event files are stored.
    pub storage_dir: PathBuf,
    /// Maximum number of retry attempts per file (default: 8).
    pub max_retry_attempts: u32,
    /// Base backoff delay between retries (default: 500ms).
    pub base_backoff: Duration,
    /// Maximum backoff delay (default: 30s).
    pub max_backoff: Duration,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            storage_dir: std::env::temp_dir().join("awaken-persistent-sink"),
            max_retry_attempts: 8,
            base_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
        }
    }
}

/// Envelope for run-end events persisted to disk (session duration only).
///
/// `MetricsEvent` covers the five span types; this wrapper adds the run-end
/// case so that all persisted lines share a consistent tagged JSON format.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum PersistedLine {
    RunEnd {
        #[serde(rename = "type")]
        line_type: RunEndMarker,
        session_duration_ms: u64,
    },
    Event(Box<MetricsEvent>),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum RunEndMarker {
    #[serde(rename = "run_end")]
    RunEnd,
}

/// A [`MetricsSink`] wrapper that persists events to disk on flush failure.
///
/// All `record` calls are forwarded to the inner sink immediately and also
/// buffered locally. On a successful [`MetricsSink::flush`] the buffer is
/// cleared. On failure the buffer is written as NDJSON to `storage_dir`.
/// [`retry_persisted`](PersistentSink::retry_persisted) reads those files
/// back and replays them through the inner sink.
pub struct PersistentSink {
    inner: Arc<dyn MetricsSink>,
    config: PersistenceConfig,
    pending: Arc<Mutex<Vec<PersistedLine>>>,
}

impl PersistentSink {
    /// Create a new `PersistentSink` wrapping `inner`.
    ///
    /// Creates `config.storage_dir` if it does not exist.
    pub fn new(inner: Arc<dyn MetricsSink>, config: PersistenceConfig) -> std::io::Result<Self> {
        std::fs::create_dir_all(&config.storage_dir)?;
        Ok(Self {
            inner,
            config,
            pending: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Write the given lines to an NDJSON file in `storage_dir`.
    fn persist_to_disk(&self, lines: &[PersistedLine]) -> std::io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let filename = format!("failed_events_{}.ndjson", uuid::Uuid::now_v7().hyphenated());
        let path = self.config.storage_dir.join(filename);
        let mut file = std::fs::File::create(&path)?;
        for line in lines {
            let json = serde_json::to_string(line)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            writeln!(file, "{json}")?;
        }
        file.flush()?;
        Ok(())
    }

    /// Replay a single [`PersistedLine`] through the inner sink.
    fn replay_line(&self, line: &PersistedLine) {
        match line {
            PersistedLine::Event(event) => self.inner.record(event.as_ref()),
            PersistedLine::RunEnd {
                session_duration_ms,
                ..
            } => {
                let metrics = AgentMetrics {
                    session_duration_ms: *session_duration_ms,
                    ..Default::default()
                };
                self.inner.on_run_end(&metrics);
            }
        }
    }

    /// Load persisted NDJSON files from `storage_dir`, replay events through
    /// the inner sink, and delete files that were fully replayed.
    ///
    /// Returns the total number of events replayed.
    pub fn retry_persisted(&self) -> std::io::Result<usize> {
        let mut total = 0usize;
        let entries: Vec<_> = std::fs::read_dir(&self.config.storage_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
            .collect();

        for entry in entries {
            let path = entry.path();
            let file = std::fs::File::open(&path)?;
            let reader = std::io::BufReader::new(file);
            let mut lines = Vec::new();

            for raw_line in reader.lines() {
                let raw_line = raw_line?;
                if raw_line.trim().is_empty() {
                    continue;
                }
                let line: PersistedLine = serde_json::from_str(&raw_line)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                lines.push(line);
            }

            // Replay all lines, then attempt flush.
            for line in &lines {
                self.replay_line(line);
            }

            match self.inner.flush() {
                Ok(()) => {
                    std::fs::remove_file(&path)?;
                    total += lines.len();
                }
                Err(_) => {
                    // Leave the file for a future retry attempt.
                }
            }
        }

        Ok(total)
    }

    /// Number of events buffered since the last successful flush.
    pub fn pending_count(&self) -> usize {
        self.pending.lock().len()
    }
}

impl MetricsSink for PersistentSink {
    fn record(&self, event: &MetricsEvent) {
        self.inner.record(event);
        self.pending
            .lock()
            .push(PersistedLine::Event(Box::new(event.clone())));
    }

    fn on_run_end(&self, metrics: &AgentMetrics) {
        self.inner.on_run_end(metrics);
        self.pending.lock().push(PersistedLine::RunEnd {
            line_type: RunEndMarker::RunEnd,
            session_duration_ms: metrics.session_duration_ms,
        });
    }

    fn flush(&self) -> Result<(), SinkError> {
        match self.inner.flush() {
            Ok(()) => {
                self.pending.lock().clear();
                Ok(())
            }
            Err(e) => {
                let pending: Vec<_> = self.pending.lock().drain(..).collect();
                if !pending.is_empty() {
                    let _ = self.persist_to_disk(&pending);
                }
                Err(e)
            }
        }
    }

    fn shutdown(&self) -> Result<(), SinkError> {
        let flush_result = self.flush();
        let _ = self.inner.shutdown();
        flush_result
    }
}

#[cfg(test)]
mod tests {
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
            error_type: None,
            duration_ms: 50,
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
        let sink =
            PersistentSink::new(Arc::clone(&failable) as Arc<dyn MetricsSink>, config).unwrap();

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
}
