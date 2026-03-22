use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use awaken_contract::StateError;
use awaken_contract::contract::inference::TokenUsage;
use awaken_contract::contract::tool::ToolStatus;
use awaken_contract::model::Phase;
use awaken_runtime::{
    PhaseContext, PhaseHook, Plugin, PluginDescriptor, PluginRegistrar, StateCommand,
};

use super::metrics::{AgentMetrics, GenAISpan, ToolSpan};
use super::sink::MetricsSink;

fn lock_unpoison<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

pub(crate) fn extract_token_counts(
    usage: Option<&TokenUsage>,
) -> (Option<i32>, Option<i32>, Option<i32>, Option<i32>) {
    match usage {
        Some(u) => (
            u.prompt_tokens,
            u.completion_tokens,
            u.total_tokens,
            u.thinking_tokens,
        ),
        None => (None, None, None, None),
    }
}

pub(crate) fn extract_cache_tokens(usage: Option<&TokenUsage>) -> (Option<i32>, Option<i32>) {
    match usage {
        Some(u) => (u.cache_read_tokens, u.cache_creation_tokens),
        None => (None, None),
    }
}

/// Shared mutable state between the plugin and its phase hooks.
pub(crate) struct Inner {
    pub(crate) sink: Arc<dyn MetricsSink>,
    pub(crate) run_start: Mutex<Option<Instant>>,
    pub(crate) metrics: Mutex<AgentMetrics>,
    pub(crate) inference_start: Mutex<Option<Instant>>,
    pub(crate) tool_start: Mutex<HashMap<String, Instant>>,
    pub(crate) model: Mutex<String>,
    pub(crate) provider: Mutex<String>,
    pub(crate) operation: String,
    pub(crate) temperature: Mutex<Option<f64>>,
    pub(crate) top_p: Mutex<Option<f64>>,
    pub(crate) max_tokens: Mutex<Option<u32>>,
    pub(crate) stop_sequences: Mutex<Vec<String>>,
    pub(crate) inference_tracing_span: Mutex<Option<tracing::Span>>,
    pub(crate) tool_tracing_span: Mutex<HashMap<String, tracing::Span>>,
}

/// Plugin that captures LLM and tool telemetry aligned with OpenTelemetry GenAI conventions.
pub struct ObservabilityPlugin {
    pub(crate) inner: Arc<Inner>,
}

impl ObservabilityPlugin {
    pub fn new(sink: impl MetricsSink + 'static) -> Self {
        Self {
            inner: Arc::new(Inner {
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
            }),
        }
    }

    #[must_use]
    pub fn with_model(self, model: impl Into<String>) -> Self {
        *lock_unpoison(&self.inner.model) = model.into();
        self
    }

    #[must_use]
    pub fn with_provider(self, provider: impl Into<String>) -> Self {
        *lock_unpoison(&self.inner.provider) = provider.into();
        self
    }

    #[must_use]
    pub fn with_temperature(self, temperature: f64) -> Self {
        *lock_unpoison(&self.inner.temperature) = Some(temperature);
        self
    }

    #[must_use]
    pub fn with_top_p(self, top_p: f64) -> Self {
        *lock_unpoison(&self.inner.top_p) = Some(top_p);
        self
    }

    #[must_use]
    pub fn with_max_tokens(self, max_tokens: u32) -> Self {
        *lock_unpoison(&self.inner.max_tokens) = Some(max_tokens);
        self
    }

    #[must_use]
    pub fn with_stop_sequences(self, seqs: Vec<String>) -> Self {
        *lock_unpoison(&self.inner.stop_sequences) = seqs;
        self
    }
}

impl Plugin for ObservabilityPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "observability",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        let id = "observability";
        let s = Arc::clone(&self.inner);
        registrar.register_phase_hook(id, Phase::RunStart, RunStartHook(Arc::clone(&s)))?;
        registrar.register_phase_hook(
            id,
            Phase::BeforeInference,
            BeforeInferenceHook(Arc::clone(&s)),
        )?;
        registrar.register_phase_hook(
            id,
            Phase::AfterInference,
            AfterInferenceHook(Arc::clone(&s)),
        )?;
        registrar.register_phase_hook(
            id,
            Phase::BeforeToolExecute,
            BeforeToolExecuteHook(Arc::clone(&s)),
        )?;
        registrar.register_phase_hook(
            id,
            Phase::AfterToolExecute,
            AfterToolExecuteHook(Arc::clone(&s)),
        )?;
        registrar.register_phase_hook(id, Phase::RunEnd, RunEndHook(Arc::clone(&s)))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Phase hook implementations
