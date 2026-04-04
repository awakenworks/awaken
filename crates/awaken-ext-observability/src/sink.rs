use std::sync::Arc;

use parking_lot::Mutex;

use super::metrics::{AgentMetrics, MetricsEvent};

/// Error type for sink operations (flush, shutdown).
#[derive(Debug)]
pub struct SinkError {
    pub message: String,
    pub source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl std::fmt::Display for SinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SinkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|e| e.as_ref() as &(dyn std::error::Error + 'static))
    }
}

impl SinkError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            source: None,
        }
    }

    pub fn with_source(
        msg: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: msg.into(),
            source: Some(Box::new(source)),
        }
    }
}

/// Trait for consuming telemetry data.
pub trait MetricsSink: Send + Sync {
    /// Record a single metrics event.
    fn record(&self, event: &MetricsEvent);

    /// Called at end of run with aggregated metrics.
    fn on_run_end(&self, metrics: &AgentMetrics);

    /// Flush any buffered data. Returns `Ok(())` by default.
    fn flush(&self) -> Result<(), SinkError> {
        Ok(())
    }

    /// Graceful shutdown. Returns `Ok(())` by default.
    fn shutdown(&self) -> Result<(), SinkError> {
        Ok(())
    }
}

/// In-memory sink for testing and inspection.
#[derive(Debug, Clone, Default)]
pub struct InMemorySink {
    inner: Arc<Mutex<AgentMetrics>>,
}

impl InMemorySink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn metrics(&self) -> AgentMetrics {
        self.inner.lock().clone()
    }
}

impl MetricsSink for InMemorySink {
    fn record(&self, event: &MetricsEvent) {
        let mut inner = self.inner.lock();
        match event {
            MetricsEvent::Inference(s) => inner.inferences.push(s.clone()),
            MetricsEvent::Tool(s) => inner.tools.push(s.clone()),
            MetricsEvent::Suspension(s) => inner.suspensions.push(s.clone()),
            MetricsEvent::Handoff(s) => inner.handoffs.push(s.clone()),
            MetricsEvent::Delegation(s) => inner.delegations.push(s.clone()),
        }
    }

