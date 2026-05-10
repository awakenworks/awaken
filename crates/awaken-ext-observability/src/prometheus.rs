use metrics::{counter, histogram};

use crate::metrics::{
    AgentMetrics, BackgroundTaskSpan, DelegationSpan, GenAISpan, HandoffSpan, MetricsEvent,
    SuspensionSpan, ToolSpan,
};
use crate::sink::MetricsSink;

/// `MetricsSink` that emits Prometheus counters and histograms via the global
/// `metrics` recorder.
///
/// All recording happens through the [`metrics`](https://crates.io/crates/metrics)
/// facade — installing a `metrics-exporter-prometheus` recorder is the
/// caller's responsibility (`awaken-server::metrics::install_recorder` already
/// does this for the HTTP `/metrics` route).  When no recorder is installed
/// the calls become silent no-ops.
///
/// `PrometheusSink` is `Default`, `Clone`, `Send`, and `Sync` so it can be
/// freely shared across plugin instances and threads.
#[derive(Debug, Default, Clone, Copy)]
pub struct PrometheusSink;

impl PrometheusSink {
    /// Construct a new sink. Recording is a no-op until a `metrics` recorder
    /// is installed somewhere in the process.
    pub fn new() -> Self {
        Self
    }
}

impl MetricsSink for PrometheusSink {
    fn record(&self, event: &MetricsEvent) {
        match event {
            MetricsEvent::Inference(span) => record_inference(span),
            MetricsEvent::Tool(span) => record_tool(span),
            MetricsEvent::Suspension(span) => record_suspension(span),
            MetricsEvent::Handoff(span) => record_handoff(span),
            MetricsEvent::Delegation(span) => record_delegation(span),
        }
    }

    fn record_background_task(&self, span: &BackgroundTaskSpan) {
        record_background_task(span);
    }

    fn on_run_end(&self, metrics: &AgentMetrics) {
        record_run_end(metrics);
    }
}

pub(crate) fn record_inference(span: &GenAISpan) {
    let status = if span.error_type.is_some() {
        "error"
    } else {
        "ok"
    };
    counter!(
        "awaken_inference_requests_total",
        "model" => span.model.clone(),
        "provider" => span.provider.clone(),
        "status" => status
    )
    .increment(1);
    histogram!(
        "awaken_inference_duration_seconds",
        "model" => span.model.clone(),
        "provider" => span.provider.clone(),
        "status" => status
    )
    .record(span.duration_ms as f64 / 1000.0);

    inc_tokens(span, "input", span.input_tokens);
    inc_tokens(span, "output", span.output_tokens);
    inc_tokens(span, "total", span.total_tokens);
    inc_tokens(span, "thinking", span.thinking_tokens);
    inc_tokens(span, "cache_read_input", span.cache_read_input_tokens);
    inc_tokens(
        span,
        "cache_creation_input",
        span.cache_creation_input_tokens,
    );

    if let Some(class) = span.error_class.as_deref().or(span.error_type.as_deref()) {
        counter!(
            "awaken_inference_errors_total",
            "model" => span.model.clone(),
            "provider" => span.provider.clone(),
            "class" => class.to_string()
        )
        .increment(1);
    }
}

pub(crate) fn record_tool(span: &ToolSpan) {
    let status = if span.error_type.is_some() {
        "error"
    } else {
        "ok"
    };
    counter!(
        "awaken_tool_calls_total",
        "tool" => span.name.clone(),
        "status" => status
    )
    .increment(1);
    histogram!(
        "awaken_tool_duration_seconds",
        "tool" => span.name.clone(),
        "status" => status
    )
    .record(span.duration_ms as f64 / 1000.0);

    if let Some(error_type) = span.error_type.as_deref() {
        counter!(
            "awaken_tool_errors_total",
            "tool" => span.name.clone(),
            "class" => error_type.to_string()
        )
        .increment(1);
    }
}

pub(crate) fn record_suspension(span: &SuspensionSpan) {
    counter!(
        "awaken_agent_suspensions_total",
        "action" => span.action.clone(),
        "resume_mode" => span.resume_mode.clone().unwrap_or_else(|| "none".to_string())
    )
    .increment(1);
}

pub(crate) fn record_handoff(_span: &HandoffSpan) {
    counter!("awaken_agent_handoffs_total").increment(1);
}

pub(crate) fn record_delegation(span: &DelegationSpan) {
    let status = if span.success { "ok" } else { "error" };
    counter!(
        "awaken_agent_delegations_total",
        "status" => status
    )
    .increment(1);
    if let Some(duration_ms) = span.duration_ms {
        histogram!(
            "awaken_agent_delegation_duration_seconds",
            "status" => status
        )
        .record(duration_ms as f64 / 1000.0);
    }
}

