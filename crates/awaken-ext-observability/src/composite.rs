use std::sync::Arc;

use crate::metrics::{AgentMetrics, MetricsEvent};
use crate::sink::{MetricsSink, SinkError};

/// Dispatches metrics to multiple sinks sequentially.
pub struct CompositeSink {
    sinks: Vec<Arc<dyn MetricsSink>>,
}

impl CompositeSink {
    pub fn new(sinks: Vec<Arc<dyn MetricsSink>>) -> Self {
        Self { sinks }
    }

    pub fn builder() -> CompositeSinkBuilder {
        CompositeSinkBuilder { sinks: Vec::new() }
    }

    pub fn add(&mut self, sink: Arc<dyn MetricsSink>) {
        self.sinks.push(sink);
    }

    pub fn len(&self) -> usize {
        self.sinks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sinks.is_empty()
    }
}

/// Builder for assembling a [`CompositeSink`] from multiple sinks.
pub struct CompositeSinkBuilder {
    sinks: Vec<Arc<dyn MetricsSink>>,
}

impl CompositeSinkBuilder {
    pub fn add(mut self, sink: Arc<dyn MetricsSink>) -> Self {
        self.sinks.push(sink);
        self
    }

    pub fn build(self) -> CompositeSink {
        CompositeSink { sinks: self.sinks }
    }
}

impl MetricsSink for CompositeSink {
    fn record(&self, event: &MetricsEvent) {
        for sink in &self.sinks {
            sink.record(event);
        }
    }

    fn on_run_end(&self, metrics: &AgentMetrics) {
        for sink in &self.sinks {
            sink.on_run_end(metrics);
        }
    }

    fn flush(&self) -> Result<(), SinkError> {
        let mut errors = Vec::new();
        for sink in &self.sinks {
            if let Err(e) = sink.flush() {
                errors.push(e);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(SinkError::new(format!(
                "{} sink(s) failed to flush",
                errors.len()
            )))
        }
    }

    fn shutdown(&self) -> Result<(), SinkError> {
        let mut errors = Vec::new();
        for sink in &self.sinks {
            if let Err(e) = sink.shutdown() {
                errors.push(e);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(SinkError::new(format!(
                "{} sink(s) failed to shutdown",
                errors.len()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{DelegationSpan, GenAISpan, HandoffSpan, SuspensionSpan, ToolSpan};
    use crate::sink::InMemorySink;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sample_genai_span() -> GenAISpan {
        GenAISpan {
            model: "test-model".to_string(),
            provider: "test".to_string(),
            operation: "chat".to_string(),
            response_model: None,
            response_id: None,
            finish_reasons: Vec::new(),
            error_type: None,
            error_class: None,
            thinking_tokens: None,
            input_tokens: Some(10),
            output_tokens: Some(5),
            total_tokens: Some(15),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop_sequences: Vec::new(),
            duration_ms: 100,
        }
    }

    fn sample_tool_span() -> ToolSpan {
        ToolSpan {
            name: "search".to_string(),
            operation: "execute_tool".to_string(),
            call_id: "c1".to_string(),
            tool_type: "function".to_string(),
            error_type: None,
            duration_ms: 50,
        }
    }

    fn sample_suspension_span() -> SuspensionSpan {
        SuspensionSpan {
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
            from_agent_id: "agent-a".to_string(),
            to_agent_id: "agent-b".to_string(),
            reason: Some("escalation".to_string()),
            timestamp_ms: 2000,
        }
    }

    fn sample_delegation_span() -> DelegationSpan {
        DelegationSpan {
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

    /// A sink that always fails on flush/shutdown.
    struct FailingSink;

    impl MetricsSink for FailingSink {
        fn record(&self, _event: &MetricsEvent) {}
        fn on_run_end(&self, _metrics: &AgentMetrics) {}

        fn flush(&self) -> Result<(), SinkError> {
            Err(SinkError::new("flush failed"))
        }

        fn shutdown(&self) -> Result<(), SinkError> {
            Err(SinkError::new("shutdown failed"))
        }
    }

    /// A sink that counts shutdown calls.
    struct CountingSink {
        shutdown_count: AtomicUsize,
    }

    impl CountingSink {
        fn new() -> Self {
            Self {
                shutdown_count: AtomicUsize::new(0),
            }
        }
    }

    impl MetricsSink for CountingSink {
        fn record(&self, _event: &MetricsEvent) {}
        fn on_run_end(&self, _metrics: &AgentMetrics) {}

        fn shutdown(&self) -> Result<(), SinkError> {
            self.shutdown_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn composite_dispatches_to_all_sinks() {
        let sink_a = Arc::new(InMemorySink::new());
        let sink_b = Arc::new(InMemorySink::new());
        let composite = CompositeSink::new(vec![sink_a.clone(), sink_b.clone()]);

        composite.record(&MetricsEvent::Inference(sample_genai_span()));
        composite.record(&MetricsEvent::Tool(sample_tool_span()));
        composite.record(&MetricsEvent::Suspension(sample_suspension_span()));
        composite.record(&MetricsEvent::Handoff(sample_handoff_span()));
        composite.record(&MetricsEvent::Delegation(sample_delegation_span()));
        composite.on_run_end(&AgentMetrics {
            session_duration_ms: 1000,
            ..Default::default()
        });

        for sink in [&sink_a, &sink_b] {
            let m = sink.metrics();
            assert_eq!(m.inference_count(), 1);
            assert_eq!(m.tool_count(), 1);
            assert_eq!(m.total_suspensions(), 1);
            assert_eq!(m.total_handoffs(), 1);
            assert_eq!(m.total_delegations(), 1);
            assert_eq!(m.session_duration_ms, 1000);
        }
    }

    #[test]
    fn composite_empty_is_valid() {
        let composite = CompositeSink::new(vec![]);
        assert!(composite.is_empty());
        assert_eq!(composite.len(), 0);

        // Should not panic on empty dispatch.
        composite.record(&MetricsEvent::Inference(sample_genai_span()));
        composite.record(&MetricsEvent::Tool(sample_tool_span()));
        composite.on_run_end(&AgentMetrics::default());
        assert!(composite.flush().is_ok());
        assert!(composite.shutdown().is_ok());
    }

    #[test]
    fn composite_flush_collects_errors() {
        let ok_sink = Arc::new(InMemorySink::new());
        let fail_sink: Arc<dyn MetricsSink> = Arc::new(FailingSink);
        let composite = CompositeSink::new(vec![ok_sink, fail_sink]);

        let result = composite.flush();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("1 sink(s) failed to flush")
        );
    }

    #[test]
    fn composite_shutdown_calls_all_sinks() {
        let counting_a = Arc::new(CountingSink::new());
        let counting_b = Arc::new(CountingSink::new());
        let composite = CompositeSink::new(vec![counting_a.clone(), counting_b.clone()]);

        composite.shutdown().unwrap();

        assert_eq!(counting_a.shutdown_count.load(Ordering::SeqCst), 1);
        assert_eq!(counting_b.shutdown_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn composite_add_sink_dynamically() {
        let mut composite = CompositeSink::new(vec![]);
        assert!(composite.is_empty());

        let sink = Arc::new(InMemorySink::new());
        composite.add(sink.clone());
        assert_eq!(composite.len(), 1);

        composite.record(&MetricsEvent::Inference(sample_genai_span()));
        assert_eq!(sink.metrics().inference_count(), 1);
    }
}
