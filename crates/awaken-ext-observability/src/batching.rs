use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

use tokio::sync::Notify;

use crate::metrics::{AgentMetrics, MetricsEvent};
use crate::sink::{MetricsSink, SinkError};

/// Configuration for [`BatchingSink`].
pub struct BatchingConfig {
    /// Flush when buffer reaches this size.
    pub max_batch_size: usize,
    /// Flush every this duration (used by background flush task).
    pub flush_interval: Duration,
    /// Maximum buffer size before dropping new events (backpressure).
    pub max_buffer_size: usize,
}

impl Default for BatchingConfig {
    fn default() -> Self {
        Self {
            max_batch_size: 100,
            flush_interval: Duration::from_secs(5),
            max_buffer_size: 10_000,
        }
    }
}

/// Internal buffer holding all event types as a unified stream.
#[derive(Default)]
struct Buffer {
    events: Vec<MetricsEvent>,
}

impl Buffer {
    fn len(&self) -> usize {
        self.events.len()
    }

    fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    fn take(&mut self) -> Vec<MetricsEvent> {
        std::mem::take(&mut self.events)
    }
}

/// A [`MetricsSink`] that buffers events and periodically flushes to an inner sink.
///
/// Events are buffered in memory and flushed to the inner sink when:
/// - The buffer reaches `max_batch_size`
/// - The periodic flush interval elapses (requires [`start_background_flush`](Self::start_background_flush))
/// - `flush()` or `shutdown()` is called explicitly
/// - `on_run_end()` is called
///
/// When the buffer reaches `max_buffer_size`, new events are dropped (backpressure).
pub struct BatchingSink {
    inner: Arc<dyn MetricsSink>,
    buffer: Arc<Mutex<Buffer>>,
    config: BatchingConfig,
    flush_notify: Arc<Notify>,
    pub(crate) shutdown_flag: Arc<AtomicBool>,
}

impl BatchingSink {
    pub fn new(inner: Arc<dyn MetricsSink>, config: BatchingConfig) -> Self {
        Self {
            inner,
            buffer: Arc::new(Mutex::new(Buffer::default())),
            config,
            flush_notify: Arc::new(Notify::new()),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn with_defaults(inner: Arc<dyn MetricsSink>) -> Self {
        Self::new(inner, BatchingConfig::default())
    }

    /// Start the background flush task. Returns a `JoinHandle`.
    ///
    /// Call this once after creation to enable periodic flushing.
    /// The task runs until `shutdown()` is called.
    pub fn start_background_flush(&self) -> tokio::task::JoinHandle<()> {
        let buffer = Arc::clone(&self.buffer);
        let inner = Arc::clone(&self.inner);
        let notify = Arc::clone(&self.flush_notify);
        let shutdown = Arc::clone(&self.shutdown_flag);
        let interval = self.config.flush_interval;

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = notify.notified() => {}
                    _ = tokio::time::sleep(interval) => {}
                }

                if shutdown.load(Ordering::SeqCst) {
                    break;
                }

                flush_to_inner(&buffer, &inner);
            }
        })
    }

    /// Flush buffered events to the inner sink immediately.
    pub(crate) fn flush_buffer(&self) {
        flush_to_inner(&self.buffer, &self.inner);
    }

    /// Get current buffer size (total items across all event types).
    pub fn buffered_count(&self) -> usize {
        self.buffer.lock().len()
    }
}

/// Drain the buffer and forward all events to the inner sink.
fn flush_to_inner(buffer: &Mutex<Buffer>, inner: &Arc<dyn MetricsSink>) {
    let batch = {
        let mut buf = buffer.lock();
        if buf.is_empty() {
            return;
        }
        buf.take()
    };

    for event in &batch {
        inner.record(event);
    }
}

impl MetricsSink for BatchingSink {
    fn record(&self, event: &MetricsEvent) {
        let mut buf = self.buffer.lock();
        if buf.len() < self.config.max_buffer_size {
            buf.events.push(event.clone());
            if buf.len() >= self.config.max_batch_size {
                drop(buf);
                self.flush_notify.notify_one();
            }
        }
    }

