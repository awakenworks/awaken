//! [`MetricsSink`] adapter that writes every event straight to a
//! [`TraceStore`] (ADR-0032 D1 integration with ADR-0030 trace storage).
//!
//! Unlike [`crate::PersistentSink`], this adapter has no buffering or
//! sampling logic — every event is appended on `record`. It's intended
//! for callers (eval replays) that want their spans to land in the same
//! file-backed [`TraceStore`] as production traces so the admin UI can
//! pivot from an eval-run item to the full trace.
//!
//! Failures (e.g. transient disk I/O) are logged at `tracing::warn!`
//! and swallowed: an eval replay must not abort because the trace
//! sidecar had a hiccup. Production-grade durability lives in
//! `PersistentSink::with_trace_store`; this adapter is for "best-effort
//! append, never block the replay".

use std::sync::Arc;

use crate::metrics::{AgentMetrics, MetricsEvent};
use crate::sink::{MetricsSink, SinkError};
use crate::trace_store::TraceStore;

/// Wraps an `Arc<dyn TraceStore>` and surfaces it as a [`MetricsSink`].
pub struct TraceStoreSink {
    store: Arc<dyn TraceStore>,
}

impl TraceStoreSink {
    pub fn new(store: Arc<dyn TraceStore>) -> Self {
        Self { store }
    }
}

impl MetricsSink for TraceStoreSink {
    fn record(&self, event: &MetricsEvent) {
        let run_id = event.run_id();
        if run_id.is_empty() {
            // Boot-time spans (test fixtures, init paths) have no run id —
            // they don't belong in TraceStore's per-run layout.
            return;
        }
        if let Err(err) = self.store.append(run_id, event) {
            tracing::warn!(
                run_id = %run_id,
                error = %err,
                "TraceStoreSink: append failed; replay continues without trace persistence"
            );
        }
    }

    fn on_run_end(&self, _metrics: &AgentMetrics) {
        // No-op: TraceStore is event-stream only; AgentMetrics is the
        // InMemorySink's concern.
    }

    fn flush(&self) -> Result<(), SinkError> {
        Ok(())
    }

    fn shutdown(&self) -> Result<(), SinkError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{GenAISpan, SpanContext};
    use crate::trace_store::file::FileTraceStore;

    fn span(run_id: &str) -> GenAISpan {
        GenAISpan {
            context: SpanContext {
                run_id: run_id.into(),
                ..Default::default()
            },
            step_index: Some(0),
            model: "m".into(),
            provider: "p".into(),
            operation: "chat".into(),
            response_model: None,
            response_id: None,
            finish_reasons: vec![],
            error_type: None,
            error_class: None,
            thinking_tokens: None,
            input_tokens: Some(1),
            output_tokens: Some(1),
            total_tokens: Some(2),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop_sequences: vec![],
            duration_ms: 1,
            started_at_ms: 0,
            ended_at_ms: 0,
            response_content: None,
            response_tool_calls: None,
            request_messages: None,
        }
    }

    #[test]
    fn record_routes_event_to_store_by_run_id() {
        let tmp = tempfile::tempdir().unwrap();
        let store: Arc<dyn TraceStore> = Arc::new(FileTraceStore::new(tmp.path()).unwrap());
        let sink = TraceStoreSink::new(store.clone());
        sink.record(&MetricsEvent::Inference(span("RUN-A")));
        let events = store.read("RUN-A").unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn record_silently_drops_events_with_empty_run_id() {
        // Boot-time spans have no run id — they'd otherwise hit
        // FileTraceStore's `InvalidRunId` path on every call. Suppress
        // here so a real replay isn't drowned in warnings.
        let tmp = tempfile::tempdir().unwrap();
        let store: Arc<dyn TraceStore> = Arc::new(FileTraceStore::new(tmp.path()).unwrap());
        let sink = TraceStoreSink::new(store.clone());
        sink.record(&MetricsEvent::Inference(span("")));
        // Nothing was written: a subsequent list yields no shards.
        let list = store
            .list(&crate::trace_store::TraceFilter::default())
            .unwrap();
        assert!(list.is_empty());
    }
}