pub(crate) fn record_background_task(span: &BackgroundTaskSpan) {
    if !span.is_terminal() {
        return;
    }
    counter!(
        "awaken_background_tasks_total",
        "task_type" => span.task_type.clone(),
        "status" => span.status.clone()
    )
    .increment(1);
    if let Some(completed_at_ms) = span.completed_at_ms {
        let duration_ms = completed_at_ms.saturating_sub(span.created_at_ms);
        histogram!(
            "awaken_background_task_duration_seconds",
            "task_type" => span.task_type.clone(),
            "status" => span.status.clone()
        )
        .record(duration_ms as f64 / 1000.0);
    }
}

pub(crate) fn record_run_end(metrics: &AgentMetrics) {
    histogram!("awaken_agent_session_duration_seconds")
        .record(metrics.session_duration_ms as f64 / 1000.0);
}

fn inc_tokens(span: &GenAISpan, token_type: &str, count: Option<i32>) {
    let Some(count) = count else {
        return;
    };
    let Ok(count) = u64::try_from(count) else {
        return;
    };
    if count == 0 {
        return;
    }
    counter!(
        "awaken_inference_tokens_total",
        "model" => span.model.clone(),
        "provider" => span.provider.clone(),
        "type" => token_type.to_string()
    )
    .increment(count);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    static PROM_HANDLE: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();

    fn install_recorder() -> &'static metrics_exporter_prometheus::PrometheusHandle {
        PROM_HANDLE.get_or_init(|| {
            metrics_exporter_prometheus::PrometheusBuilder::new()
                .install_recorder()
                .expect("install prometheus recorder")
        })
    }

    fn sample_inference() -> GenAISpan {
        GenAISpan {
            context: crate::metrics::SpanContext::default(),
            step_index: Some(0),
            model: "gpt-test".to_string(),
            provider: "openai".to_string(),
            operation: "chat".to_string(),
            response_model: None,
            response_id: None,
            finish_reasons: Vec::new(),
            error_type: None,
            error_class: None,
            thinking_tokens: Some(1),
            input_tokens: Some(10),
            output_tokens: Some(5),
            total_tokens: Some(15),
            cache_read_input_tokens: Some(2),
            cache_creation_input_tokens: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop_sequences: Vec::new(),
            duration_ms: 250,
            started_at_ms: 0,
            ended_at_ms: 0,
        }
    }

    #[test]
    fn records_inference_and_tool_prometheus_metrics() {
        let handle = install_recorder();
        record_inference(&sample_inference());
        record_tool(&ToolSpan {
            context: crate::metrics::SpanContext::default(),
            step_index: Some(0),
            name: "search".to_string(),
            operation: "execute_tool".to_string(),
            call_id: "call-1".to_string(),
            tool_type: "function".to_string(),
            call_arguments: None,
            call_result: None,
            error_type: None,
            duration_ms: 125,
            started_at_ms: 0,
            ended_at_ms: 0,
        });

        let output = handle.render();
        assert!(output.contains("awaken_inference_requests_total"));
        assert!(output.contains("awaken_inference_duration_seconds"));
        assert!(output.contains("awaken_inference_tokens_total"));
        assert!(output.contains("awaken_tool_calls_total"));
        assert!(output.contains("awaken_tool_duration_seconds"));
    }

    #[test]
    fn prometheus_sink_routes_events_through_recorder() {
        let handle = install_recorder();
        let sink = PrometheusSink::new();

        sink.record(&MetricsEvent::Inference(sample_inference()));
        sink.record(&MetricsEvent::Tool(ToolSpan {
            context: crate::metrics::SpanContext::default(),
            step_index: Some(0),
            name: "sink-test-tool".to_string(),
            operation: "execute_tool".to_string(),
            call_id: "call-sink".to_string(),
            tool_type: "function".to_string(),
            call_arguments: None,
            call_result: None,
            error_type: None,
            duration_ms: 7,
            started_at_ms: 0,
            ended_at_ms: 0,
        }));
        sink.on_run_end(&AgentMetrics {
            session_duration_ms: 1234,
            ..Default::default()
        });

        let output = handle.render();
        assert!(output.contains("sink-test-tool"));
        assert!(output.contains("awaken_agent_session_duration_seconds"));
    }

    #[test]
    fn prometheus_sink_is_copy_clone_send_sync() {
        fn assert_send_sync<T: Send + Sync + Copy + Clone>() {}
        assert_send_sync::<PrometheusSink>();
        let a = PrometheusSink::new();
        let b = a;
        let _ = (a, b);
    }
}
