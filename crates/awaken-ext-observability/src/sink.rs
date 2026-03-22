use std::sync::{Arc, Mutex};

use super::metrics::{AgentMetrics, GenAISpan, ToolSpan};

/// Trait for consuming telemetry data.
pub trait MetricsSink: Send + Sync {
    fn on_inference(&self, span: &GenAISpan);
    fn on_tool(&self, span: &ToolSpan);
    fn on_run_end(&self, metrics: &AgentMetrics);
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
        self.inner.lock().unwrap().clone()
    }
}

impl MetricsSink for InMemorySink {
    fn on_inference(&self, span: &GenAISpan) {
        self.inner.lock().unwrap().inferences.push(span.clone());
    }

    fn on_tool(&self, span: &ToolSpan) {
        self.inner.lock().unwrap().tools.push(span.clone());
    }

    fn on_run_end(&self, metrics: &AgentMetrics) {
        self.inner.lock().unwrap().session_duration_ms = metrics.session_duration_ms;
    }
}