// ---------------------------------------------------------------------------

pub(crate) struct RunStartHook(pub(crate) Arc<Inner>);

#[async_trait]
impl PhaseHook for RunStartHook {
    async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        *lock_unpoison(&self.0.run_start) = Some(Instant::now());
        Ok(StateCommand::new())
    }
}

pub(crate) struct BeforeInferenceHook(pub(crate) Arc<Inner>);

#[async_trait]
impl PhaseHook for BeforeInferenceHook {
    async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let s = &self.0;

        // Close any abandoned inference tracing span from a retried attempt.
        if let Some(previous_span) = lock_unpoison(&s.inference_tracing_span).take() {
            let message = "A previous inference attempt was retried before completion.";
            previous_span.record("error.type", "inference_retry_interrupted");
            previous_span.record("error.message", message);
            previous_span.record("otel.status_code", "ERROR");
            previous_span.record("otel.status_description", message);
            drop(previous_span);
        }

        *lock_unpoison(&s.inference_start) = Some(Instant::now());

        let model = lock_unpoison(&s.model).clone();
        let provider = lock_unpoison(&s.provider).clone();
        let span_name = format!("{} {}", s.operation, model);
        let span = tracing::info_span!("gen_ai",
            "otel.name" = %span_name,
            "otel.kind" = "client",
            "otel.status_code" = tracing::field::Empty,
            "otel.status_description" = tracing::field::Empty,
            "gen_ai.provider.name" = %provider,
            "gen_ai.operation.name" = %s.operation,
            "gen_ai.request.model" = %model,
            "gen_ai.request.temperature" = tracing::field::Empty,
            "gen_ai.request.top_p" = tracing::field::Empty,
            "gen_ai.request.max_tokens" = tracing::field::Empty,
            "gen_ai.request.stop_sequences" = tracing::field::Empty,
            "gen_ai.response.model" = tracing::field::Empty,
            "gen_ai.response.id" = tracing::field::Empty,
            "gen_ai.usage.thinking_tokens" = tracing::field::Empty,
            "gen_ai.usage.input_tokens" = tracing::field::Empty,
            "gen_ai.usage.output_tokens" = tracing::field::Empty,
            "gen_ai.response.finish_reasons" = tracing::field::Empty,
            "gen_ai.usage.cache_read.input_tokens" = tracing::field::Empty,
            "gen_ai.usage.cache_creation.input_tokens" = tracing::field::Empty,
            "error.type" = tracing::field::Empty,
            "error.message" = tracing::field::Empty,
            "gen_ai.error.class" = tracing::field::Empty,
        );

        if let Some(t) = *lock_unpoison(&s.temperature) {
            span.record("gen_ai.request.temperature", t);
        }
        if let Some(t) = *lock_unpoison(&s.top_p) {
            span.record("gen_ai.request.top_p", t);
        }
        if let Some(t) = *lock_unpoison(&s.max_tokens) {
            span.record("gen_ai.request.max_tokens", t as i64);
        }
        {
            let seqs = lock_unpoison(&s.stop_sequences);
            if !seqs.is_empty() {
                span.record(
                    "gen_ai.request.stop_sequences",
                    format!("{:?}", *seqs).as_str(),
                );
            }
        }
        *lock_unpoison(&s.inference_tracing_span) = Some(span);

        Ok(StateCommand::new())
    }
}

pub(crate) struct AfterInferenceHook(pub(crate) Arc<Inner>);