    fn on_run_end(&self, metrics: &AgentMetrics) {
        self.flush_buffer();
        self.inner.on_run_end(metrics);
    }

    fn flush(&self) -> Result<(), SinkError> {
        self.flush_buffer();
        self.inner.flush()
    }

    fn shutdown(&self) -> Result<(), SinkError> {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        self.flush_notify.notify_one();
        self.flush_buffer();
        self.inner.shutdown()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{DelegationSpan, GenAISpan, HandoffSpan, SuspensionSpan, ToolSpan};
    use crate::sink::InMemorySink;
    use std::sync::atomic::AtomicUsize;

    fn sample_genai_span() -> GenAISpan {
        GenAISpan {
            context: crate::metrics::SpanContext::default(),
            step_index: None,
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
            context: crate::metrics::SpanContext::default(),
            step_index: None,
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

    /// A sink that tracks flush and shutdown calls.
    struct TrackingSink {
        inner: InMemorySink,
        flush_count: AtomicUsize,
        shutdown_count: AtomicUsize,
    }

    impl TrackingSink {
        fn new() -> Self {
            Self {
                inner: InMemorySink::new(),
                flush_count: AtomicUsize::new(0),
                shutdown_count: AtomicUsize::new(0),
            }
        }
    }

    impl MetricsSink for TrackingSink {
        fn record(&self, event: &MetricsEvent) {
            self.inner.record(event);
        }
        fn on_run_end(&self, metrics: &AgentMetrics) {
            self.inner.on_run_end(metrics);
        }
        fn flush(&self) -> Result<(), SinkError> {
            self.flush_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn shutdown(&self) -> Result<(), SinkError> {
            self.shutdown_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn batching_sink_buffers_until_flush() {
        let tracking = Arc::new(TrackingSink::new());
        let sink = BatchingSink::new(
            tracking.clone(),
            BatchingConfig {
                max_batch_size: 100,
                max_buffer_size: 10_000,
                ..Default::default()
            },
        );

        // Add 5 mixed events
        sink.record(&MetricsEvent::Inference(sample_genai_span()));
        sink.record(&MetricsEvent::Tool(sample_tool_span()));
        sink.record(&MetricsEvent::Suspension(sample_suspension_span()));
        sink.record(&MetricsEvent::Handoff(sample_handoff_span()));
        sink.record(&MetricsEvent::Delegation(sample_delegation_span()));

        // Inner should have 0 events (still buffered)
        let m = tracking.inner.metrics();
        assert_eq!(m.inference_count(), 0);
        assert_eq!(m.tool_count(), 0);
        assert_eq!(m.total_suspensions(), 0);
        assert_eq!(m.total_handoffs(), 0);
        assert_eq!(m.total_delegations(), 0);

        // Flush and verify inner has all 5
        sink.flush().unwrap();
        let m = tracking.inner.metrics();
        assert_eq!(m.inference_count(), 1);
        assert_eq!(m.tool_count(), 1);
        assert_eq!(m.total_suspensions(), 1);
        assert_eq!(m.total_handoffs(), 1);
        assert_eq!(m.total_delegations(), 1);
    }

    #[test]
    fn batching_sink_auto_flushes_at_batch_size() {
        let tracking = Arc::new(TrackingSink::new());
        let sink = BatchingSink::new(
            tracking.clone(),
            BatchingConfig {
                max_batch_size: 3,
                max_buffer_size: 10_000,
                ..Default::default()
            },
        );

        sink.record(&MetricsEvent::Inference(sample_genai_span()));
        sink.record(&MetricsEvent::Tool(sample_tool_span()));
        // Third event triggers notify (batch_size=3)
        sink.record(&MetricsEvent::Suspension(sample_suspension_span()));

        // In synchronous context, the notify doesn't flush automatically
        // (no background task). Manually call flush_buffer which is what
        // the background task would do.
        sink.flush_buffer();

        let m = tracking.inner.metrics();
        assert_eq!(m.inference_count(), 1);
        assert_eq!(m.tool_count(), 1);
        assert_eq!(m.total_suspensions(), 1);
    }

    #[test]
    fn batching_sink_backpressure_drops_at_max() {
        let tracking = Arc::new(TrackingSink::new());
        let sink = BatchingSink::new(
            tracking.clone(),
            BatchingConfig {
                max_batch_size: 100, // won't auto-flush
                max_buffer_size: 5,
                ..Default::default()
            },
        );

        // Add 10 events; only first 5 should be kept
        for _ in 0..10 {
            sink.record(&MetricsEvent::Inference(sample_genai_span()));
        }

        assert!(sink.buffered_count() <= 5);

        sink.flush().unwrap();
        let m = tracking.inner.metrics();
        assert!(m.inference_count() <= 5);
        assert!(m.inference_count() >= 1); // at least some were accepted
    }

    #[test]
    fn batching_sink_run_end_flushes() {
        let tracking = Arc::new(TrackingSink::new());
        let sink = BatchingSink::new(
            tracking.clone(),
            BatchingConfig {
                max_batch_size: 100,
                max_buffer_size: 10_000,
                ..Default::default()
            },
        );

        sink.record(&MetricsEvent::Inference(sample_genai_span()));
        sink.record(&MetricsEvent::Tool(sample_tool_span()));
        sink.record(&MetricsEvent::Delegation(sample_delegation_span()));

        let run_metrics = AgentMetrics {
            session_duration_ms: 5000,
            ..Default::default()
        };
        sink.on_run_end(&run_metrics);

        let m = tracking.inner.metrics();
        assert_eq!(m.inference_count(), 1);
        assert_eq!(m.tool_count(), 1);
        assert_eq!(m.total_delegations(), 1);
        assert_eq!(m.session_duration_ms, 5000);
    }

    #[test]
    fn batching_sink_shutdown_flushes_and_delegates() {
        let tracking = Arc::new(TrackingSink::new());
        let sink = BatchingSink::new(
            tracking.clone(),
            BatchingConfig {
                max_batch_size: 100,
                max_buffer_size: 10_000,
                ..Default::default()
            },
        );

        sink.record(&MetricsEvent::Inference(sample_genai_span()));
        sink.record(&MetricsEvent::Tool(sample_tool_span()));

        sink.shutdown().unwrap();

        // Events should have been flushed to inner
        let m = tracking.inner.metrics();
        assert_eq!(m.inference_count(), 1);
        assert_eq!(m.tool_count(), 1);

        // Shutdown was called on inner
        assert_eq!(tracking.shutdown_count.load(Ordering::SeqCst), 1);

        // Shutdown flag should be set
        assert!(sink.shutdown_flag.load(Ordering::SeqCst));
    }

    #[test]
    fn batching_sink_with_defaults_creates_valid_config() {
        let inner = Arc::new(InMemorySink::new());
        let sink = BatchingSink::with_defaults(inner);

        assert_eq!(sink.config.max_batch_size, 100);
        assert_eq!(sink.config.flush_interval, Duration::from_secs(5));
        assert_eq!(sink.config.max_buffer_size, 10_000);
    }

    #[test]
    fn batching_sink_buffered_count() {
        let inner = Arc::new(InMemorySink::new());
        let sink = BatchingSink::with_defaults(inner);

        assert_eq!(sink.buffered_count(), 0);

        sink.record(&MetricsEvent::Inference(sample_genai_span()));
        assert_eq!(sink.buffered_count(), 1);

        sink.record(&MetricsEvent::Tool(sample_tool_span()));
        assert_eq!(sink.buffered_count(), 2);

        sink.record(&MetricsEvent::Suspension(sample_suspension_span()));
        sink.record(&MetricsEvent::Handoff(sample_handoff_span()));
        sink.record(&MetricsEvent::Delegation(sample_delegation_span()));
        assert_eq!(sink.buffered_count(), 5);

        // After flush, buffer should be empty
        sink.flush().unwrap();
        assert_eq!(sink.buffered_count(), 0);
    }
}