    fn on_run_end(&self, metrics: &AgentMetrics) {
        self.inner.lock().session_duration_ms = metrics.session_duration_ms;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{DelegationSpan, GenAISpan, HandoffSpan, SuspensionSpan, ToolSpan};
    use std::error::Error;

    fn sample_genai_span(model: &str, input: i32, output: i32) -> GenAISpan {
        GenAISpan {
            context: crate::metrics::SpanContext::default(),
            step_index: None,
            model: model.to_string(),
            provider: "test-provider".to_string(),
            operation: "chat".to_string(),
            response_model: None,
            response_id: None,
            finish_reasons: vec!["end_turn".to_string()],
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
            stop_sequences: Vec::new(),
            duration_ms: 100,
        }
    }

    fn sample_tool_span(name: &str, error: bool) -> ToolSpan {
        ToolSpan {
            context: crate::metrics::SpanContext::default(),
            step_index: None,
            name: name.to_string(),
            operation: "execute".to_string(),
            call_id: "call_1".to_string(),
            tool_type: "function".to_string(),
            error_type: if error {
                Some("timeout".to_string())
            } else {
                None
            },
            duration_ms: 50,
        }
    }

    #[test]
    fn in_memory_sink_records_inferences() {
        let sink = InMemorySink::new();
        sink.record(&MetricsEvent::Inference(sample_genai_span(
            "gpt-4", 100, 50,
        )));
        sink.record(&MetricsEvent::Inference(sample_genai_span(
            "gpt-4", 200, 75,
        )));

        let metrics = sink.metrics();
        assert_eq!(metrics.inferences.len(), 2);
        assert_eq!(metrics.total_input_tokens(), 300);
        assert_eq!(metrics.total_output_tokens(), 125);
    }

    #[test]
    fn in_memory_sink_records_tools() {
        let sink = InMemorySink::new();
        sink.record(&MetricsEvent::Tool(sample_tool_span("read_file", false)));
        sink.record(&MetricsEvent::Tool(sample_tool_span("write_file", true)));

        let metrics = sink.metrics();
        assert_eq!(metrics.tools.len(), 2);
        assert_eq!(metrics.tool_failures(), 1);
    }

    #[test]
    fn in_memory_sink_records_session_duration() {
        let sink = InMemorySink::new();
        let run_metrics = AgentMetrics {
            session_duration_ms: 5000,
            ..Default::default()
        };
        sink.on_run_end(&run_metrics);

        let metrics = sink.metrics();
        assert_eq!(metrics.session_duration_ms, 5000);
    }

    #[test]
    fn in_memory_sink_starts_empty() {
        let sink = InMemorySink::new();
        let metrics = sink.metrics();
        assert!(metrics.inferences.is_empty());
        assert!(metrics.tools.is_empty());
        assert_eq!(metrics.session_duration_ms, 0);
    }

    #[test]
    fn in_memory_sink_clone_shares_data() {
        let sink = InMemorySink::new();
        let clone = sink.clone();
        sink.record(&MetricsEvent::Inference(sample_genai_span(
            "model-a", 10, 5,
        )));

        let metrics = clone.metrics();
        assert_eq!(metrics.inferences.len(), 1);
    }

    #[test]
    fn in_memory_sink_stats_by_model() {
        let sink = InMemorySink::new();
        sink.record(&MetricsEvent::Inference(sample_genai_span(
            "gpt-4", 100, 50,
        )));
        sink.record(&MetricsEvent::Inference(sample_genai_span(
            "gpt-4", 200, 75,
        )));
        sink.record(&MetricsEvent::Inference(sample_genai_span(
            "claude-3", 150, 60,
        )));

        let metrics = sink.metrics();
        let stats = metrics.stats_by_model();
        assert_eq!(stats.len(), 2);

        let claude = stats.iter().find(|s| s.model == "claude-3").unwrap();
        assert_eq!(claude.inference_count, 1);
        assert_eq!(claude.input_tokens, 150);

        let gpt = stats.iter().find(|s| s.model == "gpt-4").unwrap();
        assert_eq!(gpt.inference_count, 2);
        assert_eq!(gpt.input_tokens, 300);
    }

    #[test]
    fn in_memory_sink_stores_suspensions() {
        let sink = InMemorySink::new();
        sink.record(&MetricsEvent::Suspension(SuspensionSpan {
            context: crate::metrics::SpanContext::default(),
            tool_call_id: "c1".to_string(),
            tool_name: "search".to_string(),
            action: "suspended".to_string(),
            resume_mode: None,
            duration_ms: None,
            timestamp_ms: 1000,
        }));
        sink.record(&MetricsEvent::Suspension(SuspensionSpan {
            context: crate::metrics::SpanContext::default(),
            tool_call_id: "c1".to_string(),
            tool_name: "search".to_string(),
            action: "resumed".to_string(),
            resume_mode: Some("use_decision".to_string()),
            duration_ms: Some(5000),
            timestamp_ms: 6000,
        }));

        let metrics = sink.metrics();
        assert_eq!(metrics.total_suspensions(), 2);
        assert_eq!(metrics.suspensions[0].action, "suspended");
        assert_eq!(metrics.suspensions[1].action, "resumed");
    }

    #[test]
    fn in_memory_sink_stores_handoffs() {
        let sink = InMemorySink::new();
        sink.record(&MetricsEvent::Handoff(HandoffSpan {
            context: crate::metrics::SpanContext::default(),
            from_agent_id: "agent-a".to_string(),
            to_agent_id: "agent-b".to_string(),
            reason: Some("escalation".to_string()),
            timestamp_ms: 1000,
        }));

        let metrics = sink.metrics();
        assert_eq!(metrics.total_handoffs(), 1);
        assert_eq!(metrics.handoffs[0].from_agent_id, "agent-a");
        assert_eq!(metrics.handoffs[0].to_agent_id, "agent-b");
    }

    #[test]
    fn in_memory_sink_stores_delegations() {
        let sink = InMemorySink::new();
        sink.record(&MetricsEvent::Delegation(DelegationSpan {
            context: crate::metrics::SpanContext::default(),
            parent_run_id: "run-1".to_string(),
            child_run_id: Some("run-2".to_string()),
            target_agent_id: "worker".to_string(),
            tool_call_id: "c1".to_string(),
            duration_ms: Some(500),
            success: true,
            error_message: None,
            timestamp_ms: 3000,
        }));

        let metrics = sink.metrics();
        assert_eq!(metrics.total_delegations(), 1);
        assert_eq!(metrics.delegations[0].target_agent_id, "worker");
        assert!(metrics.delegations[0].success);
    }

    #[test]
    fn flush_and_shutdown_default_impls_succeed() {
        let sink = InMemorySink::new();
        assert!(sink.flush().is_ok());
        assert!(sink.shutdown().is_ok());
    }

    #[test]
    fn in_memory_sink_stats_by_tool() {
        let sink = InMemorySink::new();
        sink.record(&MetricsEvent::Tool(sample_tool_span("read_file", false)));
        sink.record(&MetricsEvent::Tool(sample_tool_span("read_file", false)));
        sink.record(&MetricsEvent::Tool(sample_tool_span("write_file", true)));

        let metrics = sink.metrics();
        let stats = metrics.stats_by_tool();
        assert_eq!(stats.len(), 2);

        let read = stats.iter().find(|s| s.name == "read_file").unwrap();
        assert_eq!(read.call_count, 2);
        assert_eq!(read.failure_count, 0);

        let write = stats.iter().find(|s| s.name == "write_file").unwrap();
        assert_eq!(write.call_count, 1);
        assert_eq!(write.failure_count, 1);
    }

    #[test]
    fn sink_error_display() {
        let err = SinkError::new("test error");
        assert_eq!(err.to_string(), "test error");
        assert!(err.source().is_none());
    }

    #[test]
    fn sink_error_with_source() {
        let source = std::io::Error::other("io error");
        let err = SinkError::with_source("wrapper", source);
        assert_eq!(err.to_string(), "wrapper");
        assert!(err.source().is_some());
    }
}
