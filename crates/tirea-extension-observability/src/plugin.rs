use super::metrics::AgentMetrics;
use super::sink::MetricsSink;
use super::spans::{GenAISpan, ToolSpan};
use async_trait::async_trait;
use genai::chat::ChatOptions;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tirea_contract::runtime::behavior::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::phase::{
    ActionSet, AfterInferenceAction, AfterToolExecuteAction, BeforeInferenceAction,
    BeforeToolExecuteAction, LifecycleAction,
};
use tirea_contract::runtime::tool_call::ToolStatus;
use tirea_contract::TokenUsage;

pub(super) fn lock_unpoison<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

pub(super) fn extract_token_counts(
    usage: Option<&TokenUsage>,
) -> (Option<i32>, Option<i32>, Option<i32>) {
    match usage {
        Some(u) => (u.prompt_tokens, u.completion_tokens, u.total_tokens),
        None => (None, None, None),
    }
}

pub(super) fn extract_cache_tokens(usage: Option<&TokenUsage>) -> (Option<i32>, Option<i32>) {
    match usage {
        Some(u) => (u.cache_read_tokens, u.cache_creation_tokens),
        None => (None, None),
    }
}

/// Plugin that captures LLM and tool telemetry.
pub struct LLMMetryPlugin {
    sink: Arc<dyn MetricsSink>,
    run_start: Mutex<Option<Instant>>,
    metrics: Mutex<AgentMetrics>,
    inference_start: Mutex<Option<Instant>>,
    tool_start: Mutex<HashMap<String, Instant>>,
    model: Mutex<String>,
    provider: Mutex<String>,
    operation: String,
    temperature: Mutex<Option<f64>>,
    top_p: Mutex<Option<f64>>,
    max_tokens: Mutex<Option<u32>>,
    stop_sequences: Mutex<Vec<String>>,
    inference_tracing_span: Mutex<Option<tracing::Span>>,
    tool_tracing_span: Mutex<HashMap<String, tracing::Span>>,
}

