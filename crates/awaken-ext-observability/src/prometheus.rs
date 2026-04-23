use metrics::{counter, histogram};

use crate::metrics::{
    AgentMetrics, DelegationSpan, GenAISpan, HandoffSpan, SuspensionSpan, ToolSpan,
};

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
            error_type: None,
            duration_ms: 125,
        });

        let output = handle.render();
        assert!(output.contains("awaken_inference_requests_total"));
        assert!(output.contains("awaken_inference_duration_seconds"));
        assert!(output.contains("awaken_inference_tokens_total"));
        assert!(output.contains("awaken_tool_calls_total"));
        assert!(output.contains("awaken_tool_duration_seconds"));
    }
}