#[async_trait]
impl PhaseHook for AfterInferenceHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let s = &self.0;

        let duration_ms = lock_unpoison(&s.inference_start)
            .take()
            .map(|start| start.elapsed().as_millis() as u64)
            .unwrap_or(0);

        // Extract usage and error from the LLM response.
        let (usage, error) = match &ctx.llm_response {
            Some(resp) => match &resp.outcome {
                Ok(result) => (result.usage.as_ref(), None),
                Err(err) => (None, Some(err)),
            },
            None => (None, None),
        };

        let (input_tokens, output_tokens, total_tokens, thinking_tokens) =
            extract_token_counts(usage);
        let (cache_read_input_tokens, cache_creation_input_tokens) = extract_cache_tokens(usage);

        let model = lock_unpoison(&s.model).clone();
        let provider = lock_unpoison(&s.provider).clone();
        let span = GenAISpan {
            model,
            provider,
            operation: s.operation.clone(),
            response_model: None,
            response_id: None,
            finish_reasons: Vec::new(),
            error_type: error.map(|e| e.error_type.clone()),
            error_class: error.and_then(|e| e.error_class.clone()),
            input_tokens,
            output_tokens,
            total_tokens,
            thinking_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            temperature: *lock_unpoison(&s.temperature),
            top_p: *lock_unpoison(&s.top_p),
            max_tokens: *lock_unpoison(&s.max_tokens),
            stop_sequences: lock_unpoison(&s.stop_sequences).clone(),
            duration_ms,
        };

        // Record tracing span attributes.
        if let Some(tracing_span) = lock_unpoison(&s.inference_tracing_span).take() {
            if let Some(v) = span.thinking_tokens {
                tracing_span.record("gen_ai.usage.thinking_tokens", v);
            }
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
            if let Some(err) = error {
                tracing_span.record("error.type", err.error_type.as_str());
                tracing_span.record("error.message", err.message.as_str());
                tracing_span.record("otel.status_code", "ERROR");
                tracing_span.record("otel.status_description", err.message.as_str());
                if let Some(ref class) = err.error_class {
                    tracing_span.record("gen_ai.error.class", class.as_str());
                }
            }
            drop(tracing_span);
        }

        s.sink.on_inference(&span);
        lock_unpoison(&s.metrics).inferences.push(span);

        Ok(StateCommand::new())
    }
}

pub(crate) struct BeforeToolExecuteHook(pub(crate) Arc<Inner>);

#[async_trait]
impl PhaseHook for BeforeToolExecuteHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let s = &self.0;

        let tool_name = ctx.tool_name.as_deref().unwrap_or_default().to_string();
        let call_id = ctx.tool_call_id.as_deref().unwrap_or_default().to_string();

        if !call_id.is_empty() {
            lock_unpoison(&s.tool_start).insert(call_id.clone(), Instant::now());
        }

        let provider = lock_unpoison(&s.provider).clone();
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
            lock_unpoison(&s.tool_tracing_span).insert(call_id, span);
        }

        Ok(StateCommand::new())
    }
}

pub(crate) struct AfterToolExecuteHook(pub(crate) Arc<Inner>);

#[async_trait]
impl PhaseHook for AfterToolExecuteHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let s = &self.0;

        let call_id = ctx.tool_call_id.as_deref().unwrap_or_default().to_string();
        let duration_ms = lock_unpoison(&s.tool_start)
            .remove(&call_id)
            .map(|start| start.elapsed().as_millis() as u64)
            .unwrap_or(0);

        let Some(result) = ctx.tool_result.as_ref() else {
            return Ok(StateCommand::new());
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
            call_id: call_id.clone(),
            tool_type: "function".to_string(),
            error_type,
            duration_ms,
        };

        let tracing_span = lock_unpoison(&s.tool_tracing_span).remove(&call_id);
        if let Some(tracing_span) = tracing_span {
            if let (Some(v), Some(msg)) = (&span.error_type, &error_message) {
                tracing_span.record("error.type", v.as_str());
                tracing_span.record("error.message", msg.as_str());
                tracing_span.record("otel.status_code", "ERROR");
                tracing_span.record("otel.status_description", msg.as_str());
            }
            drop(tracing_span);
        }

        s.sink.on_tool(&span);
        lock_unpoison(&s.metrics).tools.push(span);

        Ok(StateCommand::new())
    }
}

pub(crate) struct RunEndHook(pub(crate) Arc<Inner>);

#[async_trait]
impl PhaseHook for RunEndHook {
    async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let s = &self.0;

        let session_duration_ms = lock_unpoison(&s.run_start)
            .take()
            .map(|start| start.elapsed().as_millis() as u64)
            .unwrap_or(0);

        lock_unpoison(&s.inference_tracing_span).take();
        lock_unpoison(&s.tool_tracing_span).clear();
        lock_unpoison(&s.tool_start).clear();

        let mut metrics = lock_unpoison(&s.metrics).clone();
        metrics.session_duration_ms = session_duration_ms;
        s.sink.on_run_end(&metrics);

        Ok(StateCommand::new())
    }
}