impl LLMMetryPlugin {
    pub fn new(sink: impl MetricsSink + 'static) -> Self {
        Self {
            sink: Arc::new(sink),
            run_start: Mutex::new(None),
            metrics: Mutex::new(AgentMetrics::default()),
            inference_start: Mutex::new(None),
            tool_start: Mutex::new(HashMap::new()),
            model: Mutex::new(String::new()),
            provider: Mutex::new(String::new()),
            operation: "chat".to_string(),
            temperature: Mutex::new(None),
            top_p: Mutex::new(None),
            max_tokens: Mutex::new(None),
            stop_sequences: Mutex::new(Vec::new()),
            inference_tracing_span: Mutex::new(None),
            tool_tracing_span: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_model(self, model: impl Into<String>) -> Self {
        *lock_unpoison(&self.model) = model.into();
        self
    }

    pub fn with_provider(self, provider: impl Into<String>) -> Self {
        *lock_unpoison(&self.provider) = provider.into();
        self
    }

    pub fn with_chat_options(self, opts: &ChatOptions) -> Self {
        *lock_unpoison(&self.temperature) = opts.temperature;
        *lock_unpoison(&self.top_p) = opts.top_p;
        *lock_unpoison(&self.max_tokens) = opts.max_tokens;
        let seqs = &opts.stop_sequences;
        if !seqs.is_empty() {
            *lock_unpoison(&self.stop_sequences) = seqs.clone();
        }
        self
    }
}

#[async_trait]
impl AgentBehavior for LLMMetryPlugin {
    fn id(&self) -> &str {
        "llmmetry"
    }

    async fn run_start(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        *lock_unpoison(&self.run_start) = Some(Instant::now());
        ActionSet::empty()
    }

    async fn before_inference(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        *lock_unpoison(&self.inference_start) = Some(Instant::now());
        let model = lock_unpoison(&self.model).clone();
        let provider = lock_unpoison(&self.provider).clone();
        let span_name = format!("{} {}", self.operation, model);
        let span = tracing::info_span!("gen_ai",
            "otel.name" = %span_name,
            "otel.kind" = "client",
            "otel.status_code" = tracing::field::Empty,
            "otel.status_description" = tracing::field::Empty,
            "gen_ai.provider.name" = %provider,
            "gen_ai.operation.name" = %self.operation,
            "gen_ai.request.model" = %model,
            "gen_ai.request.temperature" = tracing::field::Empty,
            "gen_ai.request.top_p" = tracing::field::Empty,
            "gen_ai.request.max_tokens" = tracing::field::Empty,
            "gen_ai.request.stop_sequences" = tracing::field::Empty,
            "gen_ai.response.model" = tracing::field::Empty,
            "gen_ai.response.id" = tracing::field::Empty,
            "gen_ai.usage.input_tokens" = tracing::field::Empty,
            "gen_ai.usage.output_tokens" = tracing::field::Empty,
            "gen_ai.response.finish_reasons" = tracing::field::Empty,
            "gen_ai.usage.cache_read.input_tokens" = tracing::field::Empty,
            "gen_ai.usage.cache_creation.input_tokens" = tracing::field::Empty,
            "error.type" = tracing::field::Empty,
            "error.message" = tracing::field::Empty,
        );
        if let Some(t) = *lock_unpoison(&self.temperature) {
            span.record("gen_ai.request.temperature", t);
        }
        if let Some(t) = *lock_unpoison(&self.top_p) {
            span.record("gen_ai.request.top_p", t);
        }
        if let Some(t) = *lock_unpoison(&self.max_tokens) {
            span.record("gen_ai.request.max_tokens", t as i64);
        }
        {
            let seqs = lock_unpoison(&self.stop_sequences);
            if !seqs.is_empty() {
                span.record(
                    "gen_ai.request.stop_sequences",
                    format!("{:?}", *seqs).as_str(),
                );
            }
        }
        *lock_unpoison(&self.inference_tracing_span) = Some(span);
        ActionSet::empty()
    }

    async fn after_inference(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<AfterInferenceAction> {
        let duration_ms = self
            .inference_start
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .map(|s| s.elapsed().as_millis() as u64)
            .unwrap_or(0);

        let usage = ctx.response().and_then(|r| r.usage.as_ref());
        let (input_tokens, output_tokens, total_tokens) = extract_token_counts(usage);
        let (cache_read_input_tokens, cache_creation_input_tokens) = extract_cache_tokens(usage);
        let error = ctx.inference_error().cloned();

        let model = lock_unpoison(&self.model).clone();
        let provider = lock_unpoison(&self.provider).clone();
        let span = GenAISpan {
            model,
            provider,
            operation: self.operation.clone(),
            response_model: None,
            response_id: None,
            finish_reasons: Vec::new(),
            error_type: error.as_ref().map(|e| e.error_type.clone()),
            input_tokens,
            output_tokens,
            total_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            temperature: *lock_unpoison(&self.temperature),
            top_p: *lock_unpoison(&self.top_p),
            max_tokens: *lock_unpoison(&self.max_tokens),
            stop_sequences: lock_unpoison(&self.stop_sequences).clone(),
            duration_ms,
        };

        if let Some(tracing_span) = lock_unpoison(&self.inference_tracing_span).take() {
            if let Some(v) = span.input_tokens {
                tracing_span.record("gen_ai.usage.input_tokens", v);
            }
            if let Some(v) = span.output_tokens {
                tracing_span.record("gen_ai.usage.output_tokens", v);
            }
            if let Some(v) = span.cache_read_input_tokens {
                tracing_span.record("gen_ai.usage.cache_read.input_tokens", v);
            }
            if let Some(v) = span.cache_creation_input_tokens {
                tracing_span.record("gen_ai.usage.cache_creation.input_tokens", v);
            }
            if !span.finish_reasons.is_empty() {
                tracing_span.record(
                    "gen_ai.response.finish_reasons",
                    format!("{:?}", span.finish_reasons).as_str(),
                );
            }
            if let Some(ref v) = span.response_model {
                tracing_span.record("gen_ai.response.model", v.as_str());
            }
            if let Some(ref v) = span.response_id {
                tracing_span.record("gen_ai.response.id", v.as_str());
            }
            if let Some(ref err) = error {
                tracing_span.record("error.type", err.error_type.as_str());
                tracing_span.record("error.message", err.message.as_str());
                tracing_span.record("otel.status_code", "ERROR");
                tracing_span.record("otel.status_description", err.message.as_str());
            }
            drop(tracing_span);
        }

        self.sink.on_inference(&span);
        lock_unpoison(&self.metrics).inferences.push(span);
        ActionSet::empty()
    }

    async fn before_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeToolExecuteAction> {
        let tool_name = ctx.tool_name().unwrap_or_default().to_string();
        let call_id = ctx.tool_call_id().unwrap_or_default().to_string();
        if !call_id.is_empty() {
            self.tool_start
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(call_id.clone(), Instant::now());
        }
        let provider = lock_unpoison(&self.provider).clone();
        let span_name = format!("execute_tool {}", tool_name);
        let span = tracing::info_span!("gen_ai",
            "otel.name" = %span_name,
            "otel.kind" = "internal",
            "otel.status_code" = tracing::field::Empty,
            "otel.status_description" = tracing::field::Empty,
            "gen_ai.provider.name" = %provider,
            "gen_ai.operation.name" = "execute_tool",
            "gen_ai.tool.name" = %tool_name,
            "gen_ai.tool.call.id" = %call_id,
            "gen_ai.tool.type" = "function",
            "error.type" = tracing::field::Empty,
            "error.message" = tracing::field::Empty,
        );
        if !call_id.is_empty() {
            lock_unpoison(&self.tool_tracing_span).insert(call_id, span);
        }
        ActionSet::empty()
    }

    async fn after_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<AfterToolExecuteAction> {
        let call_id_for_span = ctx.tool_call_id().unwrap_or_default().to_string();
        let duration_ms = self
            .tool_start
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&call_id_for_span)
            .map(|s| s.elapsed().as_millis() as u64)
            .unwrap_or(0);

        let Some(result) = ctx.tool_result() else {
            return ActionSet::empty();
        };
        let error_type = if result.status == ToolStatus::Error {
            Some("tool_error".to_string())
        } else {
            None
        };
        let error_message = result.message.clone().filter(|_| error_type.is_some());
        let span = ToolSpan {
            name: result.tool_name.clone(),
            operation: "execute_tool".to_string(),
            call_id: call_id_for_span.clone(),
            tool_type: "function".to_string(),
            error_type,
            duration_ms,
        };

        let tracing_span = self
            .tool_tracing_span
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&call_id_for_span);
        if let Some(tracing_span) = tracing_span {
            if let (Some(ref v), Some(ref msg)) = (&span.error_type, &error_message) {
                tracing_span.record("error.type", v.as_str());
                tracing_span.record("error.message", msg.as_str());
                tracing_span.record("otel.status_code", "ERROR");
                tracing_span.record("otel.status_description", msg.as_str());
            }
            drop(tracing_span);
        }

        self.sink.on_tool(&span);
        lock_unpoison(&self.metrics).tools.push(span);
        ActionSet::empty()
    }

    async fn run_end(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        let session_duration_ms = self
            .run_start
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .map(|s| s.elapsed().as_millis() as u64)
            .unwrap_or(0);

        lock_unpoison(&self.inference_tracing_span).take();
        lock_unpoison(&self.tool_tracing_span).clear();
        lock_unpoison(&self.tool_start).clear();

        let mut metrics = lock_unpoison(&self.metrics).clone();
        metrics.session_duration_ms = session_duration_ms;
        self.sink.on_run_end(&metrics);
        ActionSet::empty()
    }
}
